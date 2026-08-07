pub(crate) mod apply_patch;
pub(crate) mod apply_patch_spec;
mod current_time;
mod dynamic;
mod env_status;
pub(crate) mod env_status_spec;
mod env_switch;
pub(crate) mod env_switch_spec;
pub(crate) mod extension_tools;
mod get_context_remaining;
pub(crate) mod get_context_remaining_spec;
mod list_available_plugins_to_install;
pub(crate) mod list_available_plugins_to_install_spec;
mod mcp;
mod mcp_resource;
pub(crate) mod mcp_resource_spec;
pub(crate) mod multi_agents;
pub(crate) mod multi_agents_common;
pub(crate) mod multi_agents_spec;
pub(crate) mod multi_agents_v2;
mod new_context_window;
pub(crate) mod new_context_window_spec;
mod plan;
pub(crate) mod plan_spec;
mod remote_command_advisory;
mod request_permissions;
mod request_plugin_install;
pub(crate) mod request_plugin_install_spec;
mod request_user_input;
pub(crate) mod request_user_input_spec;
mod shell;
pub(crate) mod shell_spec;
mod sleep;
mod test_sync;
pub(crate) mod test_sync_spec;
mod tool_search;
pub(crate) mod tool_search_spec;
pub(crate) mod unified_exec;
mod view_image;
pub(crate) mod view_image_spec;
mod wait_for_environment;

use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_sandboxing::policy_transforms::intersect_permission_profiles;
use codex_sandboxing::policy_transforms::merge_permission_profiles;
use codex_sandboxing::policy_transforms::normalize_additional_permissions;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_absolute_path::AbsolutePathBufGuard;
use serde::Deserialize;
use serde_json::Map;
use serde_json::Value;
use std::path::Path;

use crate::config::PermissionProfileSnapshot;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::function_tool::FunctionCallError;
use crate::sandboxing::SandboxPermissions;
use crate::session::session::Session;
use crate::session::turn_context::EnvironmentConfig;
use crate::session::turn_context::TurnContext;
use crate::session::turn_context::TurnEnvironment;
pub(crate) use crate::tools::code_mode::CodeModeExecuteHandler;
pub(crate) use crate::tools::code_mode::CodeModeWaitHandler;
pub use apply_patch::ApplyPatchHandler;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::TurnEnvironmentSelection;
pub use current_time::CurrentTimeHandler;
pub use dynamic::DynamicToolHandler;
pub use env_status::EnvListHandler;
pub use env_status::EnvStatusHandler;
pub use env_switch::EnvSwitchHandler;
pub use get_context_remaining::GetContextRemainingHandler;
pub use list_available_plugins_to_install::ListAvailablePluginsToInstallHandler;
pub use mcp::McpHandler;
pub use mcp_resource::ListMcpResourceTemplatesHandler;
pub use mcp_resource::ListMcpResourcesHandler;
pub use mcp_resource::ReadMcpResourceHandler;
pub use new_context_window::NewContextWindowHandler;
pub use plan::PlanHandler;
pub(crate) use remote_command_advisory::RemoteCommandAdvisoryOptions;
pub(crate) use remote_command_advisory::remote_command_advisory;
pub use request_permissions::RequestPermissionsHandler;
pub use request_plugin_install::RequestPluginInstallHandler;
pub use request_user_input::RequestUserInputHandler;
pub use shell::ShellCommandHandler;
pub(crate) use shell::ShellCommandHandlerOptions;
pub use sleep::SleepHandler;
pub use test_sync::TestSyncHandler;
pub(crate) use tool_search::ToolSearchHandlerCache;
pub use unified_exec::ExecCommandHandler;
pub(crate) use unified_exec::ExecCommandHandlerOptions;
pub use unified_exec::WriteStdinHandler;
pub use view_image::ViewImageHandler;
pub(crate) use wait_for_environment::WaitForEnvironmentHandler;
pub use wait_for_environment::WaitForEnvironmentToolConfig;

pub(crate) fn parse_arguments<T>(arguments: &str) -> Result<T, FunctionCallError>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str(arguments).map_err(|err| {
        FunctionCallError::RespondToModel(format!("failed to parse function arguments: {err}"))
    })
}

fn resolve_sandbox_permissions(
    sandbox_permissions: Option<SandboxPermissions>,
    justification: Option<&str>,
) -> Result<SandboxPermissions, FunctionCallError> {
    if justification.is_some() && sandbox_permissions.is_none() {
        return Err(FunctionCallError::RespondToModel(
            "`justification` requires an explicit `sandbox_permissions`; use `sandbox_permissions: \"require_escalated\"` for unsandboxed execution, or omit `justification`.".to_string(),
        ));
    }

    Ok(sandbox_permissions.unwrap_or_default())
}

