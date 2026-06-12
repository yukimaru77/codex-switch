use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_exec_server::provision::Hop;
use codex_exec_server::provision::RemoteLauncher;
use codex_exec_server::provision::VersionPolicy;
use codex_exec_server::provision::ensure_remote_codex;
use codex_exec_server::provision::posix_single_quote;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::env_switch_spec::ENV_SWITCH_TOOL_NAME;
use crate::tools::handlers::env_switch_spec::create_env_switch_tool;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use codex_utils_absolute_path::AbsolutePathBuf;

/// Timeout for each individual `run_remote` invocation (mkdir / echo $HOME).
const RUN_REMOTE_TIMEOUT_SECS: u64 = 20;

/// Handler for the `env_switch` tool.
///
/// Provisions a remote codex exec-server (Docker or SSH) and registers it as
/// a named environment in the session's [`EnvironmentManager`].  The
/// environment id is then returned to the model so it can be supplied as
/// `environment_id` on subsequent `shell_command` / `exec_command` /
/// `apply_patch` / `view_image` calls.
///
/// Unlike the previous implementation this handler does **not** interrupt the
/// current turn, does not update the thread's sticky environment selection,
/// and does not schedule a self-continuation.  All execution is item-level:
/// the model picks the target environment on each individual tool call.
#[derive(Default)]
pub struct EnvSwitchHandler;

/// A single hop element as supplied by the model in the `hops` array.
#[derive(Deserialize)]
struct HopArg {
    #[serde(rename = "type")]
    hop_type: String,
    host: Option<String>,
    container: Option<String>,
}

/// Arguments accepted by the `env_switch` tool.
#[derive(Deserialize)]
struct EnvSwitchArgs {
    target: Option<String>,
    container: Option<String>,
    host: Option<String>,
    cwd: Option<String>,
    hops: Option<Vec<HopArg>>,
    /// Relative mode: environment_id of the existing environment to build upon.
    /// When absent together with `extend`, the thread's most-recently-activated
    /// remote environment is used as the base.
    base: Option<String>,
    /// Relative mode: a single hop to append to the base environment.
    /// When present, `hops` / `target` / `container` / `host` are ignored.
    extend: Option<HopArg>,
}

impl ToolExecutor<ToolInvocation> for EnvSwitchHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(ENV_SWITCH_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_env_switch_tool()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move {
            let ToolInvocation {
                session,
                turn,
                payload,
                ..
            } = invocation;

            let arguments = match payload {
                ToolPayload::Function { arguments } => arguments,
                _ => {
                    return Err(FunctionCallError::RespondToModel(format!(
                        "{ENV_SWITCH_TOOL_NAME} handler received unsupported payload"
                    )));
                }
            };

            let args: EnvSwitchArgs = parse_arguments(&arguments)?;
            handle_env_switch(&session, &turn, args).await
        })
    }
}

impl CoreToolRuntime for EnvSwitchHandler {}

/// Runs a shell script on the remote via the given launcher, returning
/// `(success, stdout, stderr)`.
async fn run_remote(launcher: &RemoteLauncher, script: &str) -> (bool, String, String) {
    let argv = launcher.shell_argv(script);
    let Some((program, rest)) = argv.split_first().map(|(p, r)| (p.clone(), r.to_vec())) else {
        return (false, String::new(), "empty argv".to_string());
    };
    let future = tokio::process::Command::new(&program).args(&rest).output();
    match tokio::time::timeout(Duration::from_secs(RUN_REMOTE_TIMEOUT_SECS), future).await {
        Ok(Ok(out)) => (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ),
        Ok(Err(e)) => (false, String::new(), e.to_string()),
        Err(_) => (
            false,
            String::new(),
            format!("timed out after {RUN_REMOTE_TIMEOUT_SECS}s"),
        ),
    }
}

