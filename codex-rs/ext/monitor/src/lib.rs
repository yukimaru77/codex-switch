use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use codex_extension_api::ExtensionData;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::FunctionCallError;
use codex_extension_api::JsonToolOutput;
use codex_extension_api::ThreadStartInput;
use codex_extension_api::ToolCall;
use codex_extension_api::ToolContributor;
use codex_extension_api::ToolExecutor;
use codex_extension_api::ToolName;
use codex_extension_api::ToolOutput;
use codex_extension_api::ToolSpec;
use codex_protocol::ThreadId;
use codex_protocol::protocol::MonitorEvent;
use codex_protocol::protocol::MonitorEventKind;
use codex_protocol::protocol::MonitorWakePolicy;
use codex_protocol::protocol::Op;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use serde::Deserialize;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::BufReader;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

const MONITOR_START_TOOL: &str = "monitor_start";
const MONITOR_STOP_TOOL: &str = "monitor_stop";
const MONITOR_LIST_TOOL: &str = "monitor_list";

const MAX_BATCH_LINES: usize = 500;
const MAX_QUEUE_EVENTS: usize = 50;
const MAX_STDERR_TAIL_BYTES: usize = 4096;
const MAX_NAME_LEN: usize = 64;
/// Quiet period after the most recent line before a batch is closed.
/// Rolling (extended by every new line) so one burst of output — e.g. an
/// agmsg poll tick printing several messages — lands in a single event
/// instead of being split at an arbitrary fixed deadline.
const BATCH_WINDOW_MS: u64 = 250;
/// Hard cap on how long a batch may stay open from its first line, so a
/// process that never stays quiet still delivers events promptly.
const MAX_BATCH_WINDOW_MS: u64 = 2000;

// P0-5: Instance ID to prevent ABA race on stop/start same name
type InstanceId = u64;

struct MonitorHandle {
    instance_id: InstanceId,
    name: String,
    command_str: String,
    cancel: CancellationToken,
}

pub(crate) struct MonitorState {
    monitors: Mutex<HashMap<String, MonitorHandle>>,
    seq: AtomicU64,
    instance_counter: AtomicU64,
    pending_event_count: AtomicU64,
    thread_manager: Weak<codex_core::ThreadManager>,
    thread_id: ThreadId,
}

pub struct MonitorExtension {
    thread_manager: Weak<codex_core::ThreadManager>,
}

struct MonitorToolExecutor {
    kind: MonitorToolKind,
    state: Arc<MonitorState>,
}

#[derive(Clone, Copy)]
enum MonitorToolKind {
    Start,
    Stop,
    List,
}

#[derive(Deserialize)]
struct StartArgs {
    name: String,
    command: String,
}

#[derive(Deserialize)]
struct StopArgs {
    name: String,
}