fn updated_hook_command(updated_input: &Value) -> Result<&str, FunctionCallError> {
    updated_input
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "hook returned updatedInput without string field `command`".to_string(),
            )
        })
}

fn rewrite_function_arguments(
    arguments: &str,
    tool_name: &str,
    rewrite: impl FnOnce(&mut Map<String, Value>),
) -> Result<String, FunctionCallError> {
    let mut arguments: Value = parse_arguments(arguments)?;
    let Value::Object(arguments) = &mut arguments else {
        return Err(FunctionCallError::RespondToModel(format!(
            "{tool_name} arguments must be an object"
        )));
    };
    rewrite(arguments);
    serde_json::to_string(&arguments).map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "failed to serialize rewritten {tool_name} arguments: {err}"
        ))
    })
}

fn rewrite_function_string_argument(
    arguments: &str,
    tool_name: &str,
    field_name: &str,
    value: &str,
) -> Result<String, FunctionCallError> {
    rewrite_function_arguments(arguments, tool_name, |arguments| {
        arguments.insert(field_name.to_string(), Value::String(value.to_string()));
    })
}

fn parse_arguments_with_base_path<T>(
    arguments: &str,
    base_path: &AbsolutePathBuf,
) -> Result<T, FunctionCallError>
where
    T: for<'de> Deserialize<'de>,
{
    let _guard = AbsolutePathBufGuard::new(base_path);
    parse_arguments(arguments)
}

fn resolve_workdir_base_path(
    arguments: &str,
    default_cwd: &AbsolutePathBuf,
) -> Result<AbsolutePathBuf, FunctionCallError> {
    let arguments: Value = parse_arguments(arguments)?;
    Ok(arguments
        .get("workdir")
        .and_then(Value::as_str)
        .filter(|workdir| !workdir.is_empty())
        .map_or_else(|| default_cwd.clone(), |workdir| default_cwd.join(workdir)))
}

pub(crate) fn environment_thread_keys(session: &Session, turn: &TurnContext) -> Vec<String> {
    let mut keys = vec![session.thread_id.to_string()];
    if let Some(parent_thread_id) = turn.parent_thread_id
        && parent_thread_id != session.thread_id
    {
        keys.push(parent_thread_id.to_string());
    }
    keys
}

pub(crate) fn dynamic_environment_visible_to_thread(
    session: &Session,
    turn: &TurnContext,
    environment_id: &str,
) -> bool {
    environment_thread_keys(session, turn)
        .into_iter()
        .any(|thread_key| {
            session
                .services
                .environment_manager
                .get_thread_environment_ids(&thread_key)
                .iter()
                .any(|id| id == environment_id)
        })
}

fn last_env_switch_environment_id(session: &Session, turn: &TurnContext) -> Option<String> {
    environment_thread_keys(session, turn)
        .into_iter()
        .find_map(|thread_key| {
            session
                .services
                .environment_manager
                .get_last_environment_id(&thread_key)
        })
}

/// Looks up env_switch cwd/shell metadata for `environment_id`, preferring the
/// thread-scoped entry and falling back to the global entry.
///
/// The global fallback matters because both maps are size-capped: a burst of
/// registrations (e.g. many sub-agent threads) can evict the thread-scoped
/// entry while the global one survives, and falling back keeps resolution on
/// remote metadata instead of degrading to local turn values.
fn env_switch_metadata(
    session: &Session,
    turn: &TurnContext,
    environment_id: &str,
) -> Option<codex_exec_server::EnvironmentMetadata> {
    let thread_keys = environment_thread_keys(session, turn);
    let manager = &session.services.environment_manager;
    manager
        .get_thread_environment_metadata_for_keys(&thread_keys, environment_id)
        .or_else(|| manager.get_environment_metadata(environment_id))
}

fn dynamic_environment_defaults(
    turn: &TurnContext,
) -> (Vec<codex_utils_path_uri::PathUri>, EnvironmentConfig) {
    turn.environments.primary().map_or_else(
        || {
            (
                Vec::new(),
                EnvironmentConfig {
                    allow_login_shell: turn.config.permissions.allow_login_shell,
                    permission_profile: PermissionProfileSnapshot::legacy(
                        turn.permission_profile(),
                    ),
                },
            )
        },
        |primary| (primary.workspace_roots().to_vec(), primary.config.clone()),
    )
}

