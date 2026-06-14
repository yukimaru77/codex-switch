use std::collections::BTreeMap;

use codex_exec_server::EnvironmentMetadata;
use codex_exec_server::EnvironmentSnapshot;
use codex_protocol::ThreadId;
use codex_tools::ToolSpec;
use pretty_assertions::assert_eq;

use crate::tools::handlers::env_status_spec::ENV_LIST_TOOL_NAME;
use crate::tools::handlers::env_status_spec::ENV_STATUS_TOOL_NAME;
use crate::tools::handlers::env_status_spec::create_env_list_tool;
use crate::tools::handlers::env_status_spec::create_env_status_tool;
use crate::tools::registry::ToolExecutor;

use super::EnvListHandler;
use super::EnvStatusHandler;
use super::EnvStatusOutput;
use super::EnvironmentStatusEntry;
use super::TurnEnvironmentStatus;
use super::build_env_status_output;
use super::build_env_status_output_from_parts;

const STATUS_NOTE: &str = "This is read-only status. Compatible environment-aware tool calls that omit environment_id use default_execution_environment_id for this thread; pass a listed environment_id explicitly to target a different registered environment.";

#[test]
fn env_status_and_env_list_tool_names_are_correct() {
    let status = EnvStatusHandler;
    assert_eq!(status.tool_name().name, ENV_STATUS_TOOL_NAME);
    assert!(status.tool_name().namespace.is_none());

    let list = EnvListHandler;
    assert_eq!(list.tool_name().name, ENV_LIST_TOOL_NAME);
    assert!(list.tool_name().namespace.is_none());
}

#[test]
fn specs_are_no_arg_functions_and_describe_read_only_status() {
    for spec in [create_env_status_tool(), create_env_list_tool()] {
        let ToolSpec::Function(tool) = spec else {
            panic!("expected function spec");
        };
        assert!(!tool.strict);
        assert_eq!(tool.parameters.properties.unwrap_or_default().len(), 0);
        assert!(tool.parameters.required.unwrap_or_default().is_empty());
        assert_eq!(tool.parameters.additional_properties, Some(false.into()));

        assert!(
            tool.description.contains("context compaction"),
            "description should mention compaction recovery"
        );
        assert!(
            tool.description.contains("does not switch environments"),
            "description should say the tool is read-only"
        );
        assert!(
            tool.description.contains("environment_id"),
            "description should tell the model how to use the listed ids"
        );
    }
}

#[test]
fn output_merges_turn_cwd_and_env_switch_metadata() {
    let snapshots = vec![
        EnvironmentSnapshot {
            environment_id: "local".to_string(),
            is_remote: false,
            is_default: true,
            metadata: None,
        },
        EnvironmentSnapshot {
            environment_id: "ssh:hostname".to_string(),
            is_remote: true,
            is_default: false,
            metadata: Some(EnvironmentMetadata {
                cwd: "/remote/work".to_string(),
                shell: Some("/bin/bash".to_string()),
            }),
        },
    ];
    let turn_environments = BTreeMap::from([(
        "local".to_string(),
        TurnEnvironmentStatus {
            cwd: "/home/project".to_string(),
            shell: Some("/bin/zsh".to_string()),
            selected_for_turn: true,
        },
    )]);

    let output = build_env_status_output_from_parts(
        snapshots,
        &turn_environments,
        Some("ssh:hostname".to_string()),
        Some("ssh:hostname".to_string()),
    );

    assert_eq!(
        output,
        EnvStatusOutput {
            default_execution_environment_id: Some("ssh:hostname".to_string()),
            last_env_switch_environment_id: Some("ssh:hostname".to_string()),
            environments: vec![
                EnvironmentStatusEntry {
                    environment_id: "ssh:hostname".to_string(),
                    kind: "remote",
                    is_manager_default: false,
                    is_default_execution_environment: true,
                    is_selected_for_turn: false,
                    is_last_env_switch: true,
                    cwd: Some("/remote/work".to_string()),
                    cwd_source: "env_switch_metadata",
                    shell: Some("/bin/bash".to_string()),
                },
                EnvironmentStatusEntry {
                    environment_id: "local".to_string(),
                    kind: "local",
                    is_manager_default: true,
                    is_default_execution_environment: false,
                    is_selected_for_turn: true,
                    is_last_env_switch: false,
                    cwd: Some("/home/project".to_string()),
                    cwd_source: "turn",
                    shell: Some("/bin/zsh".to_string()),
                },
            ],
            note: STATUS_NOTE,
        }
    );
}

#[test]
fn output_marks_unknown_cwd_as_unavailable() {
    let snapshots = vec![EnvironmentSnapshot {
        environment_id: "remote-without-metadata".to_string(),
        is_remote: true,
        is_default: false,
        metadata: None,
    }];
    let output = build_env_status_output_from_parts(snapshots, &BTreeMap::new(), None, None);

    assert_eq!(
        output.environments,
        vec![EnvironmentStatusEntry {
            environment_id: "remote-without-metadata".to_string(),
            kind: "remote",
            is_manager_default: false,
            is_default_execution_environment: false,
            is_selected_for_turn: false,
            is_last_env_switch: false,
            cwd: None,
            cwd_source: "unavailable",
            shell: None,
        }]
    );
}

