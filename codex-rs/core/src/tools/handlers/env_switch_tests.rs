use codex_exec_server::provision::Hop;
use codex_exec_server::provision::RemoteLauncher;
use codex_exec_server::provision::posix_single_quote;
use codex_tools::ToolSpec;

use crate::tools::handlers::env_switch_spec::ENV_SWITCH_TOOL_NAME;
use crate::tools::handlers::env_switch_spec::create_env_switch_tool;
use crate::tools::registry::ToolExecutor;

use super::EnvSwitchHandler;

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
    // Docker: ["docker", "exec", "-i", "<container>", "sh", "-c", "<script>"]
    assert_eq!(argv[0], "docker");
    assert_eq!(argv[1], "exec");
    assert_eq!(argv[2], "-i");
    assert_eq!(argv[3], "my-container");
    assert_eq!(argv[4], "sh");
    assert_eq!(argv[5], "-c");
    assert_eq!(argv[6], "echo hello");
}

#[test]
fn ssh_launcher_shell_argv_structure() {
    let launcher = RemoteLauncher::ssh("user@remote");
    let argv = launcher.shell_argv("echo hello");
    // SSH: ["ssh", "-T", "<host>", shell_join(["sh", "-c", "<script>"])]
    assert_eq!(argv[0], "ssh");
    assert_eq!(argv[1], "-T");
    assert_eq!(argv[2], "user@remote");
    // The fourth element contains the quoted shell invocation.
    assert!(
        argv[3].starts_with("'sh'"),
        "expected ssh argv[3] to start with `'sh'`, got: {:?}",
        argv[3]
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
