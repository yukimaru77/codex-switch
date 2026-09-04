use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_protocol::models::ResponseInputItem;
use codex_tools::JsonSchema;
use codex_tools::JsonToolOutput;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::tools::context::ExecCommandToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::rewrite_function_string_argument;
use crate::tools::handlers::unified_exec::ExecCommandHandler;
use crate::tools::handlers::unified_exec::ExecCommandHandlerOptions;
use crate::tools::handlers::updated_hook_command;
use crate::tools::hook_names::HookToolName;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::PreToolUsePayload;
use crate::tools::registry::ToolExecutor;

#[path = "monitor_runtime.rs"]
mod runtime;
use runtime::emit_exit;
use runtime::emit_output;
use runtime::run_monitor;

const MONITOR_START_TOOL: &str = "monitor_start";
const MONITOR_STOP_TOOL: &str = "monitor_stop";
const MONITOR_LIST_TOOL: &str = "monitor_list";

const MAX_NAME_LEN: usize = 64;
const MAX_EVENT_BYTES: usize = 32 * 1024;
const OUTPUT_POLL_MS: u64 = 250;
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

type InstanceId = u64;

struct MonitorHandle {
    instance_id: InstanceId,
    command: String,
    process_id: Option<i32>,
    cancel: CancellationToken,
    stopped: Arc<Notify>,
}

#[derive(Default)]
pub(crate) struct MonitorManager {
    monitors: Mutex<HashMap<String, MonitorHandle>>,
    sequence: AtomicU64,
    instance: AtomicU64,
}

impl MonitorManager {
    async fn reserve(
        &self,
        name: String,
        command: String,
    ) -> Result<(InstanceId, CancellationToken, Arc<Notify>), String> {
        let mut monitors = self.monitors.lock().await;
        if monitors.contains_key(&name) {
            return Err(format!("Monitor '{name}' is already running."));
        }
        let instance_id = self.instance.fetch_add(1, Ordering::Relaxed);
        let cancel = CancellationToken::new();
        let stopped = Arc::new(Notify::new());
        monitors.insert(
            name,
            MonitorHandle {
                instance_id,
                command,
                process_id: None,
                cancel: cancel.clone(),
                stopped: Arc::clone(&stopped),
            },
        );
        Ok((instance_id, cancel, stopped))
    }

    async fn activate(&self, name: &str, instance_id: InstanceId, process_id: i32) -> bool {
        let mut monitors = self.monitors.lock().await;
        let Some(handle) = monitors.get_mut(name) else {
            return false;
        };
        if handle.instance_id != instance_id {
            return false;
        }
        handle.process_id = Some(process_id);
        true
    }

    async fn remove_matching(&self, name: &str, instance_id: InstanceId) -> Option<MonitorHandle> {
        let mut monitors = self.monitors.lock().await;
        if monitors
            .get(name)
            .is_some_and(|handle| handle.instance_id == instance_id)
        {
            monitors.remove(name)
        } else {
            None
        }
    }

    fn next_sequence(&self) -> u64 {
        self.sequence.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) async fn cancel_process(&self, process_id: i32) {
        let monitors = self.monitors.lock().await;
        if let Some(handle) = monitors
            .values()
            .find(|handle| handle.process_id == Some(process_id))
        {
            handle.cancel.cancel();
        }
    }

    pub(crate) async fn cancel_all(&self) {
        let monitors = self.monitors.lock().await;
        for handle in monitors.values() {
            handle.cancel.cancel();
        }
    }

