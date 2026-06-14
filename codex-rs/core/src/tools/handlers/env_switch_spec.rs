use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

pub(crate) const ENV_SWITCH_TOOL_NAME: &str = "env_switch";

/// Builds the `env_switch` tool spec exposed to the model.
///
/// `env_switch` prepares an execution target inside a Docker container, over
/// SSH, or back on the local host, and returns an id that compatible tools can
/// use on individual calls.
///
/// The tool supports three mutually-exclusive ways of specifying the target:
///
/// 1. **Relative mode** (`extend` + optional `base`): adds one transport layer
///    on top of an existing environment without rewriting the full hop list.
///    `base` is the id of the parent environment (e.g. `"ssh:example-host"`); when
///    omitted the thread's most-recently-switched-to remote environment is used.
///    `extend` describes the single hop to append (same schema as one element
///    of `hops`).
///
///    Example: already on `ssh:example-host`, enter container `example-container`:
///    `base: "ssh:example-host", extend: {"type":"docker","container":"example-container"}`
///    → environment id becomes `"ssh:example-host>docker:example-container"`.
///
/// 2. **Legacy single-hop** (`target` + optional `container` / `host`): kept
///    for backward-compatibility and ergonomics when only one transport layer
///    is needed.
///
/// 3. **Multi-hop** (`hops` array, outer-to-inner): allows composing an
///    arbitrary number of SSH and Docker transport layers, e.g. SSH into a
///    remote host and then `docker exec` into a container running there.
///
///    Example: `hops: [{"type":"ssh","host":"example-host"},{"type":"docker","container":"example-container"}]`
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
                     `ssh` to enter a remote host (key auth required), and `local` to target \
                     the host machine when local environment support is configured. Ignored when \
                     `hops` or `extend` is provided."
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
                 Omit this unless the user explicitly wants a particular remote path; \
                 do not pass the local workspace path as `cwd`. Defaults to the remote \
                 user's $HOME when omitted. `~` and `~/...` are resolved against the \
                 remote user's $HOME, not the local host's home."
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
                     [{\"type\":\"ssh\",\"host\":\"example-host\"},{\"type\":\"docker\",\"container\":\"example-container\"}]"
                        .to_string(),
                ),
            ),
        ),
        (
            "base".to_string(),
            JsonSchema::string(Some(
                "Relative mode: the environment_id of the existing environment to build upon \
                 (e.g. `\"ssh:example-host\"`). When omitted together with `extend`, the thread's \
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
                     Example: already on `ssh:example-host`, enter container `example-container`: \
                     extend={\"type\":\"docker\",\"container\":\"example-container\"} \
                     (optionally with base=\"ssh:example-host\")."
                        .to_string(),
                );
                s
            },
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: ENV_SWITCH_TOOL_NAME.to_string(),
        description: "Prepare a Docker container or SSH host as an execution target and \
            make it the default execution environment for compatible exec_command, \
            apply_patch, or view_image calls that omit `environment_id`. The returned \
            `environment_id` can still be passed explicitly on those tools to override \
            another default or make the target unambiguous for one call. \
            Use this when a task asks you to keep working on an SSH host, inside a Docker \
            container, or inside a container on an SSH host, so later compatible tools do not \
            need repeated ssh/docker wrappers. If env_switch is unavailable or cannot register \
            the target, report that fallback reason before continuing with raw ssh/docker. \
            Raw ssh/docker commands remain appropriate for one-off probes, \
            container creation/lifecycle operations, custom transport options, data transfer, \
            TTY/port-forwarding workflows, or fallback after env_switch cannot register the \
            target. \
            Addressing: target=`docker` with `container` for a local running container; \
            target=`ssh` with `host` for an SSH destination; target=`local` to make the host the \
            default execution environment again when local support is configured. \
            For nesting, use `hops` ordered outer-to-inner, for example \
            [{\"type\":\"ssh\",\"host\":\"example-host\"},{\"type\":\"docker\",\"container\":\"example-container\"}]. \
            To add one layer to an existing environment, use `extend` with an optional \
            `base`, for example base=\"ssh:example-host\" plus \
            extend={\"type\":\"docker\",\"container\":\"example-container\"}. \
            Omit `cwd` for ordinary SSH/Docker switches; only set it to an intentional \
            remote path, never to the local workspace path. \
            Remote provisioning requires the target path to support a POSIX shell plus basic \
            tools used by provisioning (`mkdir`, `chmod`, `tar`, `gzip` when upload is needed) \
            or an already-compatible codex binary in place."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            // The tool accepts three mutually-exclusive addressing modes:
            // legacy `target`, absolute `hops`, or relative `extend`.
            // Required-one-of validation is handled by the runtime so the
            // schema does not incorrectly reject `hops` / `extend` calls.
            None,
            Some(false.into()),
        ),
        output_schema: None,
    })
}
