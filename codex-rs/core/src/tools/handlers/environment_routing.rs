use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_protocol::protocol::EnvironmentConfig;
use codex_protocol::protocol::EnvironmentConfigState;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_utils_absolute_path::AbsolutePathBuf;

use crate::config::PermissionProfileSnapshot;
use crate::environment_selection::EnvironmentConfigOrigin;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::session::turn_context::TurnEnvironment;

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
                    workspace_roots: Vec::new(),
                    permission_profile: PermissionProfileSnapshot::legacy(
                        turn.permission_profile(),
                    ),
                    shell_environment_policy: Default::default(),
                    windows_sandbox_level: turn.windows_sandbox_level,
                    windows_sandbox_private_desktop: turn
                        .config
                        .permissions
                        .windows_sandbox_private_desktop,
                    use_legacy_landlock: turn.config.features.use_legacy_landlock(),
                    exec_policy: None,
                    mcp_policy: None,
                    network_policy: None,
                    selected_capability_roots: Vec::new(),
                },
            )
        },
        |primary| (primary.workspace_roots().to_vec(), primary.config().clone()),
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
        TurnEnvironmentSelection {
            environment_id: environment_id.to_string(),
            cwd: codex_utils_path_uri::PathUri::from_abs_path(&cwd),
            workspace_roots,
            config: EnvironmentConfigState::Ready(config),
        },
        EnvironmentConfigOrigin::Thread,
        environment,
        shell,
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
            .find(|environment| environment.selection.environment_id == LOCAL_ENVIRONMENT_ID)
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
                TurnEnvironmentSelection {
                    environment_id: LOCAL_ENVIRONMENT_ID.to_string(),
                    cwd: codex_utils_path_uri::PathUri::from_abs_path(&cwd),
                    workspace_roots,
                    config: EnvironmentConfigState::Ready(config),
                },
                EnvironmentConfigOrigin::Thread,
                environment,
                None,
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
        .find(|environment| environment.selection.environment_id == env_id)
    {
        return Ok(Some(found.clone()));
    }

    if let Some(starting) = turn
        .environments
        .starting()
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
            .map(|environment| environment.selection.environment_id.clone())
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

    let Some(primary_environment) = turn.environments.primary() else {
        return selections;
    };
    selections.insert(
        0,
        TurnEnvironmentSelection {
            environment_id: default_environment_id,
            cwd: codex_utils_path_uri::PathUri::from_abs_path(&cwd),
            workspace_roots: Vec::new(),
            config: EnvironmentConfigState::Ready(primary_environment.config().clone()),
        },
    );
    selections
}