fn turn_environment_from_env_switch_metadata(
    session: &Session,
    turn: &TurnContext,
    environment_id: &str,
) -> Result<TurnEnvironment, FunctionCallError> {
    let Some(environment) = session
        .services
        .environment_manager
        .get_environment(environment_id)
    else {
        return Err(FunctionCallError::RespondToModel(format!(
            "unknown turn environment id `{environment_id}`"
        )));
    };
    // Retrieve cwd/shell from the shared EnvironmentManager metadata maps.
    // Metadata is populated by env_switch *before* the environment is
    // registered, so it is available as soon as get_environment() succeeds.
    let Some(meta) = env_switch_metadata(session, turn, environment_id) else {
        return Err(FunctionCallError::RespondToModel(format!(
            "environment `{environment_id}` is registered but missing cwd metadata; rerun env_switch for this target"
        )));
    };
    let cwd = AbsolutePathBuf::from_absolute_path_checked(&meta.cwd).map_err(|e| {
        FunctionCallError::RespondToModel(format!(
            "environment `{environment_id}` has invalid cwd metadata `{}`: {e}",
            meta.cwd
        ))
    })?;
    let shell = meta
        .shell
        .map(|shell| crate::shell::shell_for_remote_path(std::path::Path::new(&shell)));
    let (workspace_roots, config) = dynamic_environment_defaults(turn);
    Ok(TurnEnvironment::new(
        environment_id.to_string(),
        environment,
        codex_utils_path_uri::PathUri::from_abs_path(&cwd),
        workspace_roots,
        shell,
        config,
    ))
}

/// Resolve the environment to use for a tool call.
///
/// Resolution order:
/// 1. `environment_id` is `None` → use the most recent environment selected
///    through `env_switch` for this thread or parent thread, falling back to
///    the primary turn environment.
/// 2. `environment_id` is `Some(LOCAL_ENVIRONMENT_ID)` → return the frozen
///    local turn environment when present, otherwise synthesize a local
///    environment from the live manager when local support is configured.
/// 3. `environment_id` is `Some(id)`, `id` is visible to this thread, and
///    current `env_switch` metadata exists (thread-scoped or global) →
///    synthesize a `TurnEnvironment` using that metadata.
/// 4. `environment_id` is `Some(id)` and `id` is in `turn.environments.turn_environments` →
///    clone and return it.
/// 5. `environment_id` is `Some(id)`, not in `turn` but present in the live
///    `EnvironmentManager` and recorded as visible to the current or parent
///    thread → synthesize a `TurnEnvironment` using the cwd and shell recorded
///    by `env_switch`.
/// 6. Otherwise → "unknown turn environment id" error.
///
/// Returns an owned `TurnEnvironment` because synthesised values in case 4
/// have no backing storage in `turn`.
pub(crate) async fn resolve_tool_environment(
    session: &Session,
    turn: &TurnContext,
    environment_id: Option<&str>,
) -> Result<Option<TurnEnvironment>, FunctionCallError> {
    let implicit_environment_id;
    let implicit_from_env_switch;
    let env_id = match environment_id {
        Some(env_id) => {
            implicit_from_env_switch = false;
            env_id
        }
        None => {
            implicit_environment_id = default_tool_environment_id(session, turn);
            let Some(env_id) = implicit_environment_id.as_deref() else {
                return Ok(None);
            };
            implicit_from_env_switch =
                last_env_switch_environment_id(session, turn).as_deref() == Some(env_id);
            env_id
        }
    };

    // Special case: "local" must mean the host, not the primary environment.
    // In remote-primary sessions the primary environment may be remote.
    if env_id == LOCAL_ENVIRONMENT_ID {
        if let Some(found) = turn
            .environments
            .turn_environments()
            .find(|environment| environment.environment_id == LOCAL_ENVIRONMENT_ID)
        {
            return Ok(Some(found.clone()));
        }
        if let Some(environment) = session.services.environment_manager.try_local_environment() {
            // Local fallback preserves the historical turn cwd for sessions
            // where local exists in the manager but was not frozen into the
            // turn's environment list.
            #[allow(deprecated)]
            let cwd = turn.cwd.clone();
            let (workspace_roots, config) = dynamic_environment_defaults(turn);
            return Ok(Some(TurnEnvironment::new(
                LOCAL_ENVIRONMENT_ID.to_string(),
                environment,
                codex_utils_path_uri::PathUri::from_abs_path(&cwd),
                workspace_roots,
                None,
                config,
            )));
        }
        return Err(FunctionCallError::RespondToModel(
            "local host environment is not registered in this session".to_string(),
        ));
    }

    // If this call omitted environment_id and the default came from env_switch,
    // prefer current thread metadata over a frozen turn entry. The same remote
    // id can be re-selected later with a different cwd/shell, while the turn
    // snapshot remains fixed for the duration of the turn.
    if implicit_from_env_switch {
        return turn_environment_from_env_switch_metadata(session, turn, env_id).map(Some);
    }

    if dynamic_environment_visible_to_thread(session, turn, env_id)
        && env_switch_metadata(session, turn, env_id).is_some()
    {
        return turn_environment_from_env_switch_metadata(session, turn, env_id).map(Some);
    }

    // Fast path: id already in the frozen turn list.
    if let Some(found) = turn
        .environments
        .turn_environments()
        .find(|environment| environment.environment_id == env_id)
    {
        return Ok(Some(found.clone()));
    }

    if let Some(starting) = turn
        .environments
        .starting
        .iter()
        .find(|environment| environment.selection.environment_id == env_id)
    {
        return match starting.resolved() {
            Some(Ok(environment)) => Ok(Some(environment)),
            Some(Err(err)) => Err(FunctionCallError::RespondToModel(format!(
                "environment `{env_id}` failed to start: {err}"
            ))),
            None => Err(FunctionCallError::RespondToModel(format!(
                "environment `{env_id}` is still starting; call wait_for_environment first"
            ))),
        };
    }

    // Live fallback: look up through EnvironmentManager (for dynamically
    // registered environments, e.g. registered by env_switch in the same turn).
    // Only ids recorded for this thread or its parent are visible here; the
    // manager is shared process state and may contain unrelated thread ids.
    if !dynamic_environment_visible_to_thread(session, turn, env_id) {
        return Err(FunctionCallError::RespondToModel(format!(
            "unknown turn environment id `{env_id}`"
        )));
    }
    if session
        .services
        .environment_manager
        .get_environment(env_id)
        .is_some()
    {
        return turn_environment_from_env_switch_metadata(session, turn, env_id).map(Some);
    }

    Err(FunctionCallError::RespondToModel(format!(
        "unknown turn environment id `{env_id}`"
    )))
}

