use codex_exec_server::EnvironmentManager;
use codex_exec_server::EnvironmentMetadata;
use codex_exec_server::provision::Hop;
use codex_exec_server::provision::RemoteLauncher;
use codex_exec_server::provision::posix_single_quote;
use codex_tools::ToolSpec;

use crate::tools::handlers::env_switch_spec::ENV_SWITCH_TOOL_NAME;
use crate::tools::handlers::env_switch_spec::create_env_switch_tool;
use crate::tools::registry::ToolExecutor;

use super::EnvSwitchHandler;
use super::HopArg;
use super::hop_from_arg;

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
fn spec_requires_target_field() {
    let spec = create_env_switch_tool();
    let ToolSpec::Function(tool) = spec else {
        panic!("expected ToolSpec::Function");
    };
    let params = tool.parameters;
    // The required array must contain "target"
    let required = params.required.unwrap_or_default();
    assert!(
        required.contains(&"target".to_string()),
        "required array should contain `target`, got: {required:?}"
    );
    // container / host / cwd must NOT be in required (they are optional)
    for optional in ["container", "host", "cwd"] {
        assert!(
            !required.contains(&optional.to_string()),
            "`{optional}` should not be required but was found in {required:?}"
        );
    }
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
    // SSH: ["ssh", "-T", "-q", "-o", "BatchMode=yes", "-o",
    //       "StrictHostKeyChecking=accept-new", "-o", "LogLevel=ERROR",
    //       "--", "<host>", shell_join(["sh", "-c", "<script>"])]
    // The hardening flags suppress password prompts, banners and MOTD.
    // The "--" separator prevents host names starting with "-" from being
    // misinterpreted as SSH flags.
    assert_eq!(argv[0], "ssh");
    assert_eq!(argv[1], "-T");
    assert_eq!(argv[2], "-q");
    assert_eq!(argv[3], "-o");
    assert_eq!(argv[4], "BatchMode=yes");
    assert_eq!(argv[5], "-o");
    assert_eq!(argv[6], "StrictHostKeyChecking=accept-new");
    assert_eq!(argv[7], "-o");
    assert_eq!(argv[8], "LogLevel=ERROR");
    assert_eq!(argv[9], "--");
    assert_eq!(argv[10], "user@remote");
    // The last element contains the quoted shell invocation.
    assert!(
        argv[11].starts_with("'sh'"),
        "expected ssh argv[11] to start with `'sh'`, got: {:?}",
        argv[11]
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
    // Simulate what handle_remote_switch does when explicit_cwd is Some.
    let evil_cwd = "/workspace/project;rm -rf /";
    let script = format!("mkdir -p {}", posix_single_quote(evil_cwd));
    // The complete script must equal the expected quoted form exactly.
    assert_eq!(
        script, "mkdir -p '/workspace/project;rm -rf /'",
        "expected exactly the quoted script"
    );
    // Strip the quoted argument; nothing dangerous should be left bare.
    let remainder = script.replace("'/workspace/project;rm -rf /'", "");
    assert_eq!(
        remainder, "mkdir -p ",
        "no bare metacharacters after removing quoted arg; got: {remainder:?}"
    );
}

// ---------------------------------------------------------------------------
// Relative mode: pure launcher-level logic (no network / docker required)
// ---------------------------------------------------------------------------

/// Relative mode: base="ssh:dgx" + extend=docker:c → id "ssh:dgx>docker:c"
#[test]
fn relative_mode_base_plus_extend_produces_correct_hops() {
    let base = RemoteLauncher::from_id("ssh:dgx").expect("parse base");
    let extended = base.with_appended_hop(Hop::Docker {
        container: "c".to_string(),
    });
    assert_eq!(extended.id(), "ssh:dgx>docker:c");
    assert_eq!(extended.hops.len(), 2);
    assert_eq!(
        extended.hops[0],
        Hop::Ssh {
            host: "dgx".to_string()
        }
    );
    assert_eq!(
        extended.hops[1],
        Hop::Docker {
            container: "c".to_string()
        }
    );
}

/// Relative mode starting from a two-hop base appends a third hop correctly.
#[test]
fn relative_mode_three_hop_chain() {
    let base = RemoteLauncher::from_id("ssh:bastion>ssh:dgx").expect("parse base");
    let extended = base.with_appended_hop(Hop::Docker {
        container: "ml-box".to_string(),
    });
    assert_eq!(extended.id(), "ssh:bastion>ssh:dgx>docker:ml-box");
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
