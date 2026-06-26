use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use codex_protocol::protocol::{MonitorEvent, MonitorEventKind, MonitorWakePolicy};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::app_command::AppCommand;
use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;

const MAX_BATCH_LINES: usize = 100;
const MAX_STDERR_TAIL: usize = 4096;

pub(crate) struct MonitorHandle {
    pub(crate) name: String,
    pub(crate) command_str: String,
    pub(crate) child: Child,
    pub(crate) cancel: CancellationToken,
}

pub(crate) struct MonitorManager {
    pub(crate) monitors: Arc<Mutex<HashMap<String, MonitorHandle>>>,
    pub(crate) sequence_counter: Arc<AtomicU64>,
}

impl MonitorManager {
    pub fn new() -> Self {
        Self {
            monitors: Arc::new(Mutex::new(HashMap::new())),
            sequence_counter: Arc::new(AtomicU64::new(0)),
        }
    }

    pub async fn start(
        &self,
        name: String,
        command: &str,
        event_sender: AppEventSender,
    ) -> Result<(), String> {
        let mut monitors = self.monitors.lock().await;
        if monitors.contains_key(&name) {
            return Err(format!("Monitor '{}' is already running", name));
        }

        let mut child = Command::new("sh")
            .arg("-c")
            .arg(command)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("Failed to spawn monitor: {}", e))?;

        let stdout = child.stdout.take().ok_or("Failed to capture stdout")?;
        // P0-2: Take stderr to drain it
        let stderr = child.stderr.take();
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let monitor_name = name.clone();
        let seq_counter = Arc::clone(&self.sequence_counter);
        let monitors_ref = Arc::clone(&self.monitors);

        // P0-2: Drain stderr in separate task
        if let Some(stderr) = stderr {
            let cancel_stderr = cancel.clone();
            let name_stderr = monitor_name.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 1024];
                let mut stderr = stderr;
                loop {
                    tokio::select! {
                        _ = cancel_stderr.cancelled() => break,
                        r = stderr.read(&mut buf) => {
                            match r {
                                Ok(0) | Err(_) => break,
                                Ok(_) => {}
                            }
                        }
                    }
                }
                let _ = name_stderr;
            });
        }

        tokio::spawn(async move {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            let mut batch_buffer: Vec<String> = Vec::new();
            let batch_window = tokio::time::Duration::from_millis(250);

            loop {
                tokio::select! {
                    _ = cancel_clone.cancelled() => break,
                    result = lines.next_line() => {
                        match result {
                            Ok(Some(line)) => {
                                batch_buffer.push(line);
                                let deadline = tokio::time::Instant::now() + batch_window;
                                while batch_buffer.len() < MAX_BATCH_LINES {
                                    tokio::select! {
                                        _ = cancel_clone.cancelled() => break,
                                        _ = tokio::time::sleep_until(deadline) => break,
                                        result = lines.next_line() => {
                                            match result {
                                                Ok(Some(line)) => batch_buffer.push(line),
                                                _ => break,
                                            }
                                        }
                                    }
                                }
                                if !batch_buffer.is_empty() {
                                    let seq = seq_counter.fetch_add(1, Ordering::Relaxed);
                                    let summary = batch_buffer.join("\n");
                                    batch_buffer.clear();
                                    let event = MonitorEvent {
                                        id: format!("mon-{}-{}", monitor_name, seq),
                                        monitor_name: monitor_name.clone(),
                                        sequence: seq,
                                        kind: MonitorEventKind::OutputBatch,
                                        summary,
                                        wake_policy: MonitorWakePolicy::AttachOrWake,
                                    };
                                    event_sender.send(AppEvent::CodexOp(
                                        AppCommand::MonitorEvent { event },
                                    ));
                                }
                            }
                            Ok(None) => break,
                            Err(e) => {
                                tracing::warn!(monitor = %monitor_name, error = %e, "stdout read error");
                                break;
                            }
                        }
                    }
                }
            }

            if !batch_buffer.is_empty() {
                let seq = seq_counter.fetch_add(1, Ordering::Relaxed);
                let summary = batch_buffer.join("\n");
                let event = MonitorEvent {
                    id: format!("mon-{}-{}-final", monitor_name, seq),
                    monitor_name: monitor_name.clone(),
                    sequence: seq,
                    kind: MonitorEventKind::OutputBatch,
                    summary,
                    wake_policy: MonitorWakePolicy::AttachOrWake,
                };
                event_sender.send(AppEvent::CodexOp(
                    AppCommand::MonitorEvent { event },
                ));
            }

            let mut monitors = monitors_ref.lock().await;
            if let Some(mut handle) = monitors.remove(&monitor_name) {
                let exit_seq = seq_counter.fetch_add(1, Ordering::Relaxed);
                let kind = match handle.child.try_wait() {
                    Ok(Some(status)) => {
                        let code = status.code().unwrap_or(-1);
                        if status.success() {
                            MonitorEventKind::Completed { exit_code: code }
                        } else {
                            MonitorEventKind::Failed {
                                exit_code: code,
                                stderr_tail: None,
                            }
                        }
                    }
                    _ => MonitorEventKind::Cancelled,
                };
                let event = MonitorEvent {
                    id: format!("mon-{}-{}-exit", monitor_name, exit_seq),
                    monitor_name: monitor_name.clone(),
                    sequence: exit_seq,
                    kind,
                    summary: format!("Monitor '{}' has stopped.", monitor_name),
                    wake_policy: MonitorWakePolicy::AttachOrWake,
                };
                event_sender.send(AppEvent::CodexOp(
                    AppCommand::MonitorEvent { event },
                ));
            }
        });

        let command_str = command.to_string();
        monitors.insert(
            name.clone(),
            MonitorHandle {
                name,
                command_str,
                child,
                cancel,
            },
        );
        Ok(())
    }

    pub async fn stop(&self, name: &str) -> Result<(), String> {
        let mut monitors = self.monitors.lock().await;
        if let Some(mut handle) = monitors.remove(name) {
            handle.cancel.cancel();
            let _ = handle.child.kill().await;
            Ok(())
        } else {
            Err(format!("No monitor named '{}' is running", name))
        }
    }

    pub async fn stop_all(&self) {
        let mut monitors = self.monitors.lock().await;
        for (_, mut handle) in monitors.drain() {
            handle.cancel.cancel();
            let _ = handle.child.kill().await;
        }
    }

    pub async fn status(&self) -> Vec<(String, String)> {
        let monitors = self.monitors.lock().await;
        monitors
            .iter()
            .map(|(name, handle)| (name.clone(), handle.command_str.clone()))
            .collect()
    }
}