pub(crate) fn default_tool_environment_id(session: &Session, turn: &TurnContext) -> Option<String> {
    default_tool_environment_id_for_snapshot(session, turn, &turn.environments)
}

fn default_tool_environment_id_for_snapshot(
    session: &Session,
    turn: &TurnContext,
    environments: &TurnEnvironmentSnapshot,
) -> Option<String> {
    last_env_switch_environment_id(session, turn).or_else(|| {
        environments
            .primary()
            .map(|environment| environment.environment_id.clone())
            .or_else(|| {
                environments
                    .starting()
                    .next()
                    .map(|environment| environment.selection.environment_id.clone())
            })
    })
}

pub(crate) fn environment_selections_with_default(
    session: &Session,
    turn: &TurnContext,
    environments: &TurnEnvironmentSnapshot,
) -> Vec<TurnEnvironmentSelection> {
    let mut selections = environments.to_selections();
    let Some(default_environment_id) =
        default_tool_environment_id_for_snapshot(session, turn, environments)
    else {
        return selections;
    };

    if let Some(index) = selections
        .iter()
        .position(|selection| selection.environment_id == default_environment_id)
    {
        let mut default_selection = selections.remove(index);
        if default_environment_id != LOCAL_ENVIRONMENT_ID {
            let manager = &session.services.environment_manager;
            let thread_keys = environment_thread_keys(session, turn);
            if let Some(metadata) = manager
                .get_thread_environment_metadata_for_keys(&thread_keys, &default_environment_id)
                .or_else(|| manager.get_environment_metadata(&default_environment_id))
                && let Ok(cwd) = AbsolutePathBuf::from_absolute_path_checked(&metadata.cwd)
            {
                default_selection.cwd = codex_utils_path_uri::PathUri::from_abs_path(&cwd);
            }
        }
        selections.insert(0, default_selection);
        return selections;
    }

    let manager = &session.services.environment_manager;
    let cwd = if default_environment_id == LOCAL_ENVIRONMENT_ID {
        if manager.try_local_environment().is_none() {
            return selections;
        }
        #[allow(deprecated)]
        turn.cwd.clone()
    } else {
        if manager.get_environment(&default_environment_id).is_none() {
            return selections;
        }
        let thread_keys = environment_thread_keys(session, turn);
        let Some(metadata) = manager
            .get_thread_environment_metadata_for_keys(&thread_keys, &default_environment_id)
            .or_else(|| manager.get_environment_metadata(&default_environment_id))
        else {
            return selections;
        };
        let Ok(cwd) = AbsolutePathBuf::from_absolute_path_checked(&metadata.cwd) else {
            return selections;
        };
        cwd
    };

    selections.insert(
        0,
        TurnEnvironmentSelection {
            environment_id: default_environment_id,
            cwd: codex_utils_path_uri::PathUri::from_abs_path(&cwd),
        },
    );
    selections
}

