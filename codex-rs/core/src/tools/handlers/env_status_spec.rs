use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use std::collections::BTreeMap;

pub(crate) const ENV_STATUS_TOOL_NAME: &str = "env_status";
pub(crate) const ENV_LIST_TOOL_NAME: &str = "env_list";

pub(crate) fn create_env_status_tool() -> ToolSpec {
    create_environment_status_tool(
        ENV_STATUS_TOOL_NAME,
        "Inspect the execution environments currently visible to this thread. Use this after \
         context compaction, after env_switch, or whenever you are unsure which \
         environment_id values are registered. This tool does not switch environments \
         and does not contact remote exec-servers; it reports the default execution \
         environment used by compatible tools that omit environment_id, the last \
         environment_id selected by env_switch, and each registered environment's kind \
         and known cwd.",
    )
}

pub(crate) fn create_env_list_tool() -> ToolSpec {
    create_environment_status_tool(
        ENV_LIST_TOOL_NAME,
        "List the execution environments currently visible to this thread. This is an alias \
         of env_status for recovering environment_id values after context compaction \
         or when choosing where the next exec_command / apply_patch / view_image \
         call should run. It is read-only: it does not switch environments and does \
         not contact remote exec-servers.",
    )
}

fn create_environment_status_tool(name: &str, description: &str) -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: name.to_string(),
        description: description.to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(BTreeMap::new(), /*required*/ None, Some(false.into())),
        output_schema: None,
    })
}
