use codex_exec_server::EnvironmentManager;
use codex_exec_server::EnvironmentMetadata;
use codex_exec_server::provision::Hop;
use codex_exec_server::provision::RemoteLauncher;
use codex_exec_server::provision::posix_single_quote;
use codex_protocol::ThreadId;
use codex_tools::ToolSpec;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::sync::Arc;

use crate::tools::handlers::env_switch_spec::ENV_SWITCH_TOOL_NAME;
use crate::tools::handlers::env_switch_spec::create_env_switch_tool;
use crate::tools::registry::ToolExecutor;

use super::EnvSwitchArgs;
use super::EnvSwitchHandler;
use super::HopArg;
use super::handle_local_switch;
use super::hop_from_arg;
use super::implicit_base_launcher;
use super::resolve_remote_cwd_script;
use super::validate_addressing_mode;
use crate::tools::handlers::environment_thread_keys;
use crate::tools::handlers::resolve_tool_environment;

// ---------------------------------------------------------------------------
// Spec / arg-schema tests
// ---------------------------------------------------------------------------

#[test]
fn tool_name_is_env_switch() {
    let handler = EnvSwitchHandler;
    assert_eq!(handler.tool_name().name, ENV_SWITCH_TOOL_NAME);
    assert!(handler.tool_name().namespace.is_none());
}

#[test]
fn spec_is_function_with_correct_name() {
    let spec = create_env_switch_tool();
    assert_eq!(spec.name(), ENV_SWITCH_TOOL_NAME);
    match spec {
        ToolSpec::Function(tool) => {
            assert_eq!(tool.name, ENV_SWITCH_TOOL_NAME);
            // strict must be false because cwd / container / host are optional
            assert!(!tool.strict);
        }
        other => panic!("expected ToolSpec::Function, got {other:?}"),
    }
}

#[test]
fn spec_does_not_require_target_because_hops_and_extend_are_valid_modes() {
    let spec = create_env_switch_tool();
    let ToolSpec::Function(tool) = spec else {
        panic!("expected ToolSpec::Function");
    };
    let params = tool.parameters;
    let required = params.required.unwrap_or_default();
    assert!(
        required.is_empty(),
        "schema should leave addressing-mode validation to the runtime, got: {required:?}"
    );
}

#[test]
fn spec_has_all_expected_properties() {
    let spec = create_env_switch_tool();
    let ToolSpec::Function(tool) = spec else {
        panic!("expected ToolSpec::Function");
    };
    let properties = tool.parameters.properties.unwrap_or_default();
    for expected_key in [
        "target",
        "container",
        "host",
        "cwd",
        "hops",
        "base",
        "extend",
    ] {
        assert!(
            properties.contains_key(expected_key),
            "missing property `{expected_key}` in spec, found: {properties:?}"
        );
    }
}

#[test]
fn spec_describes_environment_id_contract_without_prompt_level_prohibition() {
    let spec = create_env_switch_tool();
    let ToolSpec::Function(tool) = spec else {
        panic!("expected ToolSpec::Function");
    };
    assert!(
        tool.description
            .contains("exec_command, apply_patch, or view_image"),
        "env_switch description should name compatible environment-aware tools"
    );
    assert!(
        tool.description
            .contains("make it the default execution environment")
            && tool
                .description
                .contains("calls that omit `environment_id`"),
        "env_switch description should explain omitted environment_id default switching"
    );
    assert!(
        !tool
            .description
            .contains("does not change the default execution environment"),
        "env_switch description must not preserve the old explicit-only contract"
    );
    assert!(
        tool.description
            .contains("Raw ssh/docker commands remain appropriate"),
        "env_switch description should preserve legitimate raw ssh/docker uses"
    );
    assert!(
        tool.description.contains("base=\"ssh:example-host\"")
            && tool.description.contains("extend={\"type\":\"docker\""),
        "env_switch description should show nested SSH-to-Docker addressing"
    );
    assert!(
        tool.description
            .contains("never to the local workspace path"),
        "env_switch description should discourage passing local cwd as remote cwd"
    );
}