/// Validates feature/policy constraints for `with_additional_permissions` and
/// normalizes any path-based permissions. Errors if the request is invalid.
pub(crate) fn normalize_and_validate_additional_permissions(
    additional_permissions_allowed: bool,
    approval_policy: AskForApproval,
    sandbox_permissions: SandboxPermissions,
    additional_permissions: Option<AdditionalPermissionProfile>,
    permissions_preapproved: bool,
    _cwd: &Path,
) -> Result<Option<AdditionalPermissionProfile>, String> {
    let uses_additional_permissions = matches!(
        sandbox_permissions,
        SandboxPermissions::WithAdditionalPermissions
    );

    if !permissions_preapproved
        && !additional_permissions_allowed
        && (uses_additional_permissions || additional_permissions.is_some())
    {
        return Err(
            "additional permissions are disabled; enable `features.exec_permission_approvals` before using `with_additional_permissions`"
                .to_string(),
        );
    }

    if uses_additional_permissions {
        if !permissions_preapproved && !matches!(approval_policy, AskForApproval::OnRequest) {
            return Err(format!(
                "approval policy is {approval_policy:?}; reject command — you cannot request additional permissions unless the approval policy is OnRequest"
            ));
        }
        let Some(additional_permissions) = additional_permissions else {
            return Err(
                "missing `additional_permissions`; provide at least one of `network` or `file_system` when using `with_additional_permissions`"
                    .to_string(),
            );
        };
        let normalized = normalize_additional_permissions(additional_permissions)?;
        if normalized.is_empty() {
            return Err(
                "`additional_permissions` must include at least one requested permission in `network` or `file_system`"
                    .to_string(),
            );
        }
        return Ok(Some(normalized));
    }

    if additional_permissions.is_some() {
        Err(
            "`additional_permissions` requires `sandbox_permissions` set to `with_additional_permissions`"
                .to_string(),
        )
    } else {
        Ok(None)
    }
}

pub(super) struct EffectiveAdditionalPermissions {
    pub sandbox_permissions: SandboxPermissions,
    pub additional_permissions: Option<AdditionalPermissionProfile>,
    pub permissions_preapproved: bool,
}

pub(super) fn implicit_granted_permissions(
    sandbox_permissions: SandboxPermissions,
    additional_permissions: Option<&AdditionalPermissionProfile>,
    effective_additional_permissions: &EffectiveAdditionalPermissions,
) -> Option<AdditionalPermissionProfile> {
    if !sandbox_permissions.uses_additional_permissions()
        && !matches!(sandbox_permissions, SandboxPermissions::RequireEscalated)
        && additional_permissions.is_none()
    {
        effective_additional_permissions
            .additional_permissions
            .clone()
    } else {
        None
    }
}

pub(super) async fn apply_granted_turn_permissions(
    session: &Session,
    environment_id: &str,
    cwd: &Path,
    sandbox_permissions: SandboxPermissions,
    additional_permissions: Option<AdditionalPermissionProfile>,
) -> EffectiveAdditionalPermissions {
    if matches!(sandbox_permissions, SandboxPermissions::RequireEscalated) {
        return EffectiveAdditionalPermissions {
            sandbox_permissions,
            additional_permissions,
            permissions_preapproved: false,
        };
    }

    let granted_session_permissions = session.granted_session_permissions(environment_id).await;
    let granted_turn_permissions = session.granted_turn_permissions(environment_id).await;
    let granted_permissions = merge_permission_profiles(
        granted_session_permissions.as_ref(),
        granted_turn_permissions.as_ref(),
    );
    let effective_permissions = merge_permission_profiles(
        additional_permissions.as_ref(),
        granted_permissions.as_ref(),
    );
    let permissions_preapproved = match (effective_permissions.as_ref(), granted_permissions) {
        (Some(effective_permissions), Some(granted_permissions)) => {
            permissions_are_preapproved(effective_permissions, granted_permissions, cwd)
        }
        _ => false,
    };

    let sandbox_permissions =
        if effective_permissions.is_some() && !sandbox_permissions.uses_additional_permissions() {
            SandboxPermissions::WithAdditionalPermissions
        } else {
            sandbox_permissions
        };

    EffectiveAdditionalPermissions {
        sandbox_permissions,
        additional_permissions: effective_permissions,
        permissions_preapproved,
    }
}

