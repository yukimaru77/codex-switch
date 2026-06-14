use std::collections::BTreeMap;
use std::collections::BTreeSet;

use codex_exec_server::EnvironmentSnapshot;
use serde::Serialize;

use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::default_tool_environment_id;
use crate::tools::handlers::env_status_spec::ENV_LIST_TOOL_NAME;
use crate::tools::handlers::env_status_spec::ENV_STATUS_TOOL_NAME;
use crate::tools::handlers::env_status_spec::create_env_list_tool;
use crate::tools::handlers::env_status_spec::create_env_status_tool;
use crate::tools::handlers::environment_thread_keys;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_tools::ToolName;
use codex_tools::ToolSpec;

#[derive(Default)]
pub struct EnvStatusHandler;

#[derive(Default)]
pub struct EnvListHandler;

#[derive(Clone, Debug, PartialEq, Eq)]
struct TurnEnvironmentStatus {
    cwd: String,
    shell: Option<String>,
    selected_for_turn: bool,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct EnvStatusOutput {
    default_execution_environment_id: Option<String>,
    last_env_switch_environment_id: Option<String>,
    environments: Vec<EnvironmentStatusEntry>,
    note: &'static str,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct EnvironmentStatusEntry {
    environment_id: String,
    kind: &'static str,
    is_manager_default: bool,
    is_default_execution_environment: bool,
    is_selected_for_turn: bool,
    is_last_env_switch: bool,
    cwd: Option<String>,
    cwd_source: &'static str,
    shell: Option<String>,
}

impl ToolExecutor<ToolInvocation> for EnvStatusHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(ENV_STATUS_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_env_status_tool()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move { handle_environment_status(invocation, ENV_STATUS_TOOL_NAME) })
    }
}

impl CoreToolRuntime for EnvStatusHandler {}

impl ToolExecutor<ToolInvocation> for EnvListHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(ENV_LIST_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_env_list_tool()
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(async move { handle_environment_status(invocation, ENV_LIST_TOOL_NAME) })
    }
}

impl CoreToolRuntime for EnvListHandler {}

fn handle_environment_status(
    invocation: ToolInvocation,
    tool_name: &'static str,
) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
    if !matches!(invocation.payload, ToolPayload::Function { .. }) {
        return Err(FunctionCallError::RespondToModel(format!(
            "{tool_name} handler received unsupported payload"
        )));
    }

    let output = build_env_status_output(&invocation.session, &invocation.turn);
    let content = serde_json::to_string(&output).map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "{tool_name}: failed to serialize environment status: {err}"
        ))
    })?;
    Ok(boxed_tool_output(FunctionToolOutput::from_text(
        content,
        Some(true),
    )))
}

