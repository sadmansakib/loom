use loom_eval_b6::RawModel;

fn raw(parameters: &[&str]) -> RawModel {
    serde_json::from_value(serde_json::json!({
        "id": "vendor/model",
        "context_length": 200_000,
        "supported_parameters": parameters,
    }))
    .expect("fixture model")
}

#[test]
fn tools_without_parallel_stay_serial() {
    let caps = raw(&["tools"]).capabilities();
    assert!(caps.tool_calls, "tools is listed");
    assert!(
        !caps.parallel_tool_calls,
        "parallel_tool_calls is not listed; claiming it can fail the turn"
    );
}

#[test]
fn parallel_explicitly_listed_is_honoured() {
    let caps = raw(&["tools", "parallel_tool_calls"]).capabilities();
    assert!(caps.tool_calls && caps.parallel_tool_calls);
}

#[test]
fn neither_listed_means_no_calls() {
    let caps = raw(&["temperature"]).capabilities();
    assert!(!caps.tool_calls && !caps.parallel_tool_calls);
}