    pub(crate) async fn shutdown(&self) {
        let handles = self
            .monitors
            .lock()
            .await
            .drain()
            .map(|(_, handle)| handle)
            .collect::<Vec<_>>();
        for handle in handles {
            handle.cancel.cancel();
            handle.stopped.notify_waiters();
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum MonitorToolKind {
    Start,
    Stop,
    List,
}

pub(crate) struct MonitorHandler {
    kind: MonitorToolKind,
    exec_options: ExecCommandHandlerOptions,
}

impl MonitorHandler {
    pub(crate) fn new(kind: MonitorToolKind, exec_options: ExecCommandHandlerOptions) -> Self {
        Self { kind, exec_options }
    }
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

struct MonitorStartupCleanup {
    session: Weak<Session>,
    name: String,
    instance_id: InstanceId,
    call_id: String,
}

struct MonitorStartupGuard(Option<MonitorStartupCleanup>);

impl MonitorStartupGuard {
    fn new(session: Weak<Session>, name: String, instance_id: InstanceId, call_id: String) -> Self {
        Self(Some(MonitorStartupCleanup {
            session,
            name,
            instance_id,
            call_id,
        }))
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for MonitorStartupGuard {
    fn drop(&mut self) {
        let Some(cleanup) = self.0.take() else {
            return;
        };
        let Some(session) = cleanup.session.upgrade() else {
            return;
        };
        tokio::spawn(async move {
            cancel_startup(
                &session,
                &cleanup.name,
                cleanup.instance_id,
                &cleanup.call_id,
            )
            .await;
        });
    }
}

struct MonitorToolOutput {
    json: JsonToolOutput,
    exec: Option<ExecCommandToolOutput>,
}

impl MonitorToolOutput {
    fn new(value: serde_json::Value, success: bool, exec: Option<ExecCommandToolOutput>) -> Self {
        Self {
            json: JsonToolOutput::with_success(value, Some(success)),
            exec,
        }
    }
}

impl ToolOutput for MonitorToolOutput {
    fn log_output(&self) -> String {
        self.json.log_output()
    }

    fn success_for_logging(&self) -> bool {
        self.json.success_for_logging()
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        self.json.to_response_item(call_id, payload)
    }

    fn post_tool_use_id(&self, call_id: &str) -> String {
        self.exec.as_ref().map_or_else(
            || call_id.to_string(),
            |exec| exec.post_tool_use_id(call_id),
        )
    }

    fn post_tool_use_input(&self, payload: &ToolPayload) -> Option<serde_json::Value> {
        self.exec
            .as_ref()
            .and_then(|exec| exec.post_tool_use_input(payload))
    }

    fn post_tool_use_response(
        &self,
        call_id: &str,
        payload: &ToolPayload,
    ) -> Option<serde_json::Value> {
        self.exec
            .as_ref()
            .and_then(|exec| exec.post_tool_use_response(call_id, payload))
    }

    fn code_mode_result(&self, payload: &ToolPayload) -> serde_json::Value {
        self.json.code_mode_result(payload)
    }
}

impl ToolExecutor<ToolInvocation> for MonitorHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(match self.kind {
            MonitorToolKind::Start => MONITOR_START_TOOL,
            MonitorToolKind::Stop => MONITOR_STOP_TOOL,
            MonitorToolKind::List => MONITOR_LIST_TOOL,
        })
    }

    fn spec(&self) -> ToolSpec {
        match self.kind {
            MonitorToolKind::Start => start_spec(),
            MonitorToolKind::Stop => stop_spec(),
            MonitorToolKind::List => list_spec(),
        }
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            match self.kind {
                MonitorToolKind::Start => self.start(invocation).await,
                MonitorToolKind::Stop => stop(invocation).await,
                MonitorToolKind::List => list(invocation).await,
            }
        })
    }
}

impl CoreToolRuntime for MonitorHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }

