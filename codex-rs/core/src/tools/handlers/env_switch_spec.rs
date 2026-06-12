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
///
/// The tool supports two mutually-exclusive ways of specifying the target:
///
/// 1. **Legacy single-hop** (`target` + optional `container` / `host`): kept
///    for backward-compatibility and ergonomics when only one transport layer
///    is needed.
///
/// 2. **Multi-hop** (`hops` array, outer-to-inner): allows composing an
///    arbitrary number of SSH and Docker transport layers, e.g. SSH into a
///    remote GPU box and then `docker exec` into a container running there.
///
///    Example: `hops: [{"type":"ssh","host":"dgx"},{"type":"docker","container":"c"}]`
pub(crate) fn create_env_switch_tool() -> ToolSpec {
    // Schema for a single hop element used in the `hops` array.
    let hop_schema = JsonSchema::object(
        BTreeMap::from([
            (
                "type".to_string(),
                JsonSchema::string_enum(
                    vec![json!("ssh"), json!("docker")],
                    Some(
                        "Transport type for this hop: `ssh` to reach a remote host, \
                         `docker` to enter a running container."
                            .to_string(),
                    ),
                ),
            ),
            (
                "host".to_string(),
                JsonSchema::string(Some(
                    "SSH destination in `[user@]host` form. Required when type is `ssh`."
                        .to_string(),
                )),
            ),
            (
                "container".to_string(),
                JsonSchema::string(Some(
                    "Name or ID of the running Docker container. Required when type is `docker`."
                        .to_string(),
                )),
            ),
        ]),
        Some(vec!["type".to_string()]),
        Some(false.into()),
    );

    let properties = BTreeMap::from([
        (
            "target".to_string(),
            JsonSchema::string_enum(
                vec![json!("local"), json!("docker"), json!("ssh")],
                Some(
                    "Single-hop shorthand. Use `docker` to enter a running container, \
                     `ssh` to enter a remote host (key auth required), and `local` to return \
                     to the host machine. Ignored when `hops` is provided."
                        .to_string(),
                ),
            ),
        ),
        (
            "container".to_string(),
            JsonSchema::string(Some(
                "Name or ID of the running Docker container. Required when target is `docker` \
                 and `hops` is not provided."
                    .to_string(),
            )),
        ),
        (
            "host".to_string(),
            JsonSchema::string(Some(
                "SSH destination in `[user@]host` form. Required when target is `ssh` \
                 and `hops` is not provided."
                    .to_string(),
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
        (
            "hops".to_string(),
            JsonSchema::array(
                hop_schema,
                Some(
                    "Ordered list of transport hops from outermost to innermost. \
                     When provided, `target` / `container` / `host` are ignored. \
                     Example for SSH into a remote host then docker exec into a container: \
                     [{\"type\":\"ssh\",\"host\":\"dgx\"},{\"type\":\"docker\",\"container\":\"ml-box\"}]"
                        .to_string(),
                ),
            ),
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: ENV_SWITCH_TOOL_NAME.to_string(),
        description: "Switch the agent's execution environment so that ALL subsequent tools \
            (shell, apply_patch, read_file, …) run in the target environment. \
            Use target=`docker` to enter a running container, target=`ssh` to enter a \
            remote host over SSH, and target=`local` to return to the local machine. \
            For multi-hop routing (e.g. SSH into a remote host then docker exec into a \
            container), use the `hops` array instead of `target`. \
            The remote codex exec-server is provisioned automatically if it is absent."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            // target is required only for the legacy single-hop path; with
            // `hops` it is ignored.  We keep it in `required` so the legacy
            // path continues to work without changes to callers.
            Some(vec!["target".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}