#[test]
fn output_prefers_env_switch_metadata_for_current_default_even_when_turn_has_entry() {
    let snapshots = vec![EnvironmentSnapshot {
        environment_id: "ssh:hostname".to_string(),
        is_remote: true,
        is_default: false,
        metadata: Some(EnvironmentMetadata {
            cwd: "/new".to_string(),
            shell: Some("/bin/sh".to_string()),
        }),
    }];
    let turn_environments = BTreeMap::from([(
        "ssh:hostname".to_string(),
        TurnEnvironmentStatus {
            cwd: "/old".to_string(),
            shell: Some("/bin/bash".to_string()),
            selected_for_turn: true,
        },
    )]);

    let output = build_env_status_output_from_parts(
        snapshots,
        &turn_environments,
        Some("ssh:hostname".to_string()),
        Some("ssh:hostname".to_string()),
    );

    let entry = output.environments.first().expect("environment entry");
    assert_eq!(entry.cwd.as_deref(), Some("/new"));
    assert_eq!(entry.cwd_source, "env_switch_metadata");
    assert_eq!(entry.shell.as_deref(), Some("/bin/sh"));
}

#[tokio::test]
async fn status_lists_only_turn_and_thread_visible_environments() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let manager = &session.services.environment_manager;
    let thread_key = session.thread_id.to_string();
    manager
        .upsert_environment("ssh:mine".to_string(), "ws://127.0.0.1:8765".to_string())
        .expect("current thread environment");
    manager.set_environment_metadata(
        "ssh:mine".to_string(),
        EnvironmentMetadata {
            cwd: "/mine".to_string(),
            shell: None,
        },
    );
    manager.record_thread_environment_id(thread_key.clone(), "ssh:mine".to_string());
    manager.set_last_environment_id(thread_key, "ssh:mine".to_string());
    manager
        .upsert_environment("ssh:other".to_string(), "ws://127.0.0.1:9876".to_string())
        .expect("other thread environment");
    manager.set_environment_metadata(
        "ssh:other".to_string(),
        EnvironmentMetadata {
            cwd: "/other".to_string(),
            shell: None,
        },
    );
    manager.record_thread_environment_id("other-thread".to_string(), "ssh:other".to_string());

    let output = build_env_status_output(&session, &turn);
    let environment_ids = output
        .environments
        .iter()
        .map(|environment| environment.environment_id.as_str())
        .collect::<Vec<_>>();

    assert!(environment_ids.contains(&"local"));
    assert!(environment_ids.contains(&"ssh:mine"));
    assert!(!environment_ids.contains(&"ssh:other"));
    assert_eq!(
        output.last_env_switch_environment_id.as_deref(),
        Some("ssh:mine")
    );
    assert_eq!(
        output.default_execution_environment_id.as_deref(),
        Some("ssh:mine")
    );
}

#[tokio::test]
async fn status_uses_thread_specific_metadata_when_available() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let manager = &session.services.environment_manager;
    let thread_key = session.thread_id.to_string();
    manager
        .upsert_environment("ssh:shared".to_string(), "ws://127.0.0.1:8765".to_string())
        .expect("thread environment");
    manager.set_environment_metadata(
        "ssh:shared".to_string(),
        EnvironmentMetadata {
            cwd: "/global".to_string(),
            shell: None,
        },
    );
    manager.set_thread_environment_metadata(
        thread_key.clone(),
        "ssh:shared".to_string(),
        EnvironmentMetadata {
            cwd: "/thread".to_string(),
            shell: Some("/bin/sh".to_string()),
        },
    );
    manager.record_thread_environment_id(thread_key, "ssh:shared".to_string());

    let output = build_env_status_output(&session, &turn);
    let shared = output
        .environments
        .iter()
        .find(|environment| environment.environment_id == "ssh:shared")
        .expect("shared environment should be listed");

    assert_eq!(shared.cwd.as_deref(), Some("/thread"));
    assert_eq!(shared.shell.as_deref(), Some("/bin/sh"));
}

#[tokio::test]
async fn status_local_fallback_has_cwd_but_is_not_selected_for_turn() {
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    turn.environments.turn_environments.clear();
    let manager = &session.services.environment_manager;
    let thread_key = session.thread_id.to_string();
    manager.record_thread_environment_id(
        thread_key.clone(),
        codex_exec_server::LOCAL_ENVIRONMENT_ID.to_string(),
    );
    manager.set_last_environment_id(
        thread_key,
        codex_exec_server::LOCAL_ENVIRONMENT_ID.to_string(),
    );

    let output = build_env_status_output(&session, &turn);
    let local = output
        .environments
        .iter()
        .find(|environment| environment.environment_id == codex_exec_server::LOCAL_ENVIRONMENT_ID)
        .expect("local environment should be listed");

    #[allow(deprecated)]
    let expected_cwd = turn.cwd.to_string_lossy().into_owned();
    assert_eq!(local.cwd.as_deref(), Some(expected_cwd.as_str()));
    assert!(!local.is_selected_for_turn);
}

#[tokio::test]
async fn status_inherits_parent_thread_environment_cursor() {
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    let parent_thread_id = ThreadId::new();
    let parent_key = parent_thread_id.to_string();
    turn.parent_thread_id = Some(parent_thread_id);
    let manager = &session.services.environment_manager;
    manager
        .upsert_environment("ssh:parent".to_string(), "ws://127.0.0.1:8765".to_string())
        .expect("parent thread environment");
    manager.set_environment_metadata(
        "ssh:parent".to_string(),
        EnvironmentMetadata {
            cwd: "/parent".to_string(),
            shell: None,
        },
    );
    manager.record_thread_environment_id(parent_key.clone(), "ssh:parent".to_string());
    manager.set_last_environment_id(parent_key, "ssh:parent".to_string());

    let output = build_env_status_output(&session, &turn);

    assert!(
        output
            .environments
            .iter()
            .any(|environment| environment.environment_id == "ssh:parent")
    );
    assert_eq!(
        output.last_env_switch_environment_id.as_deref(),
        Some("ssh:parent")
    );
    assert_eq!(
        output.default_execution_environment_id.as_deref(),
        Some("ssh:parent")
    );
}