fn permissions_are_preapproved(
    effective_permissions: &AdditionalPermissionProfile,
    granted_permissions: AdditionalPermissionProfile,
    cwd: &Path,
) -> bool {
    let materialized_effective_permissions = intersect_permission_profiles(
        effective_permissions.clone(),
        effective_permissions.clone(),
        cwd,
    );
    intersect_permission_profiles(effective_permissions.clone(), granted_permissions, cwd)
        == materialized_effective_permissions
}

#[cfg(test)]
mod tests {
    use super::EffectiveAdditionalPermissions;
    use super::environment_selections_with_default;
    use super::implicit_granted_permissions;
    use super::normalize_and_validate_additional_permissions;
    use super::permissions_are_preapproved;
    use super::resolve_tool_environment;
    use crate::sandboxing::SandboxPermissions;
    use crate::session::turn_context::TurnEnvironment;
    use codex_exec_server::Environment;
    use codex_exec_server::EnvironmentMetadata;
    use codex_exec_server::LOCAL_ENVIRONMENT_ID;
    use codex_protocol::models::AdditionalPermissionProfile;
    use codex_protocol::models::FileSystemPermissions;
    use codex_protocol::models::NetworkPermissions;
    use codex_protocol::permissions::FileSystemAccessMode;
    use codex_protocol::permissions::FileSystemPath;
    use codex_protocol::permissions::FileSystemSandboxEntry;
    use codex_protocol::permissions::FileSystemSpecialPath;
    use codex_protocol::protocol::AskForApproval;
    use codex_protocol::protocol::GranularApprovalConfig;
    use codex_sandboxing::policy_transforms::intersect_permission_profiles;
    use codex_sandboxing::policy_transforms::merge_permission_profiles;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn network_permissions() -> AdditionalPermissionProfile {
        AdditionalPermissionProfile {
            network: Some(NetworkPermissions {
                enabled: Some(true),
            }),
            ..Default::default()
        }
    }

    fn file_system_permissions(path: &std::path::Path) -> AdditionalPermissionProfile {
        AdditionalPermissionProfile {
            file_system: Some(FileSystemPermissions::from_read_write_roots(
                /*read*/ None,
                Some(vec![
                    AbsolutePathBuf::from_absolute_path(path).expect("absolute path"),
                ]),
            )),
            ..Default::default()
        }
    }

    #[test]
    fn preapproved_permissions_work_when_request_permissions_tool_is_enabled_without_exec_permission_approvals_feature()
     {
        let cwd = tempdir().expect("tempdir");

        let normalized = normalize_and_validate_additional_permissions(
            /*additional_permissions_allowed*/ false,
            AskForApproval::Granular(GranularApprovalConfig {
                sandbox_approval: true,
                rules: true,
                skill_approval: true,
                request_permissions: false,
                mcp_elicitations: true,
            }),
            SandboxPermissions::WithAdditionalPermissions,
            Some(network_permissions()),
            /*permissions_preapproved*/ true,
            cwd.path(),
        )
        .expect("preapproved permissions should be allowed");

        assert_eq!(normalized, Some(network_permissions()));
    }

    #[test]
    fn fresh_additional_permissions_still_require_exec_permission_approvals_feature() {
        let cwd = tempdir().expect("tempdir");

        let err = normalize_and_validate_additional_permissions(
            /*additional_permissions_allowed*/ false,
            AskForApproval::OnRequest,
            SandboxPermissions::WithAdditionalPermissions,
            Some(network_permissions()),
            /*permissions_preapproved*/ false,
            cwd.path(),
        )
        .expect_err("fresh inline permission requests should remain disabled");

        assert_eq!(
            err,
            "additional permissions are disabled; enable `features.exec_permission_approvals` before using `with_additional_permissions`"
        );
    }