    fn pre_tool_use_payload(&self, invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        if !matches!(self.kind, MonitorToolKind::Start) {
            return None;
        }
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return None;
        };
        let args: StartArgs = serde_json::from_str(arguments).ok()?;
        Some(PreToolUsePayload {
            tool_name: HookToolName::bash(),
            tool_input: serde_json::json!({ "command": args.command }),
        })
    }

    fn with_updated_hook_input(
        &self,
        mut invocation: ToolInvocation,
        updated_input: serde_json::Value,
    ) -> Result<ToolInvocation, FunctionCallError> {
        if !matches!(self.kind, MonitorToolKind::Start) {
            return Ok(invocation);
        }
        let ToolPayload::Function { arguments } = invocation.payload else {
            return Err(FunctionCallError::RespondToModel(
                "hook input rewrite received unsupported monitor_start payload".to_string(),
            ));
        };
        invocation.payload = ToolPayload::Function {
            arguments: rewrite_function_string_argument(
                &arguments,
                MONITOR_START_TOOL,
                "command",
                updated_hook_command(&updated_input)?,
            )?,
        };
        Ok(invocation)
    }

    fn post_tool_use_payload(
        &self,
        invocation: &ToolInvocation,
        result: &dyn ToolOutput,
    ) -> Option<PostToolUsePayload> {
        if !matches!(self.kind, MonitorToolKind::Start) {
            return None;
        }
        let tool_input = result.post_tool_use_input(&invocation.payload)?;
        let tool_use_id = result.post_tool_use_id(&invocation.call_id);
        let tool_response = result.post_tool_use_response(&tool_use_id, &invocation.payload)?;
        Some(PostToolUsePayload {
            tool_name: HookToolName::bash(),
            tool_use_id,
            tool_input,
            tool_response,
        })
    }
}

impl MonitorHandler {
    async fn start(
        &self,
        mut invocation: ToolInvocation,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let args: StartArgs = parse_function_args(&invocation)?;
        if let Err(error) = validate_start_args(&args) {
            return Ok(Box::new(MonitorToolOutput::new(
                serde_json::json!({ "error": error }),
                false,
                None,
            )));
        }

        let session = Arc::clone(&invocation.session);
        let step_context = Arc::clone(&invocation.step_context);
        let startup_cancelled = invocation.cancellation_token.clone();
        let call_id = invocation.call_id.clone();
        let (instance_id, cancel, stopped) = match session
            .services
            .monitor_manager
            .reserve(args.name.clone(), args.command.clone())
            .await
        {
            Ok(reservation) => reservation,
            Err(error) => {
                return Ok(Box::new(MonitorToolOutput::new(
                    serde_json::json!({ "error": error }),
                    false,
                    None,
                )));
            }
        };
        // If the tool future is aborted while Unified Exec is starting, this
        // guard runs only after that future can no longer register a process.
        // This closes the registration race without keeping a turn alive.
        let mut startup_guard = MonitorStartupGuard::new(
            Arc::downgrade(&session),
            args.name.clone(),
            instance_id,
            call_id.clone(),
        );

        invocation.payload = ToolPayload::Function {
            arguments: serde_json::json!({
                "cmd": args.command,
                "tty": false,
                "yield_time_ms": OUTPUT_POLL_MS,
                "max_output_tokens": MAX_EVENT_BYTES / 4,
            })
            .to_string(),
        };

        let exec_output = match ExecCommandHandler::new(self.exec_options)
            .execute(invocation)
            .await
        {
            Ok(output) => output,
            Err(error) => {
                cancel_startup(&session, &args.name, instance_id, &call_id).await;
                startup_guard.disarm();
                return Err(error);
            }
        };

        let initial_output = exec_output.raw_output.clone();
        if startup_cancelled.is_cancelled() || cancel.is_cancelled() {
            cancel_startup(&session, &args.name, instance_id, &call_id).await;
            startup_guard.disarm();
            return Err(FunctionCallError::RespondToModel(
                "monitor start was cancelled".to_string(),
            ));
        }
        let Some(process_id) = exec_output.process_id else {
            if let Some(handle) = session
                .services
                .monitor_manager
                .remove_matching(&args.name, instance_id)
                .await
            {
                handle.stopped.notify_waiters();
            }
            startup_guard.disarm();
            if !initial_output.is_empty() {
                emit_output(&session, &args.name, instance_id, &initial_output).await;
            }
            let exit_code = exec_output.exit_code.unwrap_or(0);
            emit_exit(&session, &args.name, instance_id, exit_code, false, None).await;
            let success = exit_code == 0;
            return Ok(Box::new(MonitorToolOutput::new(
                serde_json::json!({
                    "status": if success { "completed" } else { "failed" },
                    "name": args.name,
                    "command": args.command,
                    "exit_code": exit_code,
                }),
                success,
                Some(exec_output),
            )));
        };

        if !session
            .services
            .monitor_manager
            .activate(&args.name, instance_id, process_id)
            .await
        {
            session
                .services
                .unified_exec_manager
                .terminate_process(process_id)
                .await;
            stopped.notify_waiters();
            startup_guard.disarm();
            return Err(FunctionCallError::RespondToModel(
                "monitor registration disappeared before process startup completed".to_string(),
            ));
        }
        if startup_cancelled.is_cancelled() || cancel.is_cancelled() {
            cancel_startup(&session, &args.name, instance_id, &call_id).await;
            startup_guard.disarm();
            return Err(FunctionCallError::RespondToModel(
                "monitor start was cancelled".to_string(),
            ));
        }

        tokio::spawn(run_monitor(
            Arc::downgrade(&session),
            step_context,
            call_id,
            args.name.clone(),
            instance_id,
            process_id,
            cancel,
            stopped,
            initial_output,
        ));
        startup_guard.disarm();

        Ok(Box::new(MonitorToolOutput::new(
            serde_json::json!({
                "status": "started",
                "name": args.name,
                "command": args.command,
            }),
            true,
            Some(exec_output),
        )))
    }
}

