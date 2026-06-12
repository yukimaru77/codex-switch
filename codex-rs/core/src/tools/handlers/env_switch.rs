use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_exec_server::provision::RemoteLauncher;
use codex_exec_server::provision::VersionPolicy;
use codex_exec_server::provision::ensure_remote_codex;
use codex_exec_server::provision::posix_single_quote;
use codex_protocol::AgentPath;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_protocol::protocol::TurnEnvironmentSelections;
use codex_utils_absolute_path::AbsolutePathBuf;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;

use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::session::SessionSettingsUpdate;
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

/// Handler for the `env_switch` tool.
///
/// Migrates the agent's execution environment into a Docker container or SSH
/// host by:
///   1. Provisioning a matching codex exec-server binary on the remote if
///      absent (via [`ensure_remote_codex`]).
///   2. Registering a stdio-backed [`Environment`] in the shared
///      [`EnvironmentManager`].
///   3. Updating this thread's sticky [`TurnEnvironmentSelection`] so that
///      the next turn resolves to the new environment.
///   4. Scheduling a self-continuation turn + deferred interrupt so the
///      current turn ends cleanly before the new environment takes effect.
///
/// No process migration occurs; the Codex session itself stays on the host.
/// Only shell / file tool execution moves to the remote environment.
#[derive(Default)]
pub struct EnvSwitchHandler;

/// Arguments accepted by the `env_switch` tool.
#[derive(Deserialize)]
struct EnvSwitchArgs {
    target: String,
    container: Option<String>,
    host: Option<String>,
    cwd: Option<String>,
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
///
/// The `script` is passed through [`RemoteLauncher::shell_argv`], which
/// ensures it arrives as a single `sh -c` argument even over SSH (where all
/// trailing argv elements are concatenated by the transport).
async fn run_remote(launcher: &RemoteLauncher, script: &str) -> (bool, String, String) {
    let argv = launcher.shell_argv(script);
    let Some((program, rest)) = argv.split_first().map(|(p, r)| (p.clone(), r.to_vec())) else {
        return (false, String::new(), "empty argv".to_string());
    };
    match tokio::process::Command::new(&program)
        .args(&rest)
        .output()
        .await
    {
        Ok(out) => (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).trim().to_string(),
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ),
        Err(e) => (false, String::new(), e.to_string()),
    }
}

/// Core logic shared between all switch directions.
async fn handle_env_switch(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    args: EnvSwitchArgs,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    match args.target.as_str() {
        "local" => handle_local_switch(session, turn, args.cwd).await,
        "docker" => {
            let container = args.container.ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "env_switch: `container` is required when target is `docker`".to_string(),
                )
            })?;
            handle_remote_switch(
                session,
                turn,
                RemoteLauncher::Docker { container },
                args.cwd,
            )
            .await
        }
        "ssh" => {
            let host = args.host.ok_or_else(|| {
                FunctionCallError::RespondToModel(
                    "env_switch: `host` is required when target is `ssh`".to_string(),
                )
            })?;
            handle_remote_switch(session, turn, RemoteLauncher::Ssh { host }, args.cwd).await
        }
        other => Err(FunctionCallError::RespondToModel(format!(
            "env_switch: unsupported target `{other}`; valid values are `local`, `docker`, `ssh`"
        ))),
    }
}

/// Restores the thread's sticky environment to the local host.
async fn handle_local_switch(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    explicit_cwd: Option<String>,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    // After a remote switch `turn.cwd` points at the REMOTE working directory
    // (e.g. `/root`), which usually does not exist on the host. The codex
    // process itself never moved, so its current directory is the original
    // local working directory.
    let host_cwd = match explicit_cwd {
        Some(cwd) => AbsolutePathBuf::from_absolute_path_checked(&cwd).map_err(|e| {
            FunctionCallError::RespondToModel(format!(
                "env_switch: `cwd` must be an absolute path: {e}"
            ))
        })?,
        None => std::env::current_dir()
            .ok()
            .and_then(|dir| AbsolutePathBuf::from_absolute_path_checked(&dir).ok())
            .unwrap_or_else(|| {
                #[allow(deprecated)]
                turn.cwd.clone()
            }),
    };

    let mut selections: Vec<TurnEnvironmentSelection> = session
        .services
        .environment_manager
        .default_environment_ids()
        .into_iter()
        .map(|environment_id| TurnEnvironmentSelection {
            environment_id,
            cwd: host_cwd.clone(),
        })
        .collect();
    if selections.is_empty() {
        selections.push(TurnEnvironmentSelection {
            environment_id: LOCAL_ENVIRONMENT_ID.to_string(),
            cwd: host_cwd.clone(),
        });
    }

    set_thread_environments(session, host_cwd.clone(), selections).await?;
    session.emit_thread_settings_applied(turn).await;

    let message = "env_switch complete: reverted to the local host environment. \
        The change takes effect on your NEXT step — end this step immediately \
        (a one-line acknowledgement) and your task will resume on the host."
        .to_string();
    schedule_continuation(session, turn, message.clone()).await;
    Ok(boxed_tool_output(FunctionToolOutput::from_text(
        message,
        Some(true),
    )))
}

