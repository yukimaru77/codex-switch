use super::*;

fn has_parameter(tool: &ToolSpec, parameter_name: &str) -> bool {
    serde_json::to_value(tool)
        .expect("tool spec should serialize")
        .pointer(&format!("/parameters/properties/{parameter_name}"))
        .is_some()
}

#[test]
fn view_image_tool_omits_environment_id_by_default() {
    let tool = create_view_image_tool(ViewImageToolOptions {
        can_request_original_image_detail: false,
        include_environment_id: false,
    });

    assert!(!has_parameter(&tool, "environment_id"));
    assert!(has_parameter(&tool, "path"));
}

#[test]
fn view_image_tool_includes_environment_id_when_requested() {
    let tool = create_view_image_tool(ViewImageToolOptions {
        can_request_original_image_detail: true,
        include_environment_id: true,
    });

    assert!(has_parameter(&tool, "environment_id"));
    let ToolSpec::Function(function) = tool else {
        panic!("expected function tool");
    };
    let properties = function.parameters.properties.expect("properties");
    let environment_id_description = properties
        .get("environment_id")
        .and_then(|schema| schema.description.as_deref())
        .expect("environment_id description");
    assert!(environment_id_description.contains("env_status/env_list"));
    assert!(environment_id_description.contains("env_switch can update when available"));
}
