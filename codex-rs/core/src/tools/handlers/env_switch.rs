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
use crate::tools::handlers::dynamic_environment_visible_to_thread;
use crate::tools::handlers::env_switch_spec::ENV_SWITCH_TOOL_NAME;
use crate::tools::handlers::env_switch_spec::create_env_switch_tool;
use crate::tools::handlers::environment_thread_keys;
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
/// environment id becomes the thread's default execution target for compatible
/// `exec_command` / `apply_patch` / `view_image` calls that omit
/// `environment_id`.
///
/// Compatible tools may still pass `environment_id` explicitly to override the
/// current default for a single call.  The badge emitted via
/// [`Session::emit_dynamic_environment_badge`] mirrors the effective default
/// target for TUI clients.
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
    /// Mutually exclusive with `hops` and `target`.
    extend: Option<HopArg>,
}

#[async_trait::async_trait]
impl ToolExecutor<ToolInvocation> for EnvSwitchHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(ENV_SWITCH_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_env_switch_tool()
    }

    async fn handle(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
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
    let mut command = tokio::process::Command::new(&program);
    command.args(&rest);
    command.kill_on_drop(true);
    let future = command.output();
    match tokio::time::timeout(Duration::from_secs(RUN_REMOTE_TIMEOUT_SECS), future).await {
        Ok(Ok(out)) => (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).to_string(),
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

fn implicit_base_launcher(session: &Session, turn: &TurnContext) -> Option<RemoteLauncher> {
    let environment_manager = &session.services.environment_manager;
    let current_thread_key = session.thread_id.to_string();
    if let Some(current_environment_id) =
        environment_manager.get_last_environment_id(&current_thread_key)
    {
        return if current_environment_id == LOCAL_ENVIRONMENT_ID {
            None
        } else {
            environment_manager.get_last_launcher(&current_thread_key)
        };
    }

    environment_thread_keys(session, turn)
        .into_iter()
        .skip(1)
        .find_map(|thread_key| environment_manager.get_last_launcher(&thread_key))
}

fn base_environment_visible(session: &Session, turn: &TurnContext, environment_id: &str) -> bool {
    turn.environments
        .turn_environments
        .iter()
        .any(|environment| environment.environment_id == environment_id)
        || dynamic_environment_visible_to_thread(session, turn, environment_id)
}

fn remote_cwd_shell_expr(cwd: &str) -> String {
    if cwd == "~" {
        "\"$HOME\"".to_string()
    } else if let Some(rest) = cwd.strip_prefix("~/") {
        format!("\"$HOME\"/{}", posix_single_quote(rest))
    } else {
        posix_single_quote(cwd)
    }
}

fn resolve_remote_cwd_script(cwd: Option<&str>) -> String {
    let cwd_expr = cwd
        .filter(|cwd| !cwd.is_empty())
        .map(remote_cwd_shell_expr)
        .unwrap_or_else(|| "\"$HOME\"".to_string());
    format!(
        "_codex_cwd={cwd_expr}\n\
         mkdir -p -- \"$_codex_cwd\" && cd -P -- \"$_codex_cwd\" && printf '%s' \"$PWD\""
    )
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
    validate_addressing_mode(&args)?;

    // --- Priority 1: relative mode (extend is present) -----------------------
    if let Some(extend_arg) = args.extend {
        let extend_hop = hop_from_arg(extend_arg)?;

        // Determine the base launcher.
        let base_launcher: RemoteLauncher = if let Some(base_id) = args.base {
            if !base_environment_visible(session, turn, &base_id) {
                return Err(FunctionCallError::RespondToModel(format!(
                    "env_switch: `base` environment id `{base_id}` is not visible to this thread; run env_status to list available environment ids"
                )));
            }
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
            implicit_base_launcher(session, turn).ok_or_else(|| {
                    FunctionCallError::RespondToModel(
                        "env_switch: relative mode requires a `base` or a previously-activated \
                         remote environment on this thread or its parent thread, but none was found. \
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
        "local" => handle_local_switch(session, turn, args.cwd).await,
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

fn validate_addressing_mode(args: &EnvSwitchArgs) -> Result<(), FunctionCallError> {
    let has_target = args
        .target
        .as_deref()
        .is_some_and(|target| !target.is_empty());
    let has_hops = args.hops.is_some();
    let has_extend = args.extend.is_some();
    let mode_count = [has_target, has_hops, has_extend]
        .into_iter()
        .filter(|present| *present)
        .count();

    if mode_count == 0 {
        return Err(FunctionCallError::RespondToModel(
            "env_switch: specify exactly one addressing mode: `target`, `hops`, or `extend`"
                .to_string(),
        ));
    }
    if mode_count > 1 {
        return Err(FunctionCallError::RespondToModel(
            "env_switch: `target`, `hops`, and `extend` are mutually exclusive addressing modes"
                .to_string(),
        ));
    }
    if args.base.is_some() && !has_extend {
        return Err(FunctionCallError::RespondToModel(
            "env_switch: `base` is only valid with relative mode (`extend`)".to_string(),
        ));
    }
    if has_extend && (args.host.is_some() || args.container.is_some()) {
        return Err(FunctionCallError::RespondToModel(
            "env_switch: top-level `host` and `container` are not valid with `extend`; put the value inside the `extend` object"
                .to_string(),
        ));
    }
    if has_hops && (args.host.is_some() || args.container.is_some()) {
        return Err(FunctionCallError::RespondToModel(
            "env_switch: top-level `host` and `container` are not valid with `hops`; put values inside each hop object"
                .to_string(),
        ));
    }
    if matches!(args.target.as_deref(), Some("local"))
        && (args.host.is_some() || args.container.is_some())
    {
        return Err(FunctionCallError::RespondToModel(
            "env_switch: `host` and `container` are not valid when target is `local`".to_string(),
        ));
    }
    if matches!(args.target.as_deref(), Some("docker")) && args.host.is_some() {
        return Err(FunctionCallError::RespondToModel(
            "env_switch: `host` is not valid when target is `docker`".to_string(),
        ));
    }
    if matches!(args.target.as_deref(), Some("ssh")) && args.container.is_some() {
        return Err(FunctionCallError::RespondToModel(
            "env_switch: `container` is not valid when target is `ssh`".to_string(),
        ));
    }
    Ok(())
}

/// Returns a message explaining that the host environment is the default.
///
/// This updates the env_switch runtime cursor and clears the non-local badge.
/// The stored thread environment configuration remains unchanged.
async fn handle_local_switch(
    session: &Arc<Session>,
    _turn: &Arc<TurnContext>,
    explicit_cwd: Option<String>,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    if explicit_cwd.as_deref().is_some_and(|cwd| !cwd.is_empty()) {
        return Err(FunctionCallError::RespondToModel(
            "env_switch: `cwd` is only valid for remote targets; omit `cwd` when target is `local`"
                .to_string(),
        ));
    }
    if session
        .services
        .environment_manager
        .try_local_environment()
        .is_none()
    {
        return Err(FunctionCallError::RespondToModel(
            "env_switch: the local host environment is not registered in this session; \
             use the default execution environment or start Codex with local environment support."
                .to_string(),
        ));
    }
    let thread_key = session.thread_id.to_string();
    session
        .services
        .environment_manager
        .clear_last_launcher(&thread_key);
    session
        .services
        .environment_manager
        .set_last_environment_id(thread_key.clone(), LOCAL_ENVIRONMENT_ID.to_string());
    session
        .services
        .environment_manager
        .record_thread_environment_id(thread_key, LOCAL_ENVIRONMENT_ID.to_string());
    session
        .emit_dynamic_environment_badge(LOCAL_ENVIRONMENT_ID)
        .await;
    let message = format!(
        "env_switch complete: the local host environment is available (id: `{LOCAL_ENVIRONMENT_ID}`). \
         It is now the default execution environment for compatible exec_command / apply_patch / view_image calls that omit `environment_id`. \
         Pass another `environment_id` explicitly to target a different registered environment for one call."
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
/// External sandbox enforcement is rejected before this function is called
/// because the remote exec-server cannot inherit the external sandbox.
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
        PermissionProfile::External { .. } => "workspace-write",
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
    if matches!(
        turn.permission_profile,
        codex_protocol::models::PermissionProfile::External { .. }
    ) {
        return Err(FunctionCallError::RespondToModel(
            "env_switch: remote environment switching is unavailable under an external sandbox because the remote exec-server cannot inherit that sandbox enforcement"
                .to_string(),
        ));
    }
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
    let remote_cwd: String = {
        let script = resolve_remote_cwd_script(explicit_cwd.as_deref());
        let (ok, cwd_out, err) = run_remote(&launcher, &script).await;
        if !ok {
            let requested = explicit_cwd.as_deref().unwrap_or("$HOME");
            return Err(FunctionCallError::RespondToModel(format!(
                "env_switch: could not resolve remote cwd `{requested}`: {err}"
            )));
        }
        if cwd_out.is_empty() {
            return Err(FunctionCallError::RespondToModel(
                "env_switch: remote cwd resolver returned empty output".to_string(),
            ));
        }
        cwd_out
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
    let metadata = EnvironmentMetadata {
        cwd: remote_cwd.clone(),
        shell: provisioned.shell.clone(),
    };
    session
        .services
        .environment_manager
        .set_environment_metadata(environment_id.clone(), metadata.clone());

    let thread_key = session.thread_id.to_string();
    session
        .services
        .environment_manager
        .set_thread_environment_metadata(thread_key.clone(), environment_id.clone(), metadata);

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

    // Record the launcher after registration succeeds so relative mode never
    // inherits a launcher for an environment that failed to register.
    session
        .services
        .environment_manager
        .set_last_launcher(thread_key.clone(), launcher.clone());
    session
        .services
        .environment_manager
        .set_last_environment_id(thread_key.clone(), environment_id.clone());
    session
        .services
        .environment_manager
        .record_thread_environment_id(thread_key, environment_id.clone());

    // --- Step 6: emit a badge event for the new default target --------------------
    // Shows the thread's env_switch-selected default execution environment in
    // the TUI. The resolver uses the same environment id for compatible tool
    // calls that omit environment_id.
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
         It is now the default execution environment for compatible exec_command / apply_patch / view_image calls that omit `environment_id`. \
         Pass `\"environment_id\": \"{environment_id}\"` explicitly only when you need to override another default or be extra explicit. \
         You usually do not need to call env_switch again for the same target unless you want to refresh provisioning, change cwd, or recover a broken connection.\
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
