use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

pub(crate) const ENV_SWITCH_TOOL_NAME: &str = "env_switch";

/// Builds the `env_switch` tool spec exposed to the model.
///
/// `env_switch` migrates the agent's execution environment so that all
/// subsequent shell / file tools run inside a Docker container, over SSH, or
/// back on the local host — without restarting the session.
pub(crate) fn create_env_switch_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "target".to_string(),
            JsonSchema::string_enum(
                vec![json!("local"), json!("docker"), json!("ssh")],
                Some(
                    "Execution environment to switch into. Use `docker` to enter a running \
                     container, `ssh` to enter a remote host (key auth required), and `local` \
                     to return to the host machine."
                        .to_string(),
                ),
            ),
        ),
        (
            "container".to_string(),
            JsonSchema::string(Some(
                "Name or ID of the running Docker container. Required when target is `docker`."
                    .to_string(),
            )),
        ),
        (
            "host".to_string(),
            JsonSchema::string(Some(
                "SSH destination in `[user@]host` form. Required when target is `ssh`.".to_string(),
            )),
        ),
        (
            "cwd".to_string(),
            JsonSchema::string(Some(
                "Working directory to use inside the remote environment. \
                 Defaults to the remote user's $HOME when omitted."
                    .to_string(),
            )),
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: ENV_SWITCH_TOOL_NAME.to_string(),
        description: "Switch the agent's execution environment so that ALL subsequent tools \
            (shell, apply_patch, read_file, …) run in the target environment.  Use \
            target=`docker` to enter a running container (like `docker exec <c> bash` once, \
            then work naturally inside it), target=`ssh` to enter a remote host over SSH, \
            and target=`local` to return to the local machine.  The remote codex exec-server \
            is provisioned automatically if it is absent."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            Some(vec!["target".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}
