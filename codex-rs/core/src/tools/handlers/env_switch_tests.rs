use codex_exec_server::provision::RemoteLauncher;
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
    for expected_key in ["target", "container", "host", "cwd"] {
        assert!(
            properties.contains_key(expected_key),
            "missing property `{expected_key}` in spec, found: {properties:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// RemoteLauncher argv prefix tests (exec-server provision crate)
// ---------------------------------------------------------------------------

#[test]
fn docker_launcher_argv_prefix() {
    let launcher = RemoteLauncher::Docker {
        container: "my-container".to_string(),
    };
    assert_eq!(
        launcher.argv_prefix(),
        vec!["docker", "exec", "-i", "my-container"]
    );
}

#[test]
fn ssh_launcher_argv_prefix() {
    let launcher = RemoteLauncher::Ssh {
        host: "user@remote".to_string(),
    };
    assert_eq!(launcher.argv_prefix(), vec!["ssh", "-T", "user@remote"]);
}
