//! Command-monitor registry and output delivery.
//!
//! A monitor runs a shell command as a long-lived background process and
//! delivers each output line (stdout or stderr) to the session as a
//! notification, waking an idle session at the next turn boundary. It lives
//! inside `unified_exec` so the delivery loop can read the process's
//! `pub(super)` output stream.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio::time::sleep_until;
use tokio_util::task::AbortOnDropHandle;

use super::async_watcher::TRAILING_OUTPUT_GRACE;
use super::process::UnifiedExecProcess;
use crate::context::MonitorNotification;
use crate::session::session::Session;

/// Lines emitted within this window coalesce into one notification.
const BATCH_WINDOW: Duration = Duration::from_millis(200);

/// A monitor that emits more than this many lines is auto-stopped so a runaway
/// command cannot wake the agent without bound.
const FLOOD_MAX_LINES: usize = 5000;

/// A run of bytes with no terminating newline is truncated at this length, so a
/// watcher that streams without newlines (a binary blob, `cat /dev/urandom`)
/// cannot grow the delivery buffer, or one notification, without bound. Each
/// truncation still counts toward the line ceiling, so an endless newline-free
/// stream trips the flood guard instead of exhausting host memory.
const MAX_LINE_BYTES: usize = 700;
const MAX_BATCH_BYTES: usize = 2400;

/// A snapshot of one active monitor, returned by [`MonitorManager::list`].
pub(crate) struct MonitorInfo {
    pub id: String,
    pub description: String,
    pub command: String,
}

struct MonitorEntry {
    description: String,
    command: String,
    process_id: i32,
    _task: AbortOnDropHandle<()>,
}

/// Per-session registry of active monitors. Holds the delivery tasks; the
/// underlying processes live in the shared [`UnifiedExecProcessManager`] store
/// and are reaped by its `terminate_all_processes` at session shutdown.
#[derive(Default)]
pub(crate) struct MonitorManager {
    monitors: Mutex<HashMap<String, MonitorEntry>>,
}

impl MonitorManager {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) async fn insert(
        &self,
        id: String,
        process_id: i32,
        description: String,
        command: String,
        task: JoinHandle<()>,
    ) {
        self.monitors.lock().await.insert(
            id,
            MonitorEntry {
                description,
                command,
                process_id,
                _task: AbortOnDropHandle::new(task),
            },
        );
    }

    /// Removes a monitor, returning its process id so the caller can terminate
    /// the underlying process. Dropping the entry aborts its delivery task.
    pub(crate) async fn remove(&self, id: &str) -> Option<i32> {
        self.monitors
            .lock()
            .await
            .remove(id)
            .map(|entry| entry.process_id)
    }

    /// Removes a monitor entry on behalf of its OWN delivery task as that task
    /// exits. `remove` aborts the entry's task by dropping its abort-on-drop
    /// handle, which is correct for an external `action=stop` but would cancel
    /// this task's own final exit-notice delivery; defusing the handle lets the
    /// loop prune itself first and still announce the exit.
    pub(crate) async fn deregister_self(&self, id: &str) {
        if let Some(entry) = self.monitors.lock().await.remove(id) {
            let MonitorEntry { _task, .. } = entry;
            drop(_task.detach());
        }
    }

    pub(crate) async fn list(&self) -> Vec<MonitorInfo> {
        self.monitors
            .lock()
            .await
            .iter()
            .map(|(id, entry)| MonitorInfo {
                id: id.clone(),
                description: entry.description.clone(),
                command: entry.command.clone(),
            })
            .collect()
    }

    /// Aborts every monitor's delivery task. The processes themselves are reaped
    /// separately by the unified-exec manager at shutdown.
    pub(crate) async fn abort_all(&self) {
        self.monitors.lock().await.clear();
    }
}

/// Retain even an immediately exited process, and wait for registry insertion.
pub(crate) fn spawn_delivery(
    process: Arc<UnifiedExecProcess>,
    process_id: i32,
    id: String,
    session: Weak<Session>,
    description: String,
    seed: Vec<u8>,
    ready: tokio::sync::oneshot::Receiver<()>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        if ready.await.is_ok() {
            delivery_loop(process, process_id, id, session, description, seed).await;
        }
    })
}