    #[tokio::test]
    async fn explicit_local_environment_resolves_to_host_even_when_primary_is_remote() {
        let (session, mut turn) = crate::session::tests::make_session_and_context().await;
        let local = turn
            .environments
            .turn_environments
            .first()
            .expect("local turn environment")
            .clone();
        let remote = TurnEnvironment::new(
            "remote".to_string(),
            Arc::new(
                Environment::create_for_tests(Some("ws://127.0.0.1:8765".to_string()))
                    .expect("remote environment"),
            ),
            local.cwd().clone(),
            None,
            local.config.clone(),
        );
        turn.environments.turn_environments = vec![remote, local];

        let resolved = resolve_tool_environment(&session, &turn, Some(LOCAL_ENVIRONMENT_ID))
            .await
            .expect("local should resolve")
            .expect("local environment");

        assert_eq!(resolved.environment_id, LOCAL_ENVIRONMENT_ID);
        assert!(!resolved.environment.is_remote());
    }

    #[tokio::test]
    async fn explicit_local_environment_resolves_from_manager_when_missing_from_turn() {
        let (session, mut turn) = crate::session::tests::make_session_and_context().await;
        let local_cwd = turn
            .environments
            .primary()
            .expect("primary environment")
            .cwd()
            .clone();
        turn.environments.turn_environments = Vec::new();

        let resolved = resolve_tool_environment(&session, &turn, Some(LOCAL_ENVIRONMENT_ID))
            .await
            .expect("local should resolve")
            .expect("local environment");

        assert_eq!(resolved.environment_id, LOCAL_ENVIRONMENT_ID);
        assert_eq!(resolved.cwd(), &local_cwd);
        assert!(!resolved.environment.is_remote());
    }

    #[tokio::test]
    async fn explicit_environment_uses_global_metadata_when_thread_metadata_is_missing() {
        let (session, turn) = crate::session::tests::make_session_and_context().await;
        let thread_key = session.thread_id.to_string();
        let manager = &session.services.environment_manager;
        manager
            .upsert_environment(
                "ssh:mine".to_string(),
                "ws://127.0.0.1:8765".to_string(),
                None,
            )
            .expect("seed remote environment");
        // Only global metadata exists: the thread-scoped entry may have been
        // evicted by the metadata size cap or written by another thread.
        manager.set_environment_metadata(
            "ssh:mine".to_string(),
            EnvironmentMetadata {
                cwd: "/remote/home".to_string(),
                shell: Some("/nonexistent/remote/bin/bash".to_string()),
            },
        );
        manager.record_thread_environment_id(thread_key, "ssh:mine".to_string());

        let resolved = resolve_tool_environment(&session, &turn, Some("ssh:mine"))
            .await
            .expect("resolution should succeed")
            .expect("environment should resolve");

        assert_eq!(resolved.environment_id, "ssh:mine");
        assert_eq!(
            resolved.cwd().to_abs_path().expect("absolute cwd"),
            AbsolutePathBuf::from_absolute_path("/remote/home").expect("remote cwd")
        );
        // The recorded remote shell path must be preserved verbatim, not
        // re-resolved against the local filesystem.
        assert_eq!(
            resolved
                .shell
                .as_ref()
                .map(|shell| shell.shell_path.as_path()),
            Some(std::path::Path::new("/nonexistent/remote/bin/bash"))
        );
    }

    #[tokio::test]
    async fn environment_selections_with_default_materializes_env_switch_default_first() {
        let (session, turn) = crate::session::tests::make_session_and_context().await;
        let thread_key = session.thread_id.to_string();
        let manager = &session.services.environment_manager;
        manager
            .upsert_environment(
                "ssh:mine".to_string(),
                "ws://127.0.0.1:8765".to_string(),
                None,
            )
            .expect("seed remote environment");
        manager.set_thread_environment_metadata(
            thread_key.clone(),
            "ssh:mine".to_string(),
            EnvironmentMetadata {
                cwd: "/mine".to_string(),
                shell: Some("/bin/bash".to_string()),
            },
        );
        manager.record_thread_environment_id(thread_key.clone(), "ssh:mine".to_string());
        manager.set_last_environment_id(thread_key, "ssh:mine".to_string());

        let selections = environment_selections_with_default(&session, &turn, &turn.environments);

        assert_eq!(
            selections
                .first()
                .map(|selection| selection.environment_id.as_str()),
            Some("ssh:mine")
        );
        assert_eq!(
            selections[0].cwd.to_abs_path().expect("absolute cwd"),
            AbsolutePathBuf::from_absolute_path("/mine").expect("mine cwd")
        );
        assert!(
            selections
                .iter()
                .any(|selection| selection.environment_id == LOCAL_ENVIRONMENT_ID),
            "existing local selection should be preserved after the remote default"
        );
    }