/// Provisions the remote codex exec-server and switches the thread's sticky
/// environment to the given Docker container or SSH host.
async fn handle_remote_switch(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    launcher: RemoteLauncher,
    explicit_cwd: Option<String>,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    // --- Step 1: provision the remote codex exec-server --------------------------
    // TODO: ensure_remote_codex is defined in exec-server/src/provision/ which is
    // being implemented by the parallel A-agent.  Once that file exists this will
    // compile.  The signature matches the design doc:
    //   pub async fn ensure_remote_codex(
    //       launcher: &RemoteLauncher, desired: &VersionPolicy,
    //   ) -> Result<ProvisionedCodex, ProvisionError>
    let provisioned = ensure_remote_codex(&launcher, &VersionPolicy::HostVersion)
        .await
        .map_err(|e| {
            FunctionCallError::RespondToModel(format!(
                "env_switch: failed to provision remote codex: {e}"
            ))
        })?;

    let codex_path = provisioned.codex_path.clone();
    let deployed_version = provisioned.version.clone();

    // --- Step 2: determine the effective cwd -------------------------------------
    // Use the caller-supplied cwd when present; otherwise probe the remote $HOME.
    let remote_cwd: String = if let Some(cwd) = explicit_cwd {
        cwd
    } else {
        // The probe already ran inside ensure_remote_codex; we need $HOME again.
        // Use a lightweight sh echo rather than a second full probe round-trip.
        let (ok, home, err) = run_remote(&launcher, "echo $HOME").await;
        if ok && !home.is_empty() {
            home
        } else {
            return Err(FunctionCallError::RespondToModel(format!(
                "env_switch: could not determine remote $HOME: {err}"
            )));
        }
    };

    // --- Step 3: ensure the target cwd exists ------------------------------------
    run_remote(&launcher, &format!("mkdir -p {remote_cwd}")).await;

    // --- Step 4: build the environment id and register the stdio environment ----
    let (environment_id, target_label) = match &launcher {
        RemoteLauncher::Docker { container } => (
            format!("docker:{container}"),
            format!("docker container `{container}`"),
        ),
        RemoteLauncher::Ssh { host } => (format!("ssh:{host}"), format!("ssh host `{host}`")),
    };

    // Build the argv that will start the exec-server process over stdio.
    //
    // For Docker the arguments are passed verbatim to execve, so we can supply
    // each token as a separate element.  For SSH all trailing elements are
    // joined with spaces by the transport and fed to the remote login shell, so
    // we must supply a single pre-quoted shell command string instead.
    //
    // The config values (danger-full-access, never) intentionally contain no
    // spaces, so they do not require additional quoting beyond the POSIX single-
    // quote wrapper that shell_argv applies to the whole script.
    let exec_argv: Vec<String> = match &launcher {
        RemoteLauncher::Docker { container } => vec![
            "docker".to_string(),
            "exec".to_string(),
            "-i".to_string(),
            container.clone(),
            codex_path.clone(),
            "exec-server".to_string(),
            "--listen".to_string(),
            "stdio".to_string(),
            "-c".to_string(),
            "sandbox_mode=danger-full-access".to_string(),
            "-c".to_string(),
            "approval_policy=never".to_string(),
        ],
        RemoteLauncher::Ssh { host } => {
            // Build the remote command as a plain shell word sequence.  None of
            // the tokens contain special characters, so no per-token quoting is
            // needed; posix_single_quote wraps the whole string safely.
            let remote_cmd = format!(
                "{} exec-server --listen stdio -c sandbox_mode=danger-full-access -c approval_policy=never",
                posix_single_quote(&codex_path),
            );
            vec![
                "ssh".to_string(),
                "-T".to_string(),
                host.clone(),
                remote_cmd,
            ]
        }
    };
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

    // --- Step 5: update this thread's sticky environment selection ---------------
    let abs_cwd = AbsolutePathBuf::from_absolute_path_checked(&remote_cwd).map_err(|e| {
        FunctionCallError::RespondToModel(format!(
            "env_switch: remote cwd `{remote_cwd}` is not an absolute path: {e}"
        ))
    })?;

    set_thread_environments(
        session,
        abs_cwd.clone(),
        vec![TurnEnvironmentSelection {
            environment_id: environment_id.clone(),
            cwd: abs_cwd,
        }],
    )
    .await?;
    session.emit_thread_settings_applied(turn).await;

    // --- Step 6: schedule a self-continuation turn so the new env takes effect --
    let message = format!(
        "env_switch complete: moved into {target_label} \
         (codex {deployed_version} at {codex_path}). \
         The change takes effect on your NEXT step — end this step immediately \
         (a one-line acknowledgement) and your task will resume inside the remote \
         environment. Do NOT call env_switch again until you want to change \
         environments."
    );
    schedule_continuation(session, turn, message.clone()).await;

    Ok(boxed_tool_output(FunctionToolOutput::from_text(
        message,
        Some(true),
    )))
}