async fn delivery_loop(
    process: Arc<UnifiedExecProcess>,
    process_id: i32,
    id: String,
    session: Weak<Session>,
    description: String,
    seed: Vec<u8>,
) {
    // Continue consuming the initial yield's buffer. A late broadcast
    // subscription would lose output produced between yield and registration.
    let output = process.output_handles();
    let mut buf = Vec::new();
    let mut pending = Vec::new();
    let mut flood_count = 0;
    let mut flush_at = None;
    let mut closing_at = None;
    let mut chunk = seed;
    loop {
        let notified = output.output_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if extend_lines(
            &mut buf,
            &chunk,
            &mut pending,
            &mut flood_count,
            &mut flush_at,
        ) {
            stop_for_flood(&session, &description, &id, process_id, &mut pending).await;
            return;
        }
        if pending.iter().map(String::len).sum::<usize>() >= MAX_BATCH_BYTES {
            flush(&session, &description, &mut pending).await;
            flush_at = None;
        }
        chunk = {
            let mut buffer = output.output_buffer.lock().await;
            std::mem::take(&mut *buffer).to_bytes_with_omission_marker()
        };
        if !chunk.is_empty() {
            continue;
        }
        if output.cancellation_token.is_cancelled() {
            if output
                .output_closed
                .load(std::sync::atomic::Ordering::Acquire)
            {
                break;
            }
            closing_at.get_or_insert_with(|| Instant::now() + TRAILING_OUTPUT_GRACE);
        }
        tokio::select! {
            _ = &mut notified => {}
            () = wait_until(flush_at) => {
                flush(&session, &description, &mut pending).await;
                flush_at = None;
            }
            () = output.cancellation_token.cancelled(), if closing_at.is_none() => {
                closing_at = Some(Instant::now() + TRAILING_OUTPUT_GRACE);
            }
            () = wait_until(closing_at) => break,
        }
    }
    if !buf.is_empty() {
        pending.push(String::from_utf8_lossy(&buf).trim_end().to_string());
    }
    pending.insert(0, exit_notice(&process));
    if let Some(session) = session.upgrade() {
        session.services.monitor_manager.deregister_self(&id).await;
    }
    flush(&session, &description, &mut pending).await;
}

/// Auto-stops a watcher that hit the flood ceiling: flush what we have, tell the
/// agent, terminate the process, and prune the registry. Used by both the seed
/// and the receive paths.
async fn stop_for_flood(
    session: &Weak<Session>,
    description: &str,
    id: &str,
    process_id: i32,
    pending: &mut Vec<String>,
) {
    flush(session, description, pending).await;
    // Terminate and deregister before announcing, so the agent wakes to a
    // consistent state.
    if let Some(session) = session.upgrade() {
        session
            .services
            .unified_exec_manager
            .terminate_process(process_id)
            .await;
        session.services.monitor_manager.deregister_self(id).await;
    }
    deliver(
        session,
        description,
        format!(
            "auto-stopped after {FLOOD_MAX_LINES} lines (flood guard); \
             restart with a tighter filter"
        ),
    )
    .await;
}

/// Resolves at `deadline` when set, otherwise never.
async fn wait_until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Splits `chunk` into complete lines, appending each to `pending` and arming
/// the batch timer. Returns `true` if the flood ceiling was reached mid-chunk,
/// so the caller can auto-stop before a single huge chunk blows past the bound.
fn extend_lines(
    buf: &mut Vec<u8>,
    chunk: &[u8],
    pending: &mut Vec<String>,
    flood_count: &mut usize,
    flush_at: &mut Option<Instant>,
) -> bool {
    buf.extend_from_slice(chunk);
    loop {
        let line: Vec<u8> =
            if let Some(nl) = buf.iter().take(MAX_LINE_BYTES).position(|&b| b == b'\n') {
                buf.drain(..=nl).collect()
            } else if buf.len() > MAX_LINE_BYTES {
                // No newline yet, but the buffer is already pathologically long.
                // Emit a truncated prefix so a newline-free stream cannot grow the
                // buffer without bound; the remainder keeps draining on later passes,
                // and each truncation counts toward the flood ceiling below.
                let mut line = b"(line truncated) ".to_vec();
                line.extend(buf.drain(..MAX_LINE_BYTES));
                line
            } else {
                break;
            };
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end();
        if text.is_empty() {
            continue;
        }
        pending.push(text.to_string());
        *flood_count += 1;
        if flush_at.is_none() {
            *flush_at = Some(Instant::now() + BATCH_WINDOW);
        }
        if *flood_count >= FLOOD_MAX_LINES {
            return true;
        }
    }
    false
}

fn exit_notice(process: &UnifiedExecProcess) -> String {
    if let Some(message) = process.failure_message() {
        format!("watcher ended: {message}")
    } else {
        match process.exit_code() {
            Some(code) => format!("watcher exited (code {code})"),
            None => "watcher exited".to_string(),
        }
    }
}

/// Delivers the accumulated batch as one notification and clears it.
async fn flush(session: &Weak<Session>, description: &str, pending: &mut Vec<String>) {
    if pending.is_empty() {
        return;
    }
    let text = std::mem::take(pending).join("\n");
    deliver(session, description, text).await;
}

/// Queue bounded, labelled context before waking the shared pending-work
/// scheduler. A running turn drains it at a safe boundary; turn completion
/// checks the same queue, so an event racing the final answer is not lost.
/// Queue overflow and output truncation are reported in the notification.
async fn deliver(session: &Weak<Session>, description: &str, body: String) {
    let Some(session) = session.upgrade() else {
        return;
    };
    session
        .input_queue
        .enqueue_monitor_notification(MonitorNotification::new(description, body))
        .await;
    session.maybe_start_turn_for_pending_work().await;
}

#[cfg(test)]
#[path = "monitor_tests.rs"]
mod tests;