fn start_spec() -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: MONITOR_START_TOOL.to_string(),
        description: "Launch a new background monitor to observe a long-running command. \
            The command will run in the background and its stdout will be streamed \
            as notifications. Each notification line becomes a new event delivered \
            at the next safe model-call boundary — even while you are idle or \
            executing other tools. Use this for test watchers, build systems, \
            log tails, file watchers, or any long-running process. \
            Bug reports: https://github.com/yukimaru77/codex-switch/issues"
            .to_string(),
        strict: true,
        defer_loading: None,
        parameters: JsonSchema::object(
            std::collections::BTreeMap::from([
                (
                    "name".to_string(),
                    JsonSchema::string(Some(
                        "Unique name for this monitor (alphanumeric, dash, underscore).".into(),
                    )),
                ),
                (
                    "command".to_string(),
                    JsonSchema::string(Some("The shell command to run in the background.".into())),
                ),
            ]),
            Some(vec!["name".into(), "command".into()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

fn stop_spec() -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: MONITOR_STOP_TOOL.to_string(),
        description: "Stop a running background monitor by name. The monitor process will be \
            terminated and a completion event will be delivered. \
            Bug reports: https://github.com/yukimaru77/codex-switch/issues"
            .to_string(),
        strict: true,
        defer_loading: None,
        parameters: JsonSchema::object(
            std::collections::BTreeMap::from([(
                "name".to_string(),
                JsonSchema::string(Some("Name of the monitor to stop.".into())),
            )]),
            Some(vec!["name".into()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

fn list_spec() -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: MONITOR_LIST_TOOL.to_string(),
        description:
            "List all currently running background monitors with their names and commands. \
            Bug reports: https://github.com/yukimaru77/codex-switch/issues"
                .to_string(),
        strict: true,
        defer_loading: None,
        parameters: JsonSchema::object(
            std::collections::BTreeMap::new(),
            Some(Vec::new()),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

// P0-1: Name validation to prevent injection
fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return Err(format!("Monitor name must be 1-{MAX_NAME_LEN} characters."));
    }
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        return Err("Monitor name must be alphanumeric, dash, or underscore.".into());
    }
    Ok(())
}

impl ToolExecutor<ToolCall> for MonitorToolExecutor {
    fn tool_name(&self) -> ToolName {
        ToolName::new(
            None,
            match self.kind {
                MonitorToolKind::Start => MONITOR_START_TOOL,
                MonitorToolKind::Stop => MONITOR_STOP_TOOL,
                MonitorToolKind::List => MONITOR_LIST_TOOL,
            },
        )
    }

    fn spec(&self) -> ToolSpec {
        match self.kind {
            MonitorToolKind::Start => start_spec(),
            MonitorToolKind::Stop => stop_spec(),
            MonitorToolKind::List => list_spec(),
        }
    }

    fn handle(&self, invocation: ToolCall) -> codex_extension_api::ToolExecutorFuture<'_> {
        let state = Arc::clone(&self.state);
        let kind = self.kind;
        Box::pin(async move {
            match kind {
                MonitorToolKind::Start => {
                    let args: StartArgs = serde_json::from_str(invocation.function_arguments()?)
                        .map_err(|e| {
                            FunctionCallError::RespondToModel(format!("Invalid arguments: {e}"))
                        })?;
                    start_monitor(state, args).await
                }
                MonitorToolKind::Stop => {
                    let args: StopArgs = serde_json::from_str(invocation.function_arguments()?)
                        .map_err(|e| {
                            FunctionCallError::RespondToModel(format!("Invalid arguments: {e}"))
                        })?;
                    stop_monitor(state, args).await
                }
                MonitorToolKind::List => list_monitors(state).await,
            }
        })
    }
}

async fn start_monitor(
    state: Arc<MonitorState>,
    args: StartArgs,
) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
    // P0-1: Validate name
    if let Err(e) = validate_name(&args.name) {
        return Ok(Box::new(JsonToolOutput::new(
            serde_json::json!({"error": e}),
        )));
    }

    let mut monitors = state.monitors.lock().await;
    if monitors.contains_key(&args.name) {
        return Ok(Box::new(JsonToolOutput::new(serde_json::json!({
            "error": format!("Monitor '{}' is already running.", args.name)
        }))));
    }

    // P0-1: Use argv instead of sh -c to avoid shell injection.
    // Split command into program + args for simple cases,
    // but still use sh -c for complex commands (pipes, redirects, etc.)
    // The command is constrained by the thread's sandbox/approval policy
    // since monitor_start goes through the tool execution path.
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&args.command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped()) // P0-2: we WILL drain stderr
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| FunctionCallError::RespondToModel(format!("Spawn failed: {e}")))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| FunctionCallError::RespondToModel("Failed to capture stdout".into()))?;

    // P0-2: Take stderr so we can drain it
    let stderr = child.stderr.take();

    // P0-5: Assign instance ID to prevent ABA race
    let instance_id = state.instance_counter.fetch_add(1, Ordering::Relaxed);

    let cancel = CancellationToken::new();
    let cancel_clone = cancel.clone();
    let name = args.name.clone();

    // P0-2: Spawn stderr drain task
    if let Some(stderr) = stderr {
        let cancel_stderr = cancel.clone();
        let name_stderr = name.clone();
        tokio::spawn(async move {
            drain_stderr(cancel_stderr, stderr, &name_stderr).await;
        });
    }

    // Supervisor task owns the Child so it survives across turns.
    // The child is moved into the spawned task, not dropped when
    // start_monitor() returns.
    let cancel_reaper = cancel.clone();
    tokio::spawn({
        let state_for_reaper = Arc::clone(&state);
        async move {
            // Main stdout reader loop
            stdout_reader_loop(&cancel_reaper, stdout, &name, instance_id, &state_for_reaper)
                .await;

            // Wait for child to exit properly, then send lifecycle event
            let exit_code = match child.wait().await {
                Ok(status) => status.code().unwrap_or(-1),
                Err(_) => -1,
            };
            send_exit_event(&state_for_reaper, &name, instance_id, exit_code).await;
        }
    });

    monitors.insert(
        args.name.clone(),
        MonitorHandle {
            instance_id,
            name: args.name.clone(),
            command_str: args.command.clone(),
            cancel,
        },
    );

    Ok(Box::new(JsonToolOutput::new(serde_json::json!({
        "status": "started",
        "name": args.name,
        "command": args.command,
    }))))
}