// ---------------------------------------------------------------------------
// RemoteLauncher shell_argv tests (replaces deprecated argv_prefix)
// ---------------------------------------------------------------------------

#[test]
fn docker_launcher_shell_argv_structure() {
    let launcher = RemoteLauncher::docker("my-container");
    let argv = launcher.shell_argv("echo hello");
    // Docker: ["docker", "exec", "-i", "--", "<container>", "sh", "-c", "<script>"]
    // The "--" end-of-options separator prevents container names that start with
    // "-" from being misinterpreted as docker flags.
    assert_eq!(argv[0], "docker");
    assert_eq!(argv[1], "exec");
    assert_eq!(argv[2], "-i");
    assert_eq!(argv[3], "--");
    assert_eq!(argv[4], "my-container");
    assert_eq!(argv[5], "sh");
    assert_eq!(argv[6], "-c");
    assert_eq!(argv[7], "echo hello");
}

#[test]
fn ssh_launcher_shell_argv_structure() {
    let launcher = RemoteLauncher::ssh("user@remote");
    let argv = launcher.shell_argv("echo hello");
    // SSH: ["ssh", <hardening flags>, "--", "<host>",
    //       shell_join(["sh", "-c", "<script>"])]
    // The hardening flags suppress password prompts, banners and MOTD.
    // The "--" separator prevents host names starting with "-" from being
    // misinterpreted as SSH flags.
    assert_eq!(argv[0], "ssh");
    assert!(argv.contains(&"-T".to_string()));
    assert!(argv.contains(&"-q".to_string()));
    assert!(argv.contains(&"BatchMode=yes".to_string()));
    assert!(argv.contains(&"StrictHostKeyChecking=accept-new".to_string()));
    assert!(argv.contains(&"LogLevel=ERROR".to_string()));
    assert!(argv.contains(&"ConnectTimeout=20".to_string()));
    let separator = argv
        .iter()
        .position(|arg| arg == "--")
        .expect("ssh argv should include -- separator");
    assert_eq!(argv[separator + 1], "user@remote");
    // The last element contains the quoted shell invocation.
    let script_arg = argv.last().expect("ssh script argument");
    assert!(
        script_arg.starts_with("'sh'"),
        "expected ssh script arg to start with `'sh'`, got: {script_arg:?}",
    );
}

// ---------------------------------------------------------------------------
// cwd injection / shell quoting tests (#1)
// ---------------------------------------------------------------------------

/// Verifies that a cwd containing shell metacharacters is wrapped in POSIX
/// single-quotes so it cannot be interpreted as a shell command.
///
/// The key property: the metacharacters appear only inside single-quote
/// delimiters and never as bare tokens that a shell would interpret.
#[test]
fn posix_single_quote_neutralises_metacharacters() {
    // If the cwd contains `; touch /tmp/pwned`, the raw string would inject a
    // second shell command.  posix_single_quote must wrap it so the semicolon
    // is neutralised.
    let evil_cwd = "/tmp/test; touch /tmp/pwned";
    let quoted = posix_single_quote(evil_cwd);
    assert_eq!(quoted, "'/tmp/test; touch /tmp/pwned'");

    // The resulting mkdir -p script must contain the fully-quoted form.
    let script = format!("mkdir -p {}", posix_single_quote(evil_cwd));
    assert_eq!(script, "mkdir -p '/tmp/test; touch /tmp/pwned'");

    // The dangerous sequence must NOT appear as bare (unquoted) tokens.
    // We verify this by checking that there is no `'; ...'` token split that
    // would allow the shell to interpret `;` as a command separator.
    // Specifically, the string outside all single-quoted segments must not
    // contain `;`.
    //
    // We check this by stripping the expected quoted argument and verifying
    // what remains is only `mkdir -p ` (no bare metacharacters).
    let remainder = script.replace("'/tmp/test; touch /tmp/pwned'", "");
    assert_eq!(
        remainder, "mkdir -p ",
        "nothing should remain after removing the quoted arg, got: {remainder:?}"
    );
}