fn build_env_status_output(session: &Session, turn: &TurnContext) -> EnvStatusOutput {
    let environment_manager = &session.services.environment_manager;
    let mut turn_environments = turn
        .environments
        .turn_environments
        .iter()
        .map(|environment| {
            (
                environment.environment_id.clone(),
                TurnEnvironmentStatus {
                    cwd: environment.cwd.to_string_lossy().into_owned(),
                    shell: environment.shell.clone(),
                    selected_for_turn: true,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let thread_keys = environment_thread_keys(session, turn);
    let last_env_switch_environment_id = thread_keys
        .iter()
        .find_map(|thread_key| environment_manager.get_last_environment_id(thread_key));
    let default_execution_environment_id = default_tool_environment_id(session, turn);
    let mut visible_environment_ids = turn_environments.keys().cloned().collect::<BTreeSet<_>>();
    for thread_key in &thread_keys {
        visible_environment_ids.extend(environment_manager.get_thread_environment_ids(thread_key));
    }
    if let Some(last_environment_id) = &last_env_switch_environment_id {
        visible_environment_ids.insert(last_environment_id.clone());
    }
    if visible_environment_ids.contains(LOCAL_ENVIRONMENT_ID)
        && !turn_environments.contains_key(LOCAL_ENVIRONMENT_ID)
        && environment_manager.try_local_environment().is_some()
    {
        // Local status fallback for sessions where local support exists in the
        // live manager but was not frozen into the turn environment list.
        #[allow(deprecated)]
        let cwd = turn.cwd.to_string_lossy().into_owned();
        turn_environments.insert(
            LOCAL_ENVIRONMENT_ID.to_string(),
            TurnEnvironmentStatus {
                cwd,
                shell: None,
                selected_for_turn: false,
            },
        );
    }
    let snapshots = environment_manager
        .environment_snapshots()
        .into_iter()
        .filter(|snapshot| visible_environment_ids.contains(&snapshot.environment_id))
        .map(|mut snapshot| {
            if let Some(metadata) = environment_manager
                .get_thread_environment_metadata_for_keys(&thread_keys, &snapshot.environment_id)
            {
                snapshot.metadata = Some(metadata);
            }
            snapshot
        })
        .collect();

    build_env_status_output_from_parts(
        snapshots,
        &turn_environments,
        default_execution_environment_id,
        last_env_switch_environment_id,
    )
}

fn build_env_status_output_from_parts(
    snapshots: Vec<EnvironmentSnapshot>,
    turn_environments: &BTreeMap<String, TurnEnvironmentStatus>,
    default_execution_environment_id: Option<String>,
    last_env_switch_environment_id: Option<String>,
) -> EnvStatusOutput {
    let mut environments = snapshots
        .into_iter()
        .map(|snapshot| {
            let turn_environment = turn_environments.get(&snapshot.environment_id);
            let metadata = snapshot.metadata;
            let is_default_execution_environment = default_execution_environment_id.as_deref()
                == Some(snapshot.environment_id.as_str());
            let is_last_env_switch =
                last_env_switch_environment_id.as_deref() == Some(snapshot.environment_id.as_str());
            let prefer_env_switch_metadata =
                (is_default_execution_environment || is_last_env_switch) && metadata.is_some();
            let (cwd, cwd_source) = match turn_environment {
                Some(turn_environment) if !prefer_env_switch_metadata => {
                    (Some(turn_environment.cwd.clone()), "turn")
                }
                None => metadata
                    .as_ref()
                    .map(|metadata| (Some(metadata.cwd.clone()), "env_switch_metadata"))
                    .unwrap_or((None, "unavailable")),
                Some(_) => metadata
                    .as_ref()
                    .map(|metadata| (Some(metadata.cwd.clone()), "env_switch_metadata"))
                    .unwrap_or((None, "unavailable")),
            };
            let shell = if prefer_env_switch_metadata {
                metadata
                    .as_ref()
                    .and_then(|metadata| metadata.shell.clone())
                    .or_else(|| {
                        turn_environment.and_then(|turn_environment| turn_environment.shell.clone())
                    })
            } else {
                turn_environment
                    .and_then(|turn_environment| turn_environment.shell.clone())
                    .or_else(|| {
                        metadata
                            .as_ref()
                            .and_then(|metadata| metadata.shell.clone())
                    })
            };

            EnvironmentStatusEntry {
                environment_id: snapshot.environment_id,
                kind: if snapshot.is_remote {
                    "remote"
                } else {
                    "local"
                },
                is_manager_default: snapshot.is_default,
                is_default_execution_environment,
                is_selected_for_turn: turn_environment
                    .is_some_and(|environment| environment.selected_for_turn),
                is_last_env_switch,
                cwd,
                cwd_source,
                shell,
            }
        })
        .collect::<Vec<_>>();
    environments.sort_by(|left, right| {
        let left_priority = environment_status_sort_priority(left);
        let right_priority = environment_status_sort_priority(right);
        left_priority
            .cmp(&right_priority)
            .then_with(|| left.environment_id.cmp(&right.environment_id))
    });

    EnvStatusOutput {
        default_execution_environment_id,
        last_env_switch_environment_id,
        environments,
        note: "This is read-only status. Compatible environment-aware tool calls that omit environment_id use default_execution_environment_id for this thread; pass a listed environment_id explicitly to target a different registered environment.",
    }
}

fn environment_status_sort_priority(entry: &EnvironmentStatusEntry) -> u8 {
    if entry.is_default_execution_environment {
        0
    } else if entry.is_last_env_switch {
        1
    } else {
        2
    }
}

#[cfg(test)]
#[path = "env_status_tests.rs"]
mod tests;