/// Updates the thread's sticky [`TurnEnvironmentSelections`] via
/// [`Session::update_settings`].
async fn set_thread_environments(
    session: &Arc<Session>,
    legacy_fallback_cwd: AbsolutePathBuf,
    selections: Vec<TurnEnvironmentSelection>,
) -> Result<(), FunctionCallError> {
    let new_environments = TurnEnvironmentSelections::new(legacy_fallback_cwd, selections);
    session
        .update_settings(SessionSettingsUpdate {
            environments: Some(new_environments),
            ..Default::default()
        })
        .await
        .map_err(|e| {
            FunctionCallError::RespondToModel(format!(
                "env_switch: failed to update thread environments: {e}"
            ))
        })
}

/// Queues a self-addressed, turn-triggering inter-agent message so this thread
/// runs another turn after the current one ends.  The new turn re-resolves the
/// (just-updated) sticky environment, so the agent continues its task in the
/// new environment.
///
/// This mirrors the `continue_in_new_turn` pattern validated in the old
/// `handle_subagent_switch` implementation (env-switch-subagent branch, line
/// 776+).  The same mechanism works for root threads because `input_queue`,
/// `active_turn`, and `agent_control` are present on every `Session`.
async fn schedule_continuation(session: &Arc<Session>, turn: &Arc<TurnContext>, content: String) {
    // Defer mailbox delivery to the NEXT turn so the message is not drained
    // into the still-active turn (which runs on the OLD environment).
    session
        .input_queue
        .defer_mailbox_delivery_to_next_turn(&session.active_turn, &turn.sub_id)
        .await;

    let self_path = turn
        .session_source
        .get_agent_path()
        .unwrap_or_else(AgentPath::root);

    let communication = InterAgentCommunication::new(
        self_path.clone(),
        self_path,
        Vec::new(),
        content,
        /*trigger_turn*/ true,
    );
    session
        .input_queue
        .enqueue_mailbox_communication(communication)
        .await;

    // Fire the self-interrupt from a detached task so that the current tool
    // call can return first (awaiting here would deadlock because we are
    // inside the active turn).  The 200 ms delay gives the tool-call output
    // time to be recorded in history before the turn is aborted; the abort
    // path then restarts the deferred continuation.
    let agent_control = session.services.agent_control.clone();
    let thread_id = session.thread_id();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        if let Err(e) = agent_control.interrupt_agent(thread_id).await {
            tracing::warn!("env_switch: failed to interrupt turn for environment switch: {e}");
        }
    });
}

#[cfg(test)]
#[path = "env_switch_tests.rs"]
mod tests;