#[test]
fn posix_single_quote_neutralises_dollar_expansion() {
    let cwd = "/home/$(id -u)/.secret";
    let quoted = posix_single_quote(cwd);
    // Must be wrapped in single quotes so $(...) is not expanded.
    assert!(quoted.starts_with('\''), "must start with single quote");
    assert!(quoted.ends_with('\''), "must end with single quote");
    // The dollar sign and parentheses are present literally, not as a command.
    assert!(
        quoted.contains("$(id -u)"),
        "literal content must be preserved"
    );
}

#[test]
fn posix_single_quote_handles_embedded_single_quote() {
    // A cwd like "/root/it's" must be escaped as '/root/it'\''s'.
    let cwd = "/root/it's";
    let quoted = posix_single_quote(cwd);
    assert_eq!(quoted, r"'/root/it'\''s'");
}

#[test]
fn posix_single_quote_handles_spaces() {
    let cwd = "/my dir/with spaces";
    let quoted = posix_single_quote(cwd);
    assert_eq!(quoted, "'/my dir/with spaces'");

    let script = format!("mkdir -p {quoted}");
    // The spaces appear only inside the quotes; the raw unquoted form would
    // be "/my" followed by "dir/with" as separate tokens.
    assert_eq!(script, "mkdir -p '/my dir/with spaces'");
}

/// When the script that probes/creates $HOME is built, it must not embed
/// the literal cwd string unquoted when a caller-supplied cwd is used.
#[test]
fn mkdir_script_quotes_caller_supplied_cwd() {
    let evil_cwd = "/workspace/project;rm -rf /";
    let script = resolve_remote_cwd_script(Some(evil_cwd));
    // The complete script must equal the expected quoted form exactly.
    assert_eq!(
        script,
        "_codex_cwd='/workspace/project;rm -rf /'\nmkdir -p -- \"$_codex_cwd\" && cd -P -- \"$_codex_cwd\" && printf '%s' \"$PWD\"",
        "expected exactly the quoted script"
    );
    // Strip the quoted argument; nothing dangerous should be left bare.
    let remainder = script.replace("'/workspace/project;rm -rf /'", "");
    assert!(
        !remainder.contains(";rm -rf /"),
        "no bare metacharacters after removing quoted arg; got: {remainder:?}"
    );
}

#[test]
fn remote_cwd_script_uses_remote_home_when_omitted() {
    let script = resolve_remote_cwd_script(None);
    assert_eq!(
        script,
        "_codex_cwd=\"$HOME\"\nmkdir -p -- \"$_codex_cwd\" && cd -P -- \"$_codex_cwd\" && printf '%s' \"$PWD\""
    );
}

#[test]
fn remote_cwd_script_expands_tilde_against_remote_home() {
    let script = resolve_remote_cwd_script(Some("~"));
    assert_eq!(
        script,
        "_codex_cwd=\"$HOME\"\nmkdir -p -- \"$_codex_cwd\" && cd -P -- \"$_codex_cwd\" && printf '%s' \"$PWD\""
    );
}

#[test]
fn remote_cwd_script_expands_tilde_prefix_against_remote_home() {
    let script = resolve_remote_cwd_script(Some("~/work dir/it's"));
    assert_eq!(
        script,
        "_codex_cwd=\"$HOME\"/'work dir/it'\\''s'\nmkdir -p -- \"$_codex_cwd\" && cd -P -- \"$_codex_cwd\" && printf '%s' \"$PWD\""
    );
}

// ---------------------------------------------------------------------------
// Relative mode: pure launcher-level logic (no network / docker required)
// ---------------------------------------------------------------------------

