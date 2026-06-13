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
/// The tool supports three mutually-exclusive ways of specifying the target:
///
/// 1. **Relative mode** (`extend` + optional `base`): adds one transport layer
///    on top of an existing environment without rewriting the full hop list.
///    `base` is the id of the parent environment (e.g. `"ssh:dgx"`); when
///    omitted the thread's most-recently-switched-to remote environment is used.
///    `extend` describes the single hop to append (same schema as one element
///    of `hops`).
///
///    Example: already on `ssh:dgx`, enter container `c`:
///    `base: "ssh:dgx", extend: {"type":"docker","container":"c"}`
///    → environment id becomes `"ssh:dgx>docker:c"`.
///
/// 2. **Legacy single-hop** (`target` + optional `container` / `host`): kept
///    for backward-compatibility and ergonomics when only one transport layer
///    is needed.
///
/// 3. **Multi-hop** (`hops` array, outer-to-inner): allows composing an
///    arbitrary number of SSH and Docker transport layers, e.g. SSH into a
///    remote GPU box and then `docker exec` into a container running there.
///
///    Example: `hops: [{"type":"ssh","host":"dgx"},{"type":"docker","container":"c"}]`
pub(crate) fn create_env_switch_tool() -> ToolSpec {
    // Schema for a single hop element used in the `hops` array and `extend`.
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
                     to the host machine. Ignored when `hops` or `extend` is provided."
                        .to_string(),
                ),
            ),
        ),
        (
            "container".to_string(),
            JsonSchema::string(Some(
                "Name or ID of the running Docker container. Required when target is `docker` \
                 and `hops` / `extend` are not provided."
                    .to_string(),
            )),
        ),
        (
            "host".to_string(),
            JsonSchema::string(Some(
                "SSH destination in `[user@]host` form. Required when target is `ssh` \
                 and `hops` / `extend` are not provided."
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
                hop_schema.clone(),
                Some(
                    "Ordered list of transport hops from outermost to innermost. \
                     When provided, `target` / `container` / `host` are ignored. \
                     Example for SSH into a remote host then docker exec into a container: \
                     [{\"type\":\"ssh\",\"host\":\"dgx\"},{\"type\":\"docker\",\"container\":\"ml-box\"}]"
                        .to_string(),
                ),
            ),
        ),
        (
            "base".to_string(),
            JsonSchema::string(Some(
                "Relative mode: the environment_id of the existing environment to build upon \
                 (e.g. `\"ssh:dgx\"`). When omitted together with `extend`, the thread's \
                 most-recently-activated remote environment is used as the base. \
                 Ignored unless `extend` is also provided."
                    .to_string(),
            )),
        ),
        (
            "extend".to_string(),
            {
                let mut s = hop_schema;
                s.description = Some(
                    "Relative mode: a single hop to append to the base environment's hop list. \
                     When present, `hops` / `target` / `container` / `host` are ignored. \
                     Example: already on `ssh:dgx`, enter container `c`: \
                     extend={\"type\":\"docker\",\"container\":\"c\"} \
                     (optionally with base=\"ssh:dgx\")."
                        .to_string(),
                );
                s
            },
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: ENV_SWITCH_TOOL_NAME.to_string(),
        description: "Prepare a Docker container or SSH host as an execution target and \
            return its `environment_id`. This does NOT change where your other tools run \
            by itself: after calling env_switch, pass the returned `environment_id` on each \
            shell / exec_command / apply_patch / read_file / view_image call to run THAT \
            call inside the target; calls that omit `environment_id` keep running on the \
            local host. A single turn can therefore mix host and remote work. \
            WHEN TO USE: call this first whenever the task asks you to read, edit, run, or \
            inspect files or commands that live inside a Docker container or on a remote \
            SSH host (e.g. \"fix the bug in container web\", \"run the tests on dgx\", \
            \"edit /app/main.py in the foo container\"). Get the id, then use it on the \
            tools that do the work. You do not need the user to mention env_switch. \
            ADDRESSING: target=`docker` (with `container`) for a local container, \
            target=`ssh` (with `host`) for a remote host, target=`local` to note you are \
            back on the host. For nesting (e.g. SSH into a host then docker exec into a \
            container there), use the `hops` array [outer→inner] instead of `target`. \
            To add one layer to an environment you already created, use `extend` (one hop) \
            with an optional `base` environment_id — e.g. after `ssh:dgx`, \
            base=\"ssh:dgx\" + extend={\"type\":\"docker\",\"container\":\"c\"} yields \
            `ssh:dgx>docker:c`; omit `base` to extend the environment you most recently \
            switched into. The remote codex exec-server is provisioned automatically if \
            absent, so the target only needs a shell."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            // target is required only for the legacy single-hop path; with
            // `hops` or `extend` it is ignored.  We keep it in `required` so
            // the legacy path continues to work without changes to callers.
            Some(vec!["target".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}
