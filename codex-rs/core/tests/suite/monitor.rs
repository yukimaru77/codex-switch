use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_features::Feature;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use core_test_support::process::wait_for_pid_file;
use core_test_support::process::wait_for_process_exit;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::skip_if_host_windows;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_sandbox;
use core_test_support::test_codex::TestCodexHarness;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_match;
use serde_json::Value;
use serde_json::json;

fn monitor_builder() -> core_test_support::test_codex::TestCodexBuilder {
    test_codex().with_model("gpt-5.4").with_config(|config| {
        for feature in [Feature::ShellTool, Feature::UnifiedExec, Feature::Monitor] {
            config
                .features
                .enable(feature)
                .expect("monitor test feature should enable");
        }
    })
}

fn parse_tool_json(output: &str) -> Value {
    serde_json::from_str(output).expect("monitor tool output should be JSON")
}

fn assert_monitor_context(request: &ResponsesRequest, name: &str, expected_text: &str) {
    assert!(request.has_content_kinds(&["monitor.event"]));
    let monitor_field = format!(r#""monitor":"{name}""#);
    let body = request.body_json();
    let has_valid_assistant_output = body["input"]
        .as_array()
        .expect("request input should be an array")
        .iter()
        .filter(|item| item["role"] == "assistant")
        .filter_map(|item| item["content"].as_array())
        .flatten()
        .any(|content| {
            content["type"] == "output_text"
                && content["text"].as_str().is_some_and(|text| {
                    text.contains(&monitor_field) && text.contains(expected_text)
                })
        });
    assert!(
        has_valid_assistant_output,
        "monitor context must use assistant output_text"
    );
}

async fn start_turn_with_disabled_permissions(
    harness: &TestCodexHarness,
    prompt: &str,
) -> Result<()> {
    let test = harness.test();
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, test.config.cwd.as_path());
    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: prompt.to_string(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                collaboration_mode: Some(CollaborationMode {
                    mode: ModeKind::Default,
                    settings: Settings {
                        model: test.session_configured.model.clone(),
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            }),
        )
        .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn monitor_start_list_stop_terminates_process_tree() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    skip_if_host_windows!(Ok(()));

    let harness = TestCodexHarness::with_auto_env_builder(monitor_builder()).await?;
    let pid_path = harness.path("monitor-child.pid");
    let command = "sleep 30 & child=$!; echo \"$child\" > monitor-child.pid; wait \"$child\"";
    let start_args = json!({ "name": "integration", "command": command });
    let stop_args = json!({ "name": "integration" });
    let responses = mount_sse_sequence(
        harness.server(),
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(
                    "monitor-start",
                    "monitor_start",
                    &serde_json::to_string(&start_args)?,
                ),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-2", "monitor started"),
                ev_completed("resp-2"),
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_function_call("monitor-list", "monitor_list", "{}"),
                ev_completed("resp-3"),
            ]),
            sse(vec![
                ev_response_created("resp-4"),
                ev_function_call(
                    "monitor-stop",
                    "monitor_stop",
                    &serde_json::to_string(&stop_args)?,
                ),
                ev_completed("resp-4"),
            ]),
            sse(vec![
                ev_response_created("resp-5"),
                ev_assistant_message("msg-5", "done"),
                ev_completed("resp-5"),
            ]),
        ],
    )
    .await;

    harness
        .submit_with_permission_profile("start the monitor", PermissionProfile::Disabled)
        .await?;

    let start = parse_tool_json(&harness.function_call_stdout("monitor-start").await);
    assert_eq!(start["status"], "started");
    let pid = wait_for_pid_file(&pid_path).await?;

    harness
        .submit_with_permission_profile("inspect and stop the monitor", PermissionProfile::Disabled)
        .await?;

    let list = parse_tool_json(&harness.function_call_stdout("monitor-list").await);
    assert_eq!(list["monitors"][0]["name"], "integration");
    assert_eq!(list["monitors"][0]["status"], "running");
    let stop = parse_tool_json(&harness.function_call_stdout("monitor-stop").await);
    assert_eq!(stop["status"], "stopped");

    wait_for_process_exit(&pid).await?;

    let requests = responses.requests();
    let final_request = requests
        .last()
        .expect("final request should include monitor events");
    assert_monitor_context(final_request, "integration", "cancelled");

    let first_request = requests[0].body_json();
    let first_tools = first_request["tools"]
        .as_array()
        .expect("tools should be an array")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect::<Vec<_>>();
    for name in ["monitor_start", "monitor_stop", "monitor_list"] {
        assert!(first_tools.contains(&name), "missing tool {name}");
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn short_lived_monitor_completes_without_leaking_registration() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    skip_if_host_windows!(Ok(()));

    let harness = TestCodexHarness::with_auto_env_builder(monitor_builder()).await?;
    let args = json!({ "name": "short-lived", "command": "sleep 1" });
    let responses = mount_sse_sequence(
        harness.server(),
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(
                    "monitor-short-lived",
                    "monitor_start",
                    &serde_json::to_string(&args)?,
                ),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-2", "monitor started"),
                ev_completed("resp-2"),
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_assistant_message("msg-3", "monitor completed"),
                ev_completed("resp-3"),
            ]),
            sse(vec![
                ev_response_created("resp-4"),
                ev_function_call("monitor-list-short-lived", "monitor_list", "{}"),
                ev_completed("resp-4"),
            ]),
            sse(vec![
                ev_response_created("resp-5"),
                ev_assistant_message("msg-5", "done"),
                ev_completed("resp-5"),
            ]),
        ],
    )
    .await;

    harness
        .submit_with_permission_profile("run a short monitor", PermissionProfile::Disabled)
        .await?;
    let start = parse_tool_json(&harness.function_call_stdout("monitor-short-lived").await);
    assert_eq!(start["status"], "started");

    let notification = wait_for_event_match(&harness.test().codex, |event| match event {
        EventMsg::MonitorNotification(event)
            if event.monitor_name == "short-lived" && event.kind == "completed" =>
        {
            Some(event.clone())
        }
        _ => None,
    })
    .await;
    assert_eq!(notification.kind, "completed");
    wait_for_event(&harness.test().codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    harness
        .submit_with_permission_profile("list monitors", PermissionProfile::Disabled)
        .await?;
    let list = parse_tool_json(
        &harness
            .function_call_stdout("monitor-list-short-lived")
            .await,
    );
    assert_eq!(list["monitors"], json!([]));

    let requests = responses.requests();
    assert_eq!(requests.len(), 5);
    assert_monitor_context(&requests[2], "short-lived", "completed successfully");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn monitor_output_wakes_an_idle_thread() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    skip_if_host_windows!(Ok(()));

    let harness = TestCodexHarness::with_auto_env_builder(monitor_builder()).await?;
    let pid_path = harness.path("idle-monitor.pid");
    let command = "echo \"$$\" > idle-monitor.pid; sleep 1; echo idle-wake-marker; sleep 30";
    let args = json!({ "name": "idle-wake", "command": command });
    let responses = mount_sse_sequence(
        harness.server(),
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(
                    "monitor-start-idle",
                    "monitor_start",
                    &serde_json::to_string(&args)?,
                ),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-2", "monitor started"),
                ev_completed("resp-2"),
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_assistant_message("msg-3", "monitor observed"),
                ev_completed("resp-3"),
            ]),
        ],
    )
    .await;

    harness
        .submit_with_permission_profile("start the idle monitor", PermissionProfile::Disabled)
        .await?;
    let notification = wait_for_event_match(&harness.test().codex, |event| match event {
        EventMsg::MonitorNotification(event) if event.monitor_name == "idle-wake" => {
            Some(event.clone())
        }
        _ => None,
    })
    .await;
    assert_eq!(notification.kind, "output");
    assert!(notification.summary.contains("idle-wake-marker"));
    wait_for_event(&harness.test().codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = responses.requests();
    assert_eq!(requests.len(), 3);
    assert_monitor_context(&requests[2], "idle-wake", "idle-wake-marker");

    harness.test().codex.submit(Op::Shutdown).await?;
    wait_for_event(&harness.test().codex, |event| {
        matches!(event, EventMsg::ShutdownComplete)
    })
    .await;
    let pid = wait_for_pid_file(&pid_path).await?;
    wait_for_process_exit(&pid).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn monitor_start_honors_read_only_sandbox() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    skip_if_host_windows!(Ok(()));

    let harness = TestCodexHarness::with_auto_env_builder(monitor_builder()).await?;
    let args = json!({
        "name": "sandbox-check",
        "command": "sleep 2; printf escaped > monitor-must-not-exist.txt",
    });
    mount_sse_sequence(
        harness.server(),
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(
                    "monitor-sandbox",
                    "monitor_start",
                    &serde_json::to_string(&args)?,
                ),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-2", "monitor started"),
                ev_completed("resp-2"),
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_assistant_message("msg-3", "sandbox failure observed"),
                ev_completed("resp-3"),
            ]),
        ],
    )
    .await;

    harness
        .submit_with_permission_profile(
            "verify the monitor sandbox",
            PermissionProfile::read_only(),
        )
        .await?;

    let output = parse_tool_json(&harness.function_call_stdout("monitor-sandbox").await);
    assert_eq!(output["status"], "started");
    let notification = wait_for_event_match(&harness.test().codex, |event| match event {
        EventMsg::MonitorNotification(event) if event.monitor_name == "sandbox-check" => {
            (event.kind == "failed").then(|| event.clone())
        }
        _ => None,
    })
    .await;
    assert_eq!(notification.kind, "failed");
    wait_for_event(&harness.test().codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert!(!harness.path_exists("monitor-must-not-exist.txt").await?);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_monitor_start_cleans_registration_and_process() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    skip_if_host_windows!(Ok(()));

    let harness = TestCodexHarness::with_auto_env_builder(monitor_builder()).await?;
    let command = "sleep 30";
    let start_args = json!({ "name": "interrupted", "command": command });
    let responses = mount_sse_sequence(
        harness.server(),
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(
                    "monitor-start-interrupted",
                    "monitor_start",
                    &serde_json::to_string(&start_args)?,
                ),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_function_call("monitor-list-after-interrupt", "monitor_list", "{}"),
                ev_completed("resp-2"),
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_assistant_message("msg-3", "done"),
                ev_completed("resp-3"),
            ]),
        ],
    )
    .await;

    start_turn_with_disabled_permissions(&harness, "start the monitor").await?;
    wait_for_event(&harness.test().codex, |event| {
        matches!(
            event,
            EventMsg::ExecCommandBegin(event)
                if event.call_id == "monitor-start-interrupted"
        )
    })
    .await;

    harness.test().codex.submit(Op::Interrupt).await?;
    wait_for_event(&harness.test().codex, |event| {
        matches!(event, EventMsg::TurnAborted(_))
    })
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if harness
                .test()
                .codex
                .list_background_terminals()
                .await
                .is_empty()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("interrupted monitor process should leave the Unified Exec store");

    harness
        .submit_with_permission_profile("list monitors", PermissionProfile::Disabled)
        .await?;
    let list = parse_tool_json(
        &harness
            .function_call_stdout("monitor-list-after-interrupt")
            .await,
    );
    assert_eq!(list["monitors"], json!([]));
    assert_eq!(responses.requests().len(), 3);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cleaning_background_terminals_during_startup_cleans_monitor() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_sandbox!(Ok(()));
    skip_if_host_windows!(Ok(()));

    let harness = TestCodexHarness::with_auto_env_builder(monitor_builder()).await?;
    let pid_path = harness.path("cleaned-monitor.pid");
    let args = json!({
        "name": "cleaned",
        "command": "echo \"$$\" > cleaned-monitor.pid; sleep 30",
    });
    let responses = mount_sse_sequence(
        harness.server(),
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(
                    "monitor-start-cleaned",
                    "monitor_start",
                    &serde_json::to_string(&args)?,
                ),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-2", "start cancelled"),
                ev_completed("resp-2"),
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_function_call("monitor-list-cleaned", "monitor_list", "{}"),
                ev_completed("resp-3"),
            ]),
            sse(vec![
                ev_response_created("resp-4"),
                ev_assistant_message("msg-4", "done"),
                ev_completed("resp-4"),
            ]),
        ],
    )
    .await;

    start_turn_with_disabled_permissions(&harness, "start a monitor, then clean it").await?;
    wait_for_event(&harness.test().codex, |event| {
        matches!(
            event,
            EventMsg::ExecCommandBegin(event) if event.call_id == "monitor-start-cleaned"
        )
    })
    .await;
    let pid = wait_for_pid_file(&pid_path).await?;

    harness
        .test()
        .codex
        .submit(Op::CleanBackgroundTerminals)
        .await?;
    wait_for_process_exit(&pid).await?;
    wait_for_event(&harness.test().codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    harness
        .submit_with_permission_profile("list monitors", PermissionProfile::Disabled)
        .await?;
    let list = parse_tool_json(&harness.function_call_stdout("monitor-list-cleaned").await);
    assert_eq!(list["monitors"], json!([]));
    assert_eq!(responses.requests().len(), 4);
    Ok(())
}