/// Relative mode: base="ssh:hostname" + extend=docker:container-name
/// produces id "ssh:hostname>docker:container-name".
#[test]
fn relative_mode_base_plus_extend_produces_correct_hops() {
    let base = RemoteLauncher::from_id("ssh:hostname").expect("parse base");
    let extended = base.with_appended_hop(Hop::Docker {
        container: "container-name".to_string(),
    });
    assert_eq!(extended.id(), "ssh:hostname>docker:container-name");
    assert_eq!(extended.hops.len(), 2);
    assert_eq!(
        extended.hops[0],
        Hop::Ssh {
            host: "hostname".to_string()
        }
    );
    assert_eq!(
        extended.hops[1],
        Hop::Docker {
            container: "container-name".to_string()
        }
    );
}

/// Relative mode starting from a two-hop base appends a third hop correctly.
#[test]
fn relative_mode_three_hop_chain() {
    let base = RemoteLauncher::from_id("ssh:jump-host>ssh:hostname").expect("parse base");
    let extended = base.with_appended_hop(Hop::Docker {
        container: "container-name".to_string(),
    });
    assert_eq!(
        extended.id(),
        "ssh:jump-host>ssh:hostname>docker:container-name"
    );
    assert_eq!(extended.hops.len(), 3);
}

/// from_id followed by with_appended_hop round-trips correctly.
#[test]
fn relative_mode_roundtrip_id() {
    let original_id = "ssh:user@host>docker:container-1";
    let base = RemoteLauncher::from_id(original_id).expect("parse base");
    // The parsed launcher's id must equal the original.
    assert_eq!(base.id(), original_id);
    // After appending an ssh hop the id grows as expected.
    let extended = base.with_appended_hop(Hop::Ssh {
        host: "inner".to_string(),
    });
    assert_eq!(extended.id(), "ssh:user@host>docker:container-1>ssh:inner");
}