/// Converts a `HopArg` from the model into a [`Hop`], returning an error
/// message if required fields are missing.
fn hop_from_arg(arg: HopArg) -> Result<Hop, FunctionCallError> {
    match arg.hop_type.as_str() {
        "ssh" => {
            let host = arg.host.ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "env_switch: each hop of type `ssh` requires a `host` field".to_string(),
                )
            })?;
            Ok(Hop::Ssh { host })
        }
        "docker" => {
            let container = arg.container.ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "env_switch: each hop of type `docker` requires a `container` field"
                        .to_string(),
                )
            })?;
            Ok(Hop::Docker { container })
        }
        other => Err(FunctionCallError::RespondToModel(format!(
            "env_switch: unknown hop type `{other}`; valid values are `ssh`, `docker`"
        ))),
    }
}

/// Core logic shared between all switch directions.
async fn handle_env_switch(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    args: EnvSwitchArgs,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    // --- Priority 1: relative mode (extend is present) -----------------------
    if let Some(extend_arg) = args.extend {
        let extend_hop = hop_from_arg(extend_arg)?;

        // Determine the base launcher.
        let base_launcher: RemoteLauncher = if let Some(base_id) = args.base {
            // Explicit base: restore from id string.
            RemoteLauncher::from_id(&base_id).map_err(|e| {
                FunctionCallError::RespondToModel(format!(
                    "env_switch: invalid `base` environment id `{base_id}`: {e}"
                ))
            })?
        } else {
            // Implicit base: use the thread's most-recently-activated launcher.
            let cursors = session.services.last_remote_launcher.lock().await;
            cursors.get(&session.thread_id).cloned().ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "env_switch: relative mode requires a `base` or a previously-activated \
                         remote environment on this thread, but none was found. \
                         Provide an explicit `base` environment_id."
                        .to_string(),
                )
            })?
        };

        let new_launcher = base_launcher.with_appended_hop(extend_hop);
        return handle_remote_switch(session, new_launcher, args.cwd).await;
    }

    // --- Priority 2: absolute multi-hop (hops array) -------------------------
    if let Some(hop_args) = args.hops {
        if hop_args.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "env_switch: `hops` must not be empty when provided".to_string(),
            ));
        }
        let hops: Vec<Hop> = hop_args
            .into_iter()
            .map(hop_from_arg)
            .collect::<Result<_, _>>()?;
        let launcher = RemoteLauncher::layered(hops);
        return handle_remote_switch(session, launcher, args.cwd).await;
    }

    // --- Priority 3: legacy single-hop `target` parameter -------------------
    let target = args.target.as_deref().unwrap_or("");
    match target {
        "local" => handle_local_switch(turn, args.cwd),
        "docker" => {
            let container = args.container.ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "env_switch: `container` is required when target is `docker`".to_string(),
                )
            })?;
            handle_remote_switch(session, RemoteLauncher::docker(container), args.cwd).await
        }
        "ssh" => {
            let host = args.host.ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "env_switch: `host` is required when target is `ssh`".to_string(),
                )
            })?;
            handle_remote_switch(session, RemoteLauncher::ssh(host), args.cwd).await
        }
        other => Err(FunctionCallError::RespondToModel(format!(
            "env_switch: unsupported target `{other}`; valid values are `local`, `docker`, `ssh`, \
             or provide a `hops` array for multi-hop routing, \
             or provide an `extend` object for relative mode"
        ))),
    }
}

/// Returns a message explaining that the host environment is the default.
/// No state is mutated; the model should omit `environment_id` (or use
/// `"local"` / `{LOCAL_ENVIRONMENT_ID}`) to stay on the host.
fn handle_local_switch(
    turn: &Arc<TurnContext>,
    _explicit_cwd: Option<String>,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    let _ = turn;
    let message = format!(
        "env_switch complete: the local host environment is active (id: `{LOCAL_ENVIRONMENT_ID}`). \
         Omit `environment_id` on shell/apply_patch/view_image calls to execute on the host."
    );
    Ok(boxed_tool_output(FunctionToolOutput::from_text(
        message,
        Some(true),
    )))
}

