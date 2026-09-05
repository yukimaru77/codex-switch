//! Opt-in real transport smoke test; use a disposable, unmounted container.

use anyhow::Context;
use codex_features::Feature;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use core_test_support::responses::ev_apply_patch_custom_tool_call;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires CODEX_ENV_SWITCH_TEST_CONTAINER pointing to a disposable Linux container"]
async fn env_switch_docker_monitor_round_trip() -> anyhow::Result<()> {
    let container = std::env::var("CODEX_ENV_SWITCH_TEST_CONTAINER")?;
    let server = start_mock_server().await;
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_function_call("switch", "env_switch", &json!({
                    "target": "docker", "container": container,
                    "cwd": "/tmp/codex-env-switch-test"
                }).to_string()),
                ev_completed("r1"),
            ]),
            sse(vec![
                ev_apply_patch_custom_tool_call("patch", "*** Begin Patch\n*** Add File: proof.txt\n+ENV_SWITCH_PATCH_OK\n*** End Patch"),
                ev_completed("r2"),
            ]),
            sse(vec![
                ev_function_call("remote", "exec_command", &json!({
                    "cmd": "pwd; uname -s; cat proof.txt", "max_output_tokens": 300
                }).to_string()),
                ev_completed("r3"),
            ]),
            sse(vec![
                ev_function_call("watch", "monitor", &json!({
                    "action": "start", "description": "Docker proof",
                    "command": "while [ ! -f go ]; do sleep 0.05; done; cat proof.txt; uname -s; exit 7"
                }).to_string()),
                ev_completed("r4"),
            ]),
            sse(vec![ev_assistant_message("m1", "watching"), ev_completed("r5")]),
            sse(vec![
                ev_function_call("local", "env_switch", "{\"target\":\"local\"}"),
                ev_completed("r6"),
            ]),
            sse(vec![
                ev_function_call("host", "exec_command", "{\"cmd\":\"pwd; uname -s\",\"max_output_tokens\":300}"),
                ev_completed("r7"),
            ]),
            sse(vec![
                ev_function_call("status", "env_status", "{}"),
                ev_completed("r8"),
            ]),
            sse(vec![ev_assistant_message("m2", "back on host"), ev_completed("r9")]),
        ],
    ).await;
    let test = test_codex()
        .with_config(|config| {
            config.features.enable(Feature::EnvSwitch).unwrap();
            config.features.enable(Feature::Monitor).unwrap();
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn_with_permission_profile(
        "verify Docker environment routing",
        PermissionProfile::Disabled,
    )
    .await?;
    assert!(
        mock.function_call_output_text("switch")
            .context("switch output")?
            .contains("env_switch complete")
    );
    let remote = mock
        .function_call_output_text("remote")
        .context("remote output")?;
    assert!(remote.contains("/tmp/codex-env-switch-test"), "{remote}");
    assert!(remote.contains("Linux"), "{remote}");
    assert!(remote.contains("ENV_SWITCH_PATCH_OK"), "{remote}");
    assert!(
        !test.config.cwd.join("proof.txt").exists(),
        "patch must not target the host"
    );
    let requests_before_signal = mock.requests().len();
    assert_eq!(requests_before_signal, 5);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        mock.requests().len(),
        requests_before_signal,
        "quiet remote watcher must not invoke the model"
    );
    let signal = tokio::process::Command::new("docker")
        .args(["exec", &container, "touch", "/tmp/codex-env-switch-test/go"])
        .output()
        .await?;
    assert!(
        signal.status.success(),
        "{}",
        String::from_utf8_lossy(&signal.stderr)
    );
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let requests = mock.requests();
    assert_eq!(requests.len(), 9);
    assert!(requests[5].message_input_texts("user").iter().any(
        |text| text.contains("Docker proof")
            && text.contains("ENV_SWITCH_PATCH_OK")
            && text.contains("Linux")
            && text.contains("code 7")
    ));
    let host = mock
        .function_call_output_text("host")
        .context("host output")?;
    assert!(
        host.contains(&test.config.cwd.as_path().display().to_string()),
        "{host}"
    );
    #[cfg(target_os = "macos")]
    assert!(host.contains("Darwin"), "{host}");
    let status: serde_json::Value = serde_json::from_str(
        &mock
            .function_call_output_text("status")
            .context("status output")?,
    )?;
    assert_eq!(status["default_execution_environment_id"], json!("local"));
    test.codex.submit(Op::Shutdown).await?;
    Ok(())
}