/// Spec: base and extend fields are present and not in the required list.
#[test]
fn spec_base_and_extend_are_optional() {
    let spec = create_env_switch_tool();
    let ToolSpec::Function(tool) = spec else {
        panic!("expected ToolSpec::Function");
    };
    let required = tool.parameters.required.unwrap_or_default();
    for optional in ["base", "extend"] {
        assert!(
            !required.contains(&optional.to_string()),
            "`{optional}` should be optional but was found in required: {required:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// hop_from_arg validation tests (#5 — validate_hop_value integration)
// ---------------------------------------------------------------------------

/// An ssh hop without a `host` field must return a descriptive error.
#[test]
fn hop_from_arg_ssh_missing_host_returns_error() {
    let arg = HopArg {
        hop_type: "ssh".to_string(),
        host: None,
        container: None,
    };
    let err = hop_from_arg(arg).expect_err("should fail: no host");
    match err {
        crate::function_tool::FunctionCallError::RespondToModel(msg) => {
            assert!(
                msg.contains("requires a `host` field"),
                "expected missing-host message, got: {msg}"
            );
        }
        other => panic!("expected RespondToModel, got {other:?}"),
    }
}

/// A docker hop without a `container` field must return a descriptive error.
#[test]
fn hop_from_arg_docker_missing_container_returns_error() {
    let arg = HopArg {
        hop_type: "docker".to_string(),
        host: None,
        container: None,
    };
    let err = hop_from_arg(arg).expect_err("should fail: no container");
    match err {
        crate::function_tool::FunctionCallError::RespondToModel(msg) => {
            assert!(
                msg.contains("requires a `container` field"),
                "expected missing-container message, got: {msg}"
            );
        }
        other => panic!("expected RespondToModel, got {other:?}"),
    }
}

/// An unknown hop type must produce an error listing the valid types.
#[test]
fn hop_from_arg_unknown_type_returns_error() {
    let arg = HopArg {
        hop_type: "kubernetes".to_string(),
        host: None,
        container: None,
    };
    let err = hop_from_arg(arg).expect_err("should fail: unknown type");
    match err {
        crate::function_tool::FunctionCallError::RespondToModel(msg) => {
            assert!(
                msg.contains("unknown hop type"),
                "expected unknown-type message, got: {msg}"
            );
            // The error must name the valid types so the model can self-correct.
            assert!(msg.contains("ssh"), "must mention ssh");
            assert!(msg.contains("docker"), "must mention docker");
        }
        other => panic!("expected RespondToModel, got {other:?}"),
    }
}

/// A hop value that starts with `-` must be rejected (reserved as a CLI flag).
#[test]
fn hop_from_arg_ssh_flag_prefix_rejected() {
    let arg = HopArg {
        hop_type: "ssh".to_string(),
        host: Some("-e /etc/passwd".to_string()),
        container: None,
    };
    let err = hop_from_arg(arg).expect_err("should fail: flag-like host");
    match err {
        crate::function_tool::FunctionCallError::RespondToModel(msg) => {
            assert!(
                msg.contains("must not start with `-`"),
                "expected flag-rejection message, got: {msg}"
            );
        }
        other => panic!("expected RespondToModel, got {other:?}"),
    }
}

/// A hop value that contains `>` (the id segment separator) must be rejected.
#[test]
fn hop_from_arg_docker_greater_than_rejected() {
    let arg = HopArg {
        hop_type: "docker".to_string(),
        host: None,
        container: Some("a>b".to_string()),
    };
    let err = hop_from_arg(arg).expect_err("should fail: `>` in container name");
    match err {
        crate::function_tool::FunctionCallError::RespondToModel(msg) => {
            assert!(
                msg.contains("must not contain `>`"),
                "expected separator-rejection message, got: {msg}"
            );
        }
        other => panic!("expected RespondToModel, got {other:?}"),
    }
}

/// A valid ssh hop must produce the expected `Hop::Ssh` value.
#[test]
fn hop_from_arg_valid_ssh_succeeds() {
    let arg = HopArg {
        hop_type: "ssh".to_string(),
        host: Some("user@remote".to_string()),
        container: None,
    };
    let hop = hop_from_arg(arg).expect("valid ssh hop");
    assert_eq!(
        hop,
        Hop::Ssh {
            host: "user@remote".to_string()
        }
    );
}

/// A valid docker hop must produce the expected `Hop::Docker` value.
#[test]
fn hop_from_arg_valid_docker_succeeds() {
    let arg = HopArg {
        hop_type: "docker".to_string(),
        host: None,
        container: Some("my-container".to_string()),
    };
    let hop = hop_from_arg(arg).expect("valid docker hop");
    assert_eq!(
        hop,
        Hop::Docker {
            container: "my-container".to_string()
        }
    );
}

// ---------------------------------------------------------------------------
// EnvironmentManager metadata tests (#1 — shared metadata)
// ---------------------------------------------------------------------------

/// set_environment_metadata followed by get_environment_metadata round-trips
/// the cwd and shell values correctly.
#[test]
fn environment_manager_metadata_roundtrip() {
    let manager = EnvironmentManager::without_environments();
    manager.set_environment_metadata(
        "ssh:myhost".to_string(),
        EnvironmentMetadata {
            cwd: "/remote/home".to_string(),
            shell: Some("/bin/bash".to_string()),
        },
    );

    let meta = manager
        .get_environment_metadata("ssh:myhost")
        .expect("metadata must be present");
    assert_eq!(meta.cwd, "/remote/home");
    assert_eq!(meta.shell.as_deref(), Some("/bin/bash"));
}

/// get_environment_metadata returns None for an unknown id.
#[test]
fn environment_manager_metadata_missing_returns_none() {
    let manager = EnvironmentManager::without_environments();
    assert!(manager.get_environment_metadata("ssh:unknown").is_none());
}

/// set_environment_metadata with shell=None round-trips correctly.
#[test]
fn environment_manager_metadata_no_shell() {
    let manager = EnvironmentManager::without_environments();
    manager.set_environment_metadata(
        "docker:c".to_string(),
        EnvironmentMetadata {
            cwd: "/workspace".to_string(),
            shell: None,
        },
    );
    let meta = manager
        .get_environment_metadata("docker:c")
        .expect("metadata must be present");
    assert_eq!(meta.cwd, "/workspace");
    assert!(meta.shell.is_none());
}

/// set_last_launcher / get_last_launcher round-trip correctly.
#[test]
fn environment_manager_last_launcher_roundtrip() {
    let manager = EnvironmentManager::without_environments();
    let launcher = RemoteLauncher::ssh("myhost");
    manager.set_last_launcher("thread-123".to_string(), launcher.clone());

    let retrieved = manager
        .get_last_launcher("thread-123")
        .expect("launcher must be present");
    assert_eq!(retrieved, launcher);
}

/// get_last_launcher returns None for an unknown thread key.
#[test]
fn environment_manager_last_launcher_missing_returns_none() {
    let manager = EnvironmentManager::without_environments();
    assert!(manager.get_last_launcher("nonexistent-thread").is_none());
}

/// Overwriting metadata for the same id updates the stored value.
#[test]
fn environment_manager_metadata_overwrite() {
    let manager = EnvironmentManager::without_environments();
    manager.set_environment_metadata(
        "ssh:host".to_string(),
        EnvironmentMetadata {
            cwd: "/old".to_string(),
            shell: None,
        },
    );
    manager.set_environment_metadata(
        "ssh:host".to_string(),
        EnvironmentMetadata {
            cwd: "/new".to_string(),
            shell: Some("/bin/zsh".to_string()),
        },
    );
    let meta = manager
        .get_environment_metadata("ssh:host")
        .expect("metadata must be present");
    assert_eq!(meta.cwd, "/new");
    assert_eq!(meta.shell.as_deref(), Some("/bin/zsh"));
}

#[test]
fn environment_manager_thread_metadata_prefers_requested_thread_order() {
    let manager = EnvironmentManager::without_environments();
    manager.set_thread_environment_metadata(
        "child".to_string(),
        "ssh:shared".to_string(),
        EnvironmentMetadata {
            cwd: "/child".to_string(),
            shell: Some("/bin/sh".to_string()),
        },
    );
    manager.set_thread_environment_metadata(
        "parent".to_string(),
        "ssh:shared".to_string(),
        EnvironmentMetadata {
            cwd: "/parent".to_string(),
            shell: Some("/bin/bash".to_string()),
        },
    );

    let metadata = manager
        .get_thread_environment_metadata_for_keys(
            &["child".to_string(), "parent".to_string()],
            "ssh:shared",
        )
        .expect("thread metadata");

    assert_eq!(
        metadata,
        EnvironmentMetadata {
            cwd: "/child".to_string(),
            shell: Some("/bin/sh".to_string()),
        }
    );
}

#[tokio::test]
async fn local_switch_records_status_and_clears_remote_cursor() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let thread_key = session.thread_id.to_string();
    session
        .services
        .environment_manager
        .set_last_launcher(thread_key.clone(), RemoteLauncher::ssh("hostname"));

    handle_local_switch(&session, &turn, None)
        .await
        .expect("local switch should succeed");

    assert!(
        session
            .services
            .environment_manager
            .get_last_launcher(&thread_key)
            .is_none()
    );
    assert_eq!(
        session
            .services
            .environment_manager
            .get_last_environment_id(&thread_key)
            .as_deref(),
        Some(codex_exec_server::LOCAL_ENVIRONMENT_ID)
    );
    assert_eq!(
        session
            .services
            .environment_manager
            .get_thread_environment_ids(&thread_key),
        vec![codex_exec_server::LOCAL_ENVIRONMENT_ID.to_string()]
    );
    let resolved = resolve_tool_environment(&session, &turn, None)
        .await
        .expect("omitted environment_id should resolve")
        .expect("local environment");
    assert_eq!(
        resolved.environment_id,
        codex_exec_server::LOCAL_ENVIRONMENT_ID
    );
    assert!(!resolved.environment.is_remote());
}

#[tokio::test]
async fn local_switch_rejects_cwd() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    let result = handle_local_switch(&session, &turn, Some("/tmp".to_string())).await;
    let err = match result {
        Ok(_) => panic!("local cwd should be rejected"),
        Err(err) => err,
    };

    assert!(
        err.to_string().contains("omit `cwd`"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn resolve_tool_environment_rejects_dynamic_environment_from_other_thread() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let manager = &session.services.environment_manager;
    manager
        .upsert_environment("ssh:other".to_string(), "ws://127.0.0.1:9876".to_string())
        .expect("seed remote environment");
    manager.record_thread_environment_id("other-thread".to_string(), "ssh:other".to_string());
    manager.set_thread_environment_metadata(
        "other-thread".to_string(),
        "ssh:other".to_string(),
        EnvironmentMetadata {
            cwd: "/other".to_string(),
            shell: None,
        },
    );

    let err = resolve_tool_environment(&session, &turn, Some("ssh:other"))
        .await
        .expect_err("other thread env should not be visible");

    assert!(
        err.to_string()
            .contains("unknown turn environment id `ssh:other`"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn resolve_tool_environment_errors_when_dynamic_metadata_is_missing() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let thread_key = session.thread_id.to_string();
    let manager = &session.services.environment_manager;
    manager
        .upsert_environment("ssh:mine".to_string(), "ws://127.0.0.1:9876".to_string())
        .expect("seed remote environment");
    manager.record_thread_environment_id(thread_key, "ssh:mine".to_string());

    let err = resolve_tool_environment(&session, &turn, Some("ssh:mine"))
        .await
        .expect_err("metadata should be required");

    assert!(
        err.to_string().contains("missing cwd metadata"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn resolve_tool_environment_uses_env_switch_default_and_explicit_override() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let thread_key = session.thread_id.to_string();
    let manager = &session.services.environment_manager;
    for (environment_id, cwd, shell) in [
        ("ssh:mine", "/mine", Some("/bin/bash")),
        ("docker:other", "/other", Some("/bin/sh")),
    ] {
        manager
            .upsert_environment(
                environment_id.to_string(),
                format!(
                    "ws://127.0.0.1:{}",
                    if environment_id == "ssh:mine" {
                        8765
                    } else {
                        9876
                    }
                ),
            )
            .expect("seed remote environment");
        manager.set_thread_environment_metadata(
            thread_key.clone(),
            environment_id.to_string(),
            EnvironmentMetadata {
                cwd: cwd.to_string(),
                shell: shell.map(str::to_string),
            },
        );
        manager.record_thread_environment_id(thread_key.clone(), environment_id.to_string());
    }
    manager.set_last_environment_id(thread_key, "ssh:mine".to_string());

    let implicit = resolve_tool_environment(&session, &turn, None)
        .await
        .expect("implicit default should resolve")
        .expect("implicit environment");
    assert_eq!(implicit.environment_id, "ssh:mine");
    assert_eq!(implicit.cwd.as_path(), std::path::Path::new("/mine"));
    assert_eq!(implicit.shell.as_deref(), Some("/bin/bash"));
    assert!(implicit.environment.is_remote());

    let explicit = resolve_tool_environment(&session, &turn, Some("docker:other"))
        .await
        .expect("explicit override should resolve")
        .expect("explicit environment");
    assert_eq!(explicit.environment_id, "docker:other");
    assert_eq!(explicit.cwd.as_path(), std::path::Path::new("/other"));

    let local = resolve_tool_environment(
        &session,
        &turn,
        Some(codex_exec_server::LOCAL_ENVIRONMENT_ID),
    )
    .await
    .expect("explicit local should resolve")
    .expect("local environment");
    assert_eq!(
        local.environment_id,
        codex_exec_server::LOCAL_ENVIRONMENT_ID
    );
    assert!(!local.environment.is_remote());
}

#[tokio::test]
async fn implicit_env_switch_default_prefers_current_metadata_over_turn_snapshot() {
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    let thread_key = session.thread_id.to_string();
    let manager = &session.services.environment_manager;
    let environment = Arc::new(
        codex_exec_server::Environment::create_for_tests(Some("ws://127.0.0.1:8765".to_string()))
            .expect("remote environment"),
    );
    manager
        .upsert_environment("ssh:mine".to_string(), "ws://127.0.0.1:8765".to_string())
        .expect("seed remote environment");
    turn.environments
        .turn_environments
        .push(crate::session::turn_context::TurnEnvironment {
            environment_id: "ssh:mine".to_string(),
            environment,
            cwd: AbsolutePathBuf::from_absolute_path("/old").expect("old cwd"),
            shell: Some("/bin/bash".to_string()),
        });
    manager.set_thread_environment_metadata(
        thread_key.clone(),
        "ssh:mine".to_string(),
        EnvironmentMetadata {
            cwd: "/new".to_string(),
            shell: Some("/bin/sh".to_string()),
        },
    );
    manager.record_thread_environment_id(thread_key.clone(), "ssh:mine".to_string());
    manager.set_last_environment_id(thread_key, "ssh:mine".to_string());

    let implicit = resolve_tool_environment(&session, &turn, None)
        .await
        .expect("implicit default should resolve")
        .expect("implicit environment");
    assert_eq!(implicit.cwd.as_path(), std::path::Path::new("/new"));
    assert_eq!(implicit.shell.as_deref(), Some("/bin/sh"));

    let explicit = resolve_tool_environment(&session, &turn, Some("ssh:mine"))
        .await
        .expect("explicit environment should resolve")
        .expect("explicit environment");
    assert_eq!(explicit.cwd.as_path(), std::path::Path::new("/old"));
    assert_eq!(explicit.shell.as_deref(), Some("/bin/bash"));
}

#[tokio::test]
async fn env_switch_thread_keys_include_parent_after_current_thread() {
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    let parent_thread_id = ThreadId::new();
    turn.parent_thread_id = Some(parent_thread_id);

    assert_eq!(
        environment_thread_keys(&session, &turn),
        vec![session.thread_id.to_string(), parent_thread_id.to_string()]
    );
}

#[test]
fn validate_addressing_mode_rejects_conflicting_modes() {
    let args = EnvSwitchArgs {
        target: Some("ssh".to_string()),
        container: None,
        host: Some("example-host".to_string()),
        cwd: None,
        hops: Some(vec![HopArg {
            hop_type: "docker".to_string(),
            host: None,
            container: Some("example-container".to_string()),
        }]),
        base: None,
        extend: None,
    };

    let err = validate_addressing_mode(&args).expect_err("conflicting modes should fail");
    assert!(
        err.to_string().contains("mutually exclusive"),
        "unexpected error: {err}"
    );
}

#[test]
fn validate_addressing_mode_rejects_base_without_extend() {
    let args = EnvSwitchArgs {
        target: Some("ssh".to_string()),
        container: None,
        host: Some("example-host".to_string()),
        cwd: None,
        hops: None,
        base: Some("ssh:base".to_string()),
        extend: None,
    };

    let err = validate_addressing_mode(&args).expect_err("base without extend should fail");
    assert!(
        err.to_string().contains("only valid with relative mode"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn implicit_base_launcher_does_not_fall_back_to_parent_after_local_switch() {
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    let parent_thread_id = ThreadId::new();
    let parent_key = parent_thread_id.to_string();
    let current_key = session.thread_id.to_string();
    turn.parent_thread_id = Some(parent_thread_id);
    session
        .services
        .environment_manager
        .set_last_launcher(parent_key, RemoteLauncher::ssh("hostname"));
    session
        .services
        .environment_manager
        .set_last_environment_id(
            current_key,
            codex_exec_server::LOCAL_ENVIRONMENT_ID.to_string(),
        );

    assert_eq!(implicit_base_launcher(&session, &turn), None);
}