    #[tokio::test]
    async fn environment_selections_with_default_refreshes_existing_default_cwd() {
        let (session, mut turn) = crate::session::tests::make_session_and_context().await;
        let thread_key = session.thread_id.to_string();
        let manager = &session.services.environment_manager;
        manager
            .upsert_environment(
                "ssh:mine".to_string(),
                "ws://127.0.0.1:8765".to_string(),
                None,
            )
            .expect("seed remote environment");
        let environment_config = turn
            .environments
            .primary()
            .expect("primary environment")
            .config()
            .clone();
        turn.environments
            .environments
            .push(TurnEnvironmentState::Ready(TurnEnvironment::new(
                TurnEnvironmentSelection {
                    environment_id: "ssh:mine".to_string(),
                    cwd: AbsolutePathBuf::from_absolute_path("/old")
                        .expect("old cwd")
                        .into(),
                    config: EnvironmentConfigState::Ready(environment_config),
                },
                EnvironmentConfigOrigin::Thread,
                Arc::new(
                    Environment::create_for_tests(Some("ws://127.0.0.1:8765".to_string()))
                        .expect("remote environment"),
                ),
                None,
            )));
        manager.set_environment_metadata(
            "ssh:mine".to_string(),
            EnvironmentMetadata {
                cwd: "/global".to_string(),
                shell: Some("/bin/bash".to_string()),
            },
        );
        manager.set_thread_environment_metadata(
            thread_key.clone(),
            "ssh:mine".to_string(),
            EnvironmentMetadata {
                cwd: "/thread".to_string(),
                shell: Some("/bin/sh".to_string()),
            },
        );
        manager.record_thread_environment_id(thread_key.clone(), "ssh:mine".to_string());
        manager.set_last_environment_id(thread_key, "ssh:mine".to_string());

        let selections = environment_selections_with_default(&session, &turn, &turn.environments);

        assert_eq!(
            selections
                .first()
                .map(|selection| selection.environment_id.as_str()),
            Some("ssh:mine")
        );
        assert_eq!(
            selections[0].cwd.to_abs_path().expect("absolute cwd"),
            AbsolutePathBuf::from_absolute_path("/thread").expect("thread cwd")
        );
    }

    #[test]
    fn implicit_sticky_grants_bypass_inline_permission_validation() {
        let cwd = tempdir().expect("tempdir");
        let granted_permissions = file_system_permissions(cwd.path());
        let implicit_permissions = implicit_granted_permissions(
            SandboxPermissions::UseDefault,
            /*additional_permissions*/ None,
            &EffectiveAdditionalPermissions {
                sandbox_permissions: SandboxPermissions::WithAdditionalPermissions,
                additional_permissions: Some(granted_permissions.clone()),
                permissions_preapproved: false,
            },
        );

        assert_eq!(implicit_permissions, Some(granted_permissions));
    }

    #[test]
    fn explicit_inline_permissions_do_not_use_implicit_sticky_grant_path() {
        let cwd = tempdir().expect("tempdir");
        let requested_permissions = file_system_permissions(cwd.path());
        let implicit_permissions = implicit_granted_permissions(
            SandboxPermissions::WithAdditionalPermissions,
            Some(&requested_permissions),
            &EffectiveAdditionalPermissions {
                sandbox_permissions: SandboxPermissions::WithAdditionalPermissions,
                additional_permissions: Some(requested_permissions.clone()),
                permissions_preapproved: false,
            },
        );

        assert_eq!(implicit_permissions, None);
    }

    #[test]
    fn relative_deny_glob_grants_remain_preapproved_after_materialization() {
        let cwd = tempdir().expect("tempdir");
        let requested_permissions = AdditionalPermissionProfile {
            file_system: Some(FileSystemPermissions {
                entries: vec![
                    FileSystemSandboxEntry {
                        path: FileSystemPath::Special {
                            value: FileSystemSpecialPath::project_roots(/*subpath*/ None),
                        },
                        access: FileSystemAccessMode::Write,
                        missing_path_behavior: None,
                    },
                    FileSystemSandboxEntry {
                        path: FileSystemPath::GlobPattern {
                            pattern: "**/*.env".to_string(),
                        },
                        access: FileSystemAccessMode::Deny,
                        missing_path_behavior: None,
                    },
                ],
                glob_scan_max_depth: None,
            }),
            ..Default::default()
        };
        let stored_grant = intersect_permission_profiles(
            requested_permissions.clone(),
            requested_permissions.clone(),
            cwd.path(),
        );
        let effective_permissions =
            merge_permission_profiles(Some(&requested_permissions), Some(&stored_grant))
                .expect("merged permissions");

        assert!(permissions_are_preapproved(
            &effective_permissions,
            stored_grant,
            cwd.path(),
        ));
    }
}