async fn cancel_startup(
    session: &Arc<Session>,
    name: &str,
    instance_id: InstanceId,
    call_id: &str,
) {
    let handle = session
        .services
        .monitor_manager
        .remove_matching(name, instance_id)
        .await;
    let Some(handle) = handle else {
        // Session cleanup may already have drained the monitor reservation.
        // The tool future has stopped before this cleanup runs, so a matching
        // Unified Exec entry can no longer appear after this lookup.
        session
            .services
            .unified_exec_manager
            .terminate_process_by_call_id(call_id)
            .await;
        return;
    };
    handle.cancel.cancel();
    match handle.process_id {
        Some(process_id) => {
            session
                .services
                .unified_exec_manager
                .terminate_process(process_id)
                .await;
        }
        None => {
            session
                .services
                .unified_exec_manager
                .terminate_process_by_call_id(call_id)
                .await;
        }
    }
    handle.stopped.notify_waiters();
}

async fn stop(invocation: ToolInvocation) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
    let args: StopArgs = parse_function_args(&invocation)?;
    let state = {
        let monitors = invocation
            .session
            .services
            .monitor_manager
            .monitors
            .lock()
            .await;
        monitors.get(&args.name).map(|handle| {
            // Create the waiter while holding the manager lock. The monitor
            // must acquire this same lock before removing itself and notifying,
            // so a natural exit cannot race between lookup and registration.
            let stopped_notified = Arc::clone(&handle.stopped).notified_owned();
            (
                handle.instance_id,
                handle.process_id,
                handle.cancel.clone(),
                stopped_notified,
            )
        })
    };
    let Some((instance_id, process_id, cancel, stopped_notified)) = state else {
        return Ok(Box::new(MonitorToolOutput::new(
            serde_json::json!({ "error": format!("No monitor named '{}' is running.", args.name) }),
            false,
            None,
        )));
    };
    let Some(process_id) = process_id else {
        return Ok(Box::new(MonitorToolOutput::new(
            serde_json::json!({ "error": format!("Monitor '{}' is still starting.", args.name) }),
            false,
            None,
        )));
    };

    cancel.cancel();
    let terminated = invocation
        .session
        .services
        .unified_exec_manager
        .terminate_process(process_id)
        .await;
    if !terminated {
        tracing::debug!(monitor = %args.name, process_id, "monitor process already exited");
    }

    if tokio::time::timeout(STOP_TIMEOUT, stopped_notified)
        .await
        .is_err()
    {
        invocation
            .session
            .services
            .monitor_manager
            .remove_matching(&args.name, instance_id)
            .await;
        return Ok(Box::new(MonitorToolOutput::new(
            serde_json::json!({ "error": format!("Timed out stopping monitor '{}'.", args.name) }),
            false,
            None,
        )));
    }

    Ok(Box::new(MonitorToolOutput::new(
        serde_json::json!({ "status": "stopped", "name": args.name }),
        true,
        None,
    )))
}

