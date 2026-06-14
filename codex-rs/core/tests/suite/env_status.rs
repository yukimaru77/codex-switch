use anyhow::Context;
use anyhow::Result;
use codex_exec_server::EnvironmentMetadata;
use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_features::Feature;
use core_test_support::responses::ResponseMock;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::local;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tokio::time::Duration;

const STATUS_NOTE: &str = "This is read-only status. Compatible environment-aware tool calls that omit environment_id use default_execution_environment_id for this thread; pass a listed environment_id explicitly to target a different registered environment.";

fn tool_names(body: &Value) -> Vec<String> {
    body.get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| {
                    tool.get("name")
                        .or_else(|| tool.get("type"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn body_contains(request: &wiremock::Request, text: &str) -> bool {
    serde_json::from_slice::<Value>(&request.body).is_ok_and(|body| body.to_string().contains(text))
}

fn has_function_call_output(request: &wiremock::Request, call_id: &str) -> bool {
    serde_json::from_slice::<Value>(&request.body).is_ok_and(|body| {
        body.get("input")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items.iter().any(|item| {
                    item.get("type").and_then(Value::as_str) == Some("function_call_output")
                        && item.get("call_id").and_then(Value::as_str) == Some(call_id)
                })
            })
    })
}

async fn wait_for_function_output(mock: &ResponseMock, call_id: &str) -> Result<String> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(output) = mock.function_call_output_text(call_id) {
                return output;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .with_context(|| format!("timed out waiting for function_call_output {call_id}"))
}

fn seed_thread_remote_environment(
    test: &core_test_support::test_codex::TestCodex,
    thread_key: String,
    environment_id: &str,
    cwd: &str,
) -> Result<()> {
    let manager = test.thread_manager.environment_manager();
    manager
        .upsert_environment(
            environment_id.to_string(),
            "ws://127.0.0.1:9876".to_string(),
        )
        .context("seed remote environment")?;
    manager.set_environment_metadata(
        environment_id.to_string(),
        EnvironmentMetadata {
            cwd: cwd.to_string(),
            shell: Some("/bin/bash".to_string()),
        },
    );
    manager.record_thread_environment_id(thread_key.clone(), environment_id.to_string());
    manager.set_last_environment_id(thread_key, environment_id.to_string());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn env_list_reports_local_switch_state_through_tool_dispatch() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let switch_call_id = "call-env-switch-local";
    let list_call_id = "call-env-list";
    let responses_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(
                    switch_call_id,
                    "env_switch",
                    &json!({ "target": LOCAL_ENVIRONMENT_ID }).to_string(),
                ),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_function_call(list_call_id, "env_list", "{}"),
                ev_completed("resp-2"),
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-3"),
            ]),
        ],
    )
    .await;

    let mut builder = test_codex().with_config(|config| {
        config
            .features
            .enable(Feature::EnvSwitch)
            .expect("env_switch feature should enable for test");
    });
    let test = builder.build(&server).await?;
    let cwd = test.config.cwd.as_path().to_string_lossy().into_owned();

    test.submit_turn_with_environments(
        "switch to local and list environments",
        Some(vec![local(test.config.cwd.clone())]),
    )
    .await?;

    let list_output = wait_for_function_output(&responses_mock, list_call_id).await?;
    let first_request = responses_mock
        .requests()
        .into_iter()
        .next()
        .context("missing first request")?;
    let names = tool_names(&first_request.body_json());
    assert!(
        names.contains(&"env_switch".to_string())
            && names.contains(&"env_status".to_string())
            && names.contains(&"env_list".to_string()),
        "environment status tools should be model-visible with env_switch enabled: {names:?}",
    );

    let switch_output = responses_mock
        .function_call_output_text(switch_call_id)
        .context("missing env_switch function_call_output")?;
    assert!(
        switch_output.contains("local host environment"),
        "unexpected env_switch output: {switch_output}",
    );

    let status: Value = serde_json::from_str(&list_output)?;

    assert_eq!(
        status,
        json!({
            "default_execution_environment_id": LOCAL_ENVIRONMENT_ID,
            "last_env_switch_environment_id": LOCAL_ENVIRONMENT_ID,
            "environments": [
                {
                    "environment_id": LOCAL_ENVIRONMENT_ID,
                    "kind": "local",
                    "is_manager_default": true,
                    "is_default_execution_environment": true,
                    "is_selected_for_turn": true,
                    "is_last_env_switch": true,
                    "cwd": cwd,
                    "cwd_source": "turn",
                    "shell": null,
                }
            ],
            "note": STATUS_NOTE,
        })
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn env_status_reports_thread_visible_dynamic_environment() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let call_id = "call-env-status-dynamic";
    let responses_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_function_call(call_id, "env_status", "{}"),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-1", "done"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;

    let mut builder = test_codex().with_config(|config| {
        config
            .features
            .enable(Feature::EnvSwitch)
            .expect("env_switch feature should enable for test");
    });
    let test = builder.build(&server).await?;
    let thread_key = test.session_configured.thread_id.to_string();
    seed_thread_remote_environment(&test, thread_key, "ssh:hostname", "/remote/work")?;
    seed_thread_remote_environment(&test, "other-thread".to_string(), "ssh:other", "/other")?;

    test.submit_turn_with_environments(
        "inspect dynamic environment status",
        Some(vec![local(test.config.cwd.clone())]),
    )
    .await?;

    let output = wait_for_function_output(&responses_mock, call_id).await?;
    let status: Value = serde_json::from_str(&output)?;
    let environments = status
        .get("environments")
        .and_then(Value::as_array)
        .context("env_status should return environments array")?;

    assert_eq!(
        status.get("last_env_switch_environment_id"),
        Some(&json!("ssh:hostname"))
    );
    assert_eq!(
        status.get("default_execution_environment_id"),
        Some(&json!("ssh:hostname"))
    );
    assert!(
        environments
            .iter()
            .any(|entry| entry["environment_id"] == "ssh:hostname"
                && entry["cwd"] == "/remote/work"
                && entry["shell"] == "/bin/bash"
                && entry["is_last_env_switch"] == true
                && entry["is_default_execution_environment"] == true),
        "dynamic environment should be listed with metadata: {status}",
    );
    assert!(
        environments
            .iter()
            .all(|entry| entry["environment_id"] != "ssh:other"),
        "unrelated thread environment should not be listed: {status}",
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawned_agent_env_status_inherits_parent_thread_environment_cursor() -> Result<()> {
    skip_if_no_network!(Ok(()));

    const PARENT_PROMPT: &str = "spawn a child to inspect env status";
    const CHILD_PROMPT: &str = "child should inspect env status";
    const SPAWN_CALL_ID: &str = "call-spawn-agent";
    const CHILD_STATUS_CALL_ID: &str = "call-child-env-status";

    let server = start_mock_server().await;
    let spawn_args = serde_json::to_string(&json!({
        "message": CHILD_PROMPT,
        "task_name": "worker",
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, PARENT_PROMPT),
        sse(vec![
            ev_response_created("resp-parent-1"),
            ev_function_call(SPAWN_CALL_ID, "spawn_agent", &spawn_args),
            ev_completed("resp-parent-1"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, CHILD_PROMPT),
        sse(vec![
            ev_response_created("resp-child-1"),
            ev_function_call(CHILD_STATUS_CALL_ID, "env_status", "{}"),
            ev_completed("resp-child-1"),
        ]),
    )
    .await;
    let child_followup = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| has_function_call_output(request, CHILD_STATUS_CALL_ID),
        sse(vec![
            ev_response_created("resp-child-2"),
            ev_assistant_message("msg-child-1", "child done"),
            ev_completed("resp-child-2"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| has_function_call_output(request, SPAWN_CALL_ID),
        sse(vec![
            ev_response_created("resp-parent-2"),
            ev_assistant_message("msg-parent-1", "parent done"),
            ev_completed("resp-parent-2"),
        ]),
    )
    .await;

    let mut builder = test_codex().with_model("koffing").with_config(|config| {
        config
            .features
            .enable(Feature::EnvSwitch)
            .expect("env_switch feature should enable for test");
        config
            .features
            .enable(Feature::Collab)
            .expect("collab feature should enable for test");
        config
            .features
            .enable(Feature::MultiAgentV2)
            .expect("multi-agent v2 feature should enable for test");
    });
    let test = builder.build(&server).await?;
    let parent_key = test.session_configured.thread_id.to_string();
    seed_thread_remote_environment(&test, parent_key, "ssh:parent", "/parent/work")?;

    test.submit_turn_with_environments(PARENT_PROMPT, Some(vec![local(test.config.cwd.clone())]))
        .await?;

    let output = wait_for_function_output(&child_followup, CHILD_STATUS_CALL_ID).await?;
    let status: Value = serde_json::from_str(&output)?;
    assert_eq!(
        status.get("last_env_switch_environment_id"),
        Some(&json!("ssh:parent"))
    );
    assert_eq!(
        status.get("default_execution_environment_id"),
        Some(&json!("ssh:parent"))
    );
    assert!(
        status
            .get("environments")
            .and_then(Value::as_array)
            .is_some_and(
                |environments| environments.iter().any(|entry| entry["environment_id"]
                    == "ssh:parent"
                    && entry["cwd"] == "/parent/work"
                    && entry["is_last_env_switch"] == true
                    && entry["is_default_execution_environment"] == true)
            ),
        "child env_status should include parent-thread env_switch state: {status}",
    );

    Ok(())
}
