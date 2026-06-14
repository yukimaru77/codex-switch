use codex_protocol::models::VIEW_IMAGE_TOOL_NAME;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewImageToolOptions {
    pub can_request_original_image_detail: bool,
    pub include_environment_id: bool,
}

pub fn create_view_image_tool(options: ViewImageToolOptions) -> ToolSpec {
    let mut properties = BTreeMap::from([(
        "path".to_string(),
        JsonSchema::string(Some(
            "Filesystem path to an image file in the selected execution environment. Relative paths resolve against that environment's cwd."
                .to_string(),
        )),
    )]);
    if options.can_request_original_image_detail {
        properties.insert(
            "detail".to_string(),
            JsonSchema::string_enum(
                vec![json!("high"), json!("original")],
                Some(
                    "Image detail level. Defaults to `high`; use `original` to preserve exact resolution.".to_string(),
                ),
            ),
        );
    }
    if options.include_environment_id {
        properties.insert(
            "environment_id".to_string(),
            JsonSchema::string(Some(
                "Read the image from a specific execution target: pass an `environment_id` \
                 listed by env_status/env_list or returned by env_switch when available \
                 (e.g. `docker:container-name` or `ssh:hostname>docker:container-name`) \
                 to read inside that target. Omit it to use the current default execution \
                 environment, which env_switch can update when available."
                    .to_string(),
            )),
        );
    }

    ToolSpec::Function(ResponsesApiTool {
        name: VIEW_IMAGE_TOOL_NAME.to_string(),
        description: "View an image file from the selected execution environment when visual inspection is needed. Use this for images already available on disk."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(properties, Some(vec!["path".to_string()]), Some(false.into())),
        output_schema: Some(view_image_output_schema()),
    })
}

fn view_image_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "image_url": {
                "type": "string",
                "description": "Data URL for the loaded image."
            },
            "detail": {
                "type": "string",
                "enum": ["high", "original"],
                "description": "Image detail hint returned by view_image. Returns `high` for default resized behavior or `original` when original resolution is preserved."
            }
        },
        "required": ["image_url", "detail"],
        "additionalProperties": false
    })
}

#[cfg(test)]
#[path = "view_image_spec_tests.rs"]
mod tests;
