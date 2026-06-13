use codex_exec_server::EnvironmentMetadata;
use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_exec_server::provision::Hop;
use codex_exec_server::provision::RemoteLauncher;
use codex_exec_server::provision::VersionPolicy;
use codex_exec_server::provision::ensure_remote_codex;
use codex_exec_server::provision::posix_single_quote;
use codex_exec_server::provision::validate_hop_value;
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
/// This handler is item-level: the model picks the target environment on each
/// individual tool call.  Provisioning does not interrupt the current turn and
/// does not change the thread's sticky environment selection.  The badge emitted
/// via [`Session::emit_dynamic_environment_badge`] is display-only and shows
/// the most recently provisioned remote environment.
#[derive(Default)]
pub struct EnvSwitchHandler;

/// A single hop element as supplied by the model in the `hops` array.
#[derive(Deserialize)]
pub(super) struct HopArg {
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
/// message if required fields are missing or contain unsafe values.
///
/// Validation via [`validate_hop_value`] rejects values that are empty, start
/// with `-` (would be interpreted as CLI flags), or contain `>` (the id
/// segment separator that would corrupt round-trip parsing).
pub(super) fn hop_from_arg(arg: HopArg) -> Result<Hop, FunctionCallError> {
    match arg.hop_type.as_str() {
        "ssh" => {
            let host = arg.host.ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "env_switch: each hop of type `ssh` requires a `host` field".to_string(),
                )
            })?;
            validate_hop_value(&host).map_err(|e| {
                FunctionCallError::RespondToModel(format!(
                    "env_switch: invalid `host` value for ssh hop: {e}"
                ))
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
            validate_hop_value(&container).map_err(|e| {
                FunctionCallError::RespondToModel(format!(
                    "env_switch: invalid `container` value for docker hop: {e}"
                ))
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
            // Explicit base: validate and reconstruct from id string.
            // Only launchers whose ids can be round-tripped through from_id are
            // accepted; this rejects arbitrary user-supplied strings.
            RemoteLauncher::from_id(&base_id).map_err(|e| {
                FunctionCallError::RespondToModel(format!(
                    "env_switch: invalid `base` environment id `{base_id}`: {e}"
                ))
            })?
        } else {
            // Implicit base: use the thread's most-recently-activated launcher
            // stored in the shared EnvironmentManager.
            let thread_key = session.thread_id.to_string();
            session
                .services
                .environment_manager
                .get_last_launcher(&thread_key)
                .ok_or_else(|| {
                    FunctionCallError::RespondToModel(
                        "env_switch: relative mode requires a `base` or a previously-activated \
                         remote environment on this thread, but none was found. \
                         Provide an explicit `base` environment_id."
                            .to_string(),
                    )
                })?
        };

        let new_launcher = base_launcher.with_appended_hop(extend_hop);
        return handle_remote_switch(session, turn, new_launcher, args.cwd).await;
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
        return handle_remote_switch(session, turn, launcher, args.cwd).await;
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
            validate_hop_value(&container).map_err(|e| {
                FunctionCallError::RespondToModel(format!(
                    "env_switch: invalid `container` value: {e}"
                ))
            })?;
            handle_remote_switch(session, turn, RemoteLauncher::docker(container), args.cwd).await
        }
        "ssh" => {
            let host = args.host.ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "env_switch: `host` is required when target is `ssh`".to_string(),
                )
            })?;
            validate_hop_value(&host).map_err(|e| {
                FunctionCallError::RespondToModel(format!("env_switch: invalid `host` value: {e}"))
            })?;
            handle_remote_switch(session, turn, RemoteLauncher::ssh(host), args.cwd).await
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

/// Derives the exec-server sandbox and approval policy flags to pass to the
/// remote codex process from the current turn's configuration.
///
/// The remote exec-server is started with the host session's sandbox mode and
/// approval policy so that the security posture is inherited rather than
/// hard-coded.  Mapping:
///
/// - `PermissionProfile::Disabled` → `sandbox_mode=danger-full-access`
/// - `PermissionProfile::Managed` with full disk write → `sandbox_mode=danger-full-access`
/// - otherwise → `sandbox_mode=workspace-write` (best available on the remote)
///
/// For the approval policy we use the turn's `approval_policy` value.
/// `AskForApproval::Never` → `approval_policy=never` (auto-approve).
/// Any other value → `approval_policy=on-failure` (prompt on failure, safest
/// remote default that does not require an interactive terminal on the remote).
///
/// Note: the remote exec-server runs inside a container/SSH hop that is already
/// controlled by the model.  Using the host's sandbox mode here prevents the
/// remote from accidentally getting a *more permissive* policy than the host
/// session.
fn derive_exec_server_policy_flags(turn: &TurnContext) -> Vec<String> {
    use codex_protocol::models::PermissionProfile;
    use codex_protocol::protocol::AskForApproval;

    let sandbox_tag = match &turn.permission_profile {
        PermissionProfile::Disabled => "danger-full-access",
        PermissionProfile::External { .. } => "danger-full-access",
        PermissionProfile::Managed { .. } => {
            let fsp = turn.permission_profile.file_system_sandbox_policy();
            if fsp.has_full_disk_write_access() {
                "danger-full-access"
            } else {
                "workspace-write"
            }
        }
    };

    let approval_tag = match turn.approval_policy.value() {
        AskForApproval::Never => "never",
        _ => "on-failure",
    };

    vec![
        "-c".to_string(),
        format!("sandbox_mode={sandbox_tag}"),
        "-c".to_string(),
        format!("approval_policy={approval_tag}"),
    ]
}

/// Provisions the remote codex exec-server and registers it as a named
/// environment.  Returns a message instructing the model to pass
/// `environment_id` on subsequent tool calls.
async fn handle_remote_switch(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
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
    let provision_warning = provisioned.warning.clone();

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

    // --- Step 3: build the environment id -----------------------------------------
    let environment_id = launcher.id();

    // --- Step 4: record metadata BEFORE registering the environment ---------------
    // Recording metadata first closes the window where a concurrent exec_command
    // could observe an environment that exists in the registry but has no cwd/shell.
    let abs_cwd = AbsolutePathBuf::from_absolute_path_checked(&remote_cwd).map_err(|e| {
        FunctionCallError::RespondToModel(format!(
            "env_switch: remote cwd `{remote_cwd}` is not an absolute path: {e}"
        ))
    })?;
    let _ = abs_cwd; // stored as string in EnvironmentMetadata; AbsolutePathBuf validates it
    session
        .services
        .environment_manager
        .set_environment_metadata(
            environment_id.clone(),
            EnvironmentMetadata {
                cwd: remote_cwd.clone(),
                shell: provisioned.shell.clone(),
            },
        );

    // Record the launcher in the shared registry so sub-agents and relative-mode
    // calls on this thread can find the most-recently-activated launcher.
    let thread_key = session.thread_id.to_string();
    session
        .services
        .environment_manager
        .set_last_launcher(thread_key, launcher.clone());

    // --- Step 5: register the stdio environment in the EnvironmentManager ----------
    let policy_flags = derive_exec_server_policy_flags(turn);
    let exec_server_inner = std::iter::once(codex_path.clone())
        .chain([
            "exec-server".to_string(),
            "--listen".to_string(),
            "stdio".to_string(),
        ])
        .chain(policy_flags)
        .collect::<Vec<_>>();
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

    // --- Step 6: emit a display-only badge event ---------------------------------
    // Shows the most recently provisioned remote environment in the TUI.
    // This is a display-only badge; it does not change the thread's sticky
    // environment selection (future turns still default to the local host
    // unless the model explicitly passes environment_id).
    session
        .emit_dynamic_environment_badge(&environment_id)
        .await;

    // --- Step 7: return instructive message to the model -------------------------
    let warning_suffix = match &provision_warning {
        Some(w) => format!("\nWARNING: {w}"),
        None => String::new(),
    };
    let message = format!(
        "env_switch complete: environment `{environment_id}` is ready \
         (codex {deployed_version} at {codex_path}, cwd={remote_cwd}). \
         To run commands inside this environment, pass `\"environment_id\": \"{environment_id}\"` \
         to shell_command / exec_command / apply_patch / view_image. \
         Omitting environment_id continues to execute on the local host. \
         Do NOT call env_switch again for the same target unless you want to re-provision.\
         {warning_suffix}"
    );
    Ok(boxed_tool_output(FunctionToolOutput::from_text(
        message,
        Some(true),
    )))
}

#[cfg(test)]
#[path = "env_switch_tests.rs"]
mod tests;