async fn list(invocation: ToolInvocation) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
    let monitors = invocation
        .session
        .services
        .monitor_manager
        .monitors
        .lock()
        .await;
    let mut list = monitors
        .iter()
        .map(|(name, handle)| {
            serde_json::json!({
                "name": name,
                "command": handle.command,
                "status": if handle.process_id.is_some() { "running" } else { "starting" },
            })
        })
        .collect::<Vec<_>>();
    list.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
    Ok(Box::new(MonitorToolOutput::new(
        serde_json::json!({ "monitors": list }),
        true,
        None,
    )))
}

fn parse_function_args<T>(invocation: &ToolInvocation) -> Result<T, FunctionCallError>
where
    T: for<'de> Deserialize<'de>,
{
    let ToolPayload::Function { arguments } = &invocation.payload else {
        return Err(FunctionCallError::RespondToModel(
            "monitor handler received unsupported payload".to_string(),
        ));
    };
    serde_json::from_str(arguments)
        .map_err(|error| FunctionCallError::RespondToModel(format!("Invalid arguments: {error}")))
}

fn validate_start_args(args: &StartArgs) -> Result<(), String> {
    if args.name.is_empty() || args.name.len() > MAX_NAME_LEN {
        return Err(format!(
            "Monitor name must be between 1 and {MAX_NAME_LEN} bytes."
        ));
    }
    if !args
        .name
        .chars()
        .all(|character| character.is_alphanumeric() || character == '-' || character == '_')
    {
        return Err(
            "Monitor name must contain only letters, digits, dash, or underscore.".to_string(),
        );
    }
    if args.command.trim().is_empty() {
        return Err("Monitor command must not be empty.".to_string());
    }
    Ok(())
}

fn start_spec() -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: MONITOR_START_TOOL.to_string(),
        description: "Launch a new background monitor to observe a long-running command. The command runs through the same approval, sandbox, working-directory, and execution-environment path as exec_command. Its output is batched into notifications delivered at the next safe model-call boundary, including while the thread is idle. Use this for test watchers, build systems, log tails, file watchers, or any long-running process. Bug reports: https://github.com/yukimaru77/codex-switch/issues"
            .to_string(),
        strict: true,
        defer_loading: None,
        parameters: JsonSchema::object(
            BTreeMap::from([
                (
                    "name".to_string(),
                    JsonSchema::string(Some(
                        "Unique name using letters, digits, dash, or underscore.".to_string(),
                    )),
                ),
                (
                    "command".to_string(),
                    JsonSchema::string(Some(
                        "The shell command to run in the background.".to_string(),
                    )),
                ),
            ]),
            Some(vec!["name".to_string(), "command".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

fn stop_spec() -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: MONITOR_STOP_TOOL.to_string(),
        description: "Stop a running background monitor by name. The complete monitored process is terminated and a cancellation notification is delivered. Bug reports: https://github.com/yukimaru77/codex-switch/issues"
            .to_string(),
        strict: true,
        defer_loading: None,
        parameters: JsonSchema::object(
            BTreeMap::from([(
                "name".to_string(),
                JsonSchema::string(Some("Name of the monitor to stop.".to_string())),
            )]),
            Some(vec!["name".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

fn list_spec() -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: MONITOR_LIST_TOOL.to_string(),
        description: "List all currently running background monitors with their names, commands, and states. Bug reports: https://github.com/yukimaru77/codex-switch/issues"
            .to_string(),
        strict: true,
        defer_loading: None,
        parameters: JsonSchema::object(BTreeMap::new(), Some(Vec::new()), Some(false.into())),
        output_schema: None,
    })
}

#[cfg(test)]
#[path = "monitor_tests.rs"]
mod tests;