/// Provisions the remote codex exec-server and registers it as a named
/// environment.  Returns a message instructing the model to pass
/// `environment_id` on subsequent tool calls.
async fn handle_remote_switch(
    session: &Arc<Session>,
    launcher: RemoteLauncher,
    explicit_cwd: Option<String>,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    // --- Step 1: provision the remote codex exec-server --------------------------
    let provisioned = ensure_remote_codex(&launcher, &VersionPolicy::HostVersion)
        .await
        .map_err(|e| {
            FunctionCallError::RespondToModel(format!(
                "env_switch: failed to provision remote codex: {e}"
            ))
        })?;

    let codex_path = provisioned.codex_path.clone();
    let deployed_version = provisioned.version.clone();

    // --- Step 2: determine effective cwd and ensure it exists --------------------
    let remote_cwd: String = if let Some(cwd) = explicit_cwd {
        let script = format!("mkdir -p {}", posix_single_quote(&cwd));
        let (ok, _, err) = run_remote(&launcher, &script).await;
        if !ok {
            return Err(FunctionCallError::RespondToModel(format!(
                "env_switch: could not create remote cwd `{cwd}`: {err}"
            )));
        }
        cwd
    } else {
        let script = "printf '%s\\n' \"$HOME\" && mkdir -p \"$HOME\"";
        let (ok, home, err) = run_remote(&launcher, script).await;
        if ok && !home.is_empty() {
            home
        } else {
            return Err(FunctionCallError::RespondToModel(format!(
                "env_switch: could not determine remote $HOME: {err}"
            )));
        }
    };

    // --- Step 3: build the environment id and register the stdio environment ----
    let environment_id = launcher.id();
    let target_label = environment_id.clone();

    let exec_server_inner = vec![
        codex_path.clone(),
        "exec-server".to_string(),
        "--listen".to_string(),
        "stdio".to_string(),
        "-c".to_string(),
        "sandbox_mode=danger-full-access".to_string(),
        "-c".to_string(),
        "approval_policy=never".to_string(),
    ];
    let exec_argv = launcher.exec_argv(exec_server_inner);
    let (program, args) = exec_argv
        .split_first()
        .map(|(p, rest)| (p.clone(), rest.to_vec()))
        .ok_or_else(|| {
            FunctionCallError::RespondToModel("env_switch: internal error: empty argv".to_string())
        })?;

    session
        .services
        .environment_manager
        .upsert_stdio_environment(
            environment_id.clone(),
            program,
            args,
            HashMap::new(),
            /*cwd*/ None,
        )
        .map_err(|e| {
            FunctionCallError::RespondToModel(format!(
                "env_switch: failed to register remote environment `{environment_id}`: {e}"
            ))
        })?;

    // --- Step 4: record the remote cwd so resolve_tool_environment can use it ---
    let abs_cwd = AbsolutePathBuf::from_absolute_path_checked(&remote_cwd).map_err(|e| {
        FunctionCallError::RespondToModel(format!(
            "env_switch: remote cwd `{remote_cwd}` is not an absolute path: {e}"
        ))
    })?;
    {
        let mut cwds = session.services.dynamic_environment_cwds.lock().await;
        cwds.insert(environment_id.clone(), abs_cwd);
    }

    // --- Step 4b: update per-thread last-launcher cursor (relative mode base) ---
    {
        let mut cursors = session.services.last_remote_launcher.lock().await;
        cursors.insert(session.thread_id, launcher.clone());
    }

    // --- Step 5: emit a ThreadSettingsApplied badge event (non-sticky) -----------
    // We re-use the existing badge mechanism to surface the active remote env in
    // the TUI.  We emit a badge showing the new environment id without changing
    // the thread's sticky environment selection (so future turns still default
    // to the local host unless the model explicitly passes environment_id).
    session
        .emit_dynamic_environment_badge(&environment_id)
        .await;

    // --- Step 6: return instructive message to the model -------------------------
    let message = format!(
        "env_switch complete: environment `{environment_id}` is ready \
         ({target_label}, codex {deployed_version} at {codex_path}, cwd={remote_cwd}). \
         To run commands inside this environment, pass `\"environment_id\": \"{environment_id}\"` \
         to shell_command / exec_command / apply_patch / view_image. \
         Omitting environment_id continues to execute on the local host. \
         Do NOT call env_switch again for the same target unless you want to re-provision."
    );
    Ok(boxed_tool_output(FunctionToolOutput::from_text(
        message,
        Some(true),
    )))
}

#[cfg(test)]
#[path = "env_switch_tests.rs"]
mod tests;