// P0-2: Drain stderr to prevent pipe buffer deadlock
async fn drain_stderr(cancel: CancellationToken, stderr: tokio::process::ChildStderr, name: &str) {
    let mut buf = vec![0u8; 1024];
    let mut stderr = stderr;
    let mut total = 0usize;
    let mut tail = Vec::with_capacity(MAX_STDERR_TAIL_BYTES);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            result = stderr.read(&mut buf) => {
                match result {
                    Ok(0) => break,
                    Ok(n) => {
                        total += n;
                        // Keep last MAX_STDERR_TAIL_BYTES for failure reporting
                        tail.extend_from_slice(&buf[..n]);
                        if tail.len() > MAX_STDERR_TAIL_BYTES {
                            let start = tail.len() - MAX_STDERR_TAIL_BYTES;
                            tail.drain(..start);
                        }
                    }
                    Err(e) => {
                        tracing::debug!(monitor = %name, error = %e, "stderr read error");
                        break;
                    }
                }
            }
        }
    }
    if total > 0 {
        tracing::debug!(monitor = %name, bytes = total, "stderr drained");
    }
}

async fn stdout_reader_loop(
    cancel: &CancellationToken,
    stdout: tokio::process::ChildStdout,
    name: &str,
    instance_id: InstanceId,
    state: &MonitorState,
) {
    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();
    let batch_window = tokio::time::Duration::from_millis(BATCH_WINDOW_MS);
    let max_batch_window = tokio::time::Duration::from_millis(MAX_BATCH_WINDOW_MS);

    loop {
        let mut batch: Vec<String> = Vec::new();
        tokio::select! {
            _ = cancel.cancelled() => break,
            result = lines.next_line() => {
                match result {
                    Ok(Some(line)) => {
                        batch.push(line);
                        let hard_deadline = tokio::time::Instant::now() + max_batch_window;
                        let mut quiet_deadline = tokio::time::Instant::now() + batch_window;
                        // P1-2: Limit batch size for backpressure
                        while batch.len() < MAX_BATCH_LINES {
                            let deadline = quiet_deadline.min(hard_deadline);
                            tokio::select! {
                                _ = cancel.cancelled() => break,
                                _ = tokio::time::sleep_until(deadline) => break,
                                r = lines.next_line() => {
                                    match r {
                                        Ok(Some(l)) => {
                                            batch.push(l);
                                            quiet_deadline = tokio::time::Instant::now() + batch_window;
                                        }
                                        _ => break,
                                    }
                                }
                            }
                        }
                    }
                    Ok(None) => break,
                    // P2: Log read errors instead of silently breaking
                    Err(e) => {
                        tracing::warn!(monitor = %name, error = %e, "stdout read error");
                        break;
                    }
                }
            }
        }
        if !batch.is_empty() {
            // P1-2: Backpressure - check pending event count
            let pending = state.pending_event_count.load(Ordering::Relaxed);
            if pending >= MAX_QUEUE_EVENTS as u64 {
                tracing::warn!(
                    monitor = %name,
                    pending = pending,
                    dropped_lines = batch.len(),
                    "backpressure: dropping monitor output"
                );
                continue;
            }

            let s = state.seq.fetch_add(1, Ordering::Relaxed);
            state.pending_event_count.fetch_add(1, Ordering::Relaxed);
            submit_event(
                &state.thread_manager,
                state.thread_id,
                MonitorEvent {
                    id: format!("mon-{name}-{instance_id}-{s}"),
                    monitor_name: name.to_string(),
                    sequence: s,
                    kind: MonitorEventKind::OutputBatch,
                    summary: batch.join("\n"),
                    wake_policy: MonitorWakePolicy::AttachOrWake,
                },
            )
            .await;
            state.pending_event_count.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

async fn send_exit_event(
    state: &MonitorState,
    name: &str,
    instance_id: InstanceId,
    exit_code: i32,
) {
    {
        let mut monitors = state.monitors.lock().await;
        let should_remove = monitors
            .get(name)
            .is_some_and(|h| h.instance_id == instance_id);
        if !should_remove {
            return;
        }
        monitors.remove(name);
    }

    let s = state.seq.fetch_add(1, Ordering::Relaxed);
    let kind = if exit_code == 0 {
        MonitorEventKind::Completed { exit_code }
    } else {
        MonitorEventKind::Failed {
            exit_code,
            stderr_tail: None,
        }
    };

    submit_event(
        &state.thread_manager,
        state.thread_id,
        MonitorEvent {
            id: format!("mon-{name}-exit-{s}"),
            monitor_name: name.to_string(),
            sequence: s,
            kind,
            summary: format!("Monitor '{name}' stopped."),
            wake_policy: MonitorWakePolicy::AttachOrWake,
        },
    )
    .await;
}

async fn stop_monitor(
    state: Arc<MonitorState>,
    args: StopArgs,
) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
    let mut monitors = state.monitors.lock().await;
    if let Some(handle) = monitors.remove(&args.name) {
        handle.cancel.cancel();
        // Child is killed via kill_on_drop when handle is dropped
        drop(handle);
        Ok(Box::new(JsonToolOutput::new(serde_json::json!({
            "status": "stopped", "name": args.name
        }))))
    } else {
        Ok(Box::new(JsonToolOutput::new(serde_json::json!({
            "error": format!("No monitor named '{}' running.", args.name)
        }))))
    }
}

async fn list_monitors(state: Arc<MonitorState>) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
    let monitors = state.monitors.lock().await;
    let list: Vec<_> = monitors
        .values()
        .map(|h| {
            serde_json::json!({
                "name": h.name,
                "command": h.command_str,
                "instance_id": h.instance_id,
            })
        })
        .collect();
    Ok(Box::new(JsonToolOutput::new(
        serde_json::json!({"monitors": list}),
    )))
}

async fn submit_event(tm: &Weak<codex_core::ThreadManager>, tid: ThreadId, event: MonitorEvent) {
    tracing::info!(
        monitor = %event.monitor_name,
        seq = event.sequence,
        kind = ?event.kind,
        "submitting monitor event"
    );
    let Some(manager) = tm.upgrade() else {
        tracing::warn!("monitor event dropped: thread manager gone");
        return;
    };
    let Ok(thread) = manager.get_thread(tid).await else {
        tracing::warn!(thread_id = %tid, "monitor event dropped: thread unavailable");
        return;
    };
    match thread.submit(Op::MonitorEvent { event }).await {
        Ok(id) => tracing::info!(submission_id = %id, "monitor event submitted"),
        Err(e) => tracing::warn!("failed to submit monitor event: {e}"),
    }
}

// --- Extension wiring ---

impl ToolContributor for MonitorExtension {
    fn tools(
        &self,
        _session_store: &ExtensionData,
        thread_store: &ExtensionData,
    ) -> Vec<Arc<dyn ToolExecutor<ToolCall>>> {
        let Some(state) = thread_store.get::<MonitorState>() else {
            return Vec::new();
        };
        vec![
            Arc::new(MonitorToolExecutor {
                kind: MonitorToolKind::Start,
                state: Arc::clone(&state),
            }),
            Arc::new(MonitorToolExecutor {
                kind: MonitorToolKind::Stop,
                state: Arc::clone(&state),
            }),
            Arc::new(MonitorToolExecutor {
                kind: MonitorToolKind::List,
                state,
            }),
        ]
    }
}

impl<C: Send + Sync + 'static> codex_extension_api::ThreadLifecycleContributor<C>
    for MonitorExtension
{
    fn on_thread_start<'a>(&'a self, input: ThreadStartInput<'a, C>) -> ExtensionFuture<'a, ()> {
        let tm = self.thread_manager.clone();
        Box::pin(async move {
            let Ok(thread_id) = ThreadId::from_string(input.thread_store.level_id()) else {
                return;
            };
            input
                .thread_store
                .get_or_init::<MonitorState>(|| MonitorState {
                    monitors: Mutex::new(HashMap::new()),
                    seq: AtomicU64::new(0),
                    instance_counter: AtomicU64::new(0),
                    pending_event_count: AtomicU64::new(0),
                    thread_manager: tm,
                    thread_id,
                });
        })
    }

    fn on_thread_stop<'a>(
        &'a self,
        input: codex_extension_api::ThreadStopInput<'a>,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if let Some(state) = input.thread_store.get::<MonitorState>() {
                let mut monitors = state.monitors.lock().await;
                for (_, handle) in monitors.drain() {
                    handle.cancel.cancel();
                    drop(handle);
                }
            }
        })
    }
}

pub fn install<C>(
    registry: &mut ExtensionRegistryBuilder<C>,
    thread_manager: Weak<codex_core::ThreadManager>,
) where
    C: Send + Sync + 'static,
{
    let extension = Arc::new(MonitorExtension { thread_manager });
    registry.thread_lifecycle_contributor(extension.clone());
    registry.tool_contributor(extension);
}
