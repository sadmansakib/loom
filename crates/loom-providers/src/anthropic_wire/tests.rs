use super::*;
use loom_kernel::model::{
    Message, ModelErrorKind, ModelEvent, ModelRequest, ReasoningEffort, StopReason,
};
use loom_kernel::transcript::{
    AssistantMessage, ReasoningTrace, ToolArguments, ToolCall, ToolCallId, ToolResult, ToolStatus,
};
use serde_json::json;

fn request(messages: Vec<Message>) -> ModelRequest {
    ModelRequest {
        model: "claude-sonnet-4-5".into(),
        messages,
        tools: vec![],
        max_tokens: None,
        temperature: None,
        stable_prefix_messages: None,
        context_editing: None,
        reasoning_effort: None,
        session_id: None,
    }
}

fn assistant_call(id: &str) -> ToolCall {
    ToolCall {
        id: ToolCallId::try_from(id.to_owned()).unwrap(),
        name: "read_file".into(),
        arguments: ToolArguments::try_from(json!({"path": "x.rs"})).unwrap(),
    }
}

#[test]
fn per_turn_effort_rides_a_trailing_system_message() {
    let mut req = request(vec![Message::User { text: "hi".into() }]);
    req.reasoning_effort = Some(ReasoningEffort::High);
    let body = build_request_body(&req);
    let messages = body["messages"].as_array().unwrap();
    // The translated turn is untouched; the level lands after it.
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[1]["role"], "system");
    assert_eq!(
        messages[1]["content"][0]["text"],
        "Reasoning effort for this turn: high."
    );
}

#[test]
fn no_effort_leaves_the_wire_bytes_unchanged() {
    // The kernel default is "send nothing": a request without a
    // level must not grow a synthetic message.
    let body = build_request_body(&request(vec![Message::User { text: "hi".into() }]));
    assert_eq!(body["messages"].as_array().map(Vec::len), Some(1));
}

#[test]
fn the_effort_message_does_not_move_a_cache_breakpoint() {
    let mut req = request(vec![
        Message::System { text: "sys".into() },
        Message::User { text: "one".into() },
        Message::User { text: "two".into() },
    ]);
    req.stable_prefix_messages = Some(2);
    req.reasoning_effort = Some(ReasoningEffort::Low);
    let body = build_request_body(&req);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    // The breakpoint still marks "one", not the appended message.
    assert_eq!(
        messages[0]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
    assert_eq!(messages[2]["role"], "system");
}

#[test]
fn system_prompt_moves_to_the_top_level_parameter() {
    let body = build_request_body(&request(vec![
        Message::System { text: "sys".into() },
        Message::User { text: "hi".into() },
    ]));
    assert_eq!(body["system"], json!("sys"));
    assert_eq!(body["messages"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][0]["content"][0]["type"], "text");
    assert!(body.get("tools").is_none());
}

#[test]
fn max_tokens_is_required_so_it_defaults() {
    let body = build_request_body(&request(vec![Message::User { text: "hi".into() }]));
    assert_eq!(body["max_tokens"], json!(DEFAULT_MAX_TOKENS));
}

#[test]
fn temperature_is_never_forwarded() {
    // Claude 4.7+ answer a non-default temperature with a 400, so
    // the adapter drops the field even when the caller set one.
    let mut req = request(vec![Message::User { text: "hi".into() }]);
    req.temperature = Some(0.2);
    let body = build_request_body(&req);
    assert!(body.get("temperature").is_none());
}

#[test]
fn assistant_turns_translate_to_blocks_and_drop_reasoning() {
    let body = build_request_body(&request(vec![
        Message::System { text: "sys".into() },
        Message::Assistant(AssistantMessage {
            content: "reading".into(),
            reasoning: Some(ReasoningTrace {
                text: "plan".into(),
                provider_data: None,
            }),
            tool_calls: vec![assistant_call("toolu_1")],
            stop_reason: StopReason::ToolCalls,
        }),
        Message::ToolResult(ToolResult::new(
            ToolCallId::try_from("toolu_1".to_owned()).unwrap(),
            ToolStatus::Success,
            "file body",
            false,
        )),
    ]));
    let assistant = &body["messages"][0];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["content"][0]["type"], "text");
    assert_eq!(assistant["content"][1]["type"], "tool_use");
    assert_eq!(assistant["content"][1]["id"], "toolu_1");
    assert_eq!(assistant["content"][1]["input"], json!({"path": "x.rs"}));
    let result = &body["messages"][1];
    assert_eq!(result["role"], "user");
    assert_eq!(result["content"][0]["type"], "tool_result");
    assert_eq!(result["content"][0]["tool_use_id"], "toolu_1");
    assert_eq!(result["content"][0]["is_error"], json!(false));
    assert!(assistant.get("reasoning").is_none());
}

#[test]
fn failed_tool_results_are_flagged() {
    let body = build_request_body(&request(vec![
        Message::Assistant(AssistantMessage {
            content: String::new(),
            reasoning: None,
            tool_calls: vec![assistant_call("toolu_1")],
            stop_reason: StopReason::ToolCalls,
        }),
        Message::ToolResult(ToolResult::new(
            ToolCallId::try_from("toolu_1".to_owned()).unwrap(),
            ToolStatus::Error(loom_kernel::transcript::ToolFailure::Execution),
            "boom",
            false,
        )),
    ]));
    assert_eq!(body["messages"][1]["content"][0]["is_error"], json!(true));
}

#[test]
fn tools_become_input_schema_entries() {
    let mut req = request(vec![Message::User { text: "hi".into() }]);
    req.tools = vec![loom_kernel::tool::ToolSpec {
        name: "read_file".into(),
        description: "reads".into(),
        parameters: json!({"type": "object"}),
    }];
    let body = build_request_body(&req);
    assert_eq!(body["tools"][0]["name"], "read_file");
    assert_eq!(body["tools"][0]["input_schema"], json!({"type": "object"}));
}

#[test]
fn cache_control_lands_on_the_stable_prefix_boundary() {
    // prefix = 2 covers system + first user turn → marker on turn 0.
    let mut req = request(vec![
        Message::System { text: "sys".into() },
        Message::User {
            text: "task".into(),
        },
        Message::Assistant(AssistantMessage {
            content: "working".into(),
            reasoning: None,
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
        }),
    ]);
    req.stable_prefix_messages = Some(2);
    let body = build_request_body(&req);
    assert_eq!(
        body["messages"][0]["content"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );
    assert!(
        body["messages"][1]["content"][0]
            .get("cache_control")
            .is_none()
    );
    // prefix = 3 would mark the assistant turn instead.
    req.stable_prefix_messages = Some(3);
    let body = build_request_body(&req);
    assert!(
        body["messages"][0]["content"][0]
            .get("cache_control")
            .is_none()
    );
    assert_eq!(
        body["messages"][1]["content"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );
}

#[test]
fn cache_control_on_the_system_when_the_prefix_covers_only_it() {
    let mut req = request(vec![
        Message::System { text: "sys".into() },
        Message::User {
            text: "task".into(),
        },
    ]);
    req.stable_prefix_messages = Some(1);
    let body = build_request_body(&req);
    assert_eq!(
        body["system"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );
    assert!(
        body["messages"][0]["content"][0]
            .get("cache_control")
            .is_none()
    );
}

#[test]
fn no_cache_control_without_a_hint() {
    let body = build_request_body(&request(vec![
        Message::System { text: "sys".into() },
        Message::User { text: "hi".into() },
    ]));
    assert!(body["system"].is_string());
    assert!(
        body["messages"][0]["content"][0]
            .get("cache_control")
            .is_none()
    );
}

#[test]
fn in_turn_system_messages_stay_in_place() {
    // The Fable 5.1+ steering form: a System message after the
    // conversation has started must NOT fold into the top-level
    // `system` parameter — hoisting it would rewrite the cached
    // prefix and widen its effect to the whole request.
    let body = build_request_body(&request(vec![
        Message::System {
            text: "global prompt".into(),
        },
        Message::User {
            text: "task".into(),
        },
        Message::System {
            text: "output_config: effort high for this turn".into(),
        },
        Message::User { text: "go".into() },
    ]));
    // Only the leading system message reaches the top level.
    assert_eq!(body["system"], json!("global prompt"));
    // The in-turn system message keeps its position and role.
    assert_eq!(body["messages"].as_array().map(Vec::len), Some(3));
    assert_eq!(body["messages"][1]["role"], json!("system"));
    assert_eq!(
        body["messages"][1]["content"],
        json!([{
            "type": "text",
            "text": "output_config: effort high for this turn"
        }])
    );
    assert_eq!(body["messages"][2]["role"], json!("user"));
}

#[test]
fn a_stable_prefix_marks_turns_before_an_in_turn_system_message() {
    // Breakpoint indexing counts an in-turn system message like any
    // other turn: prefix = 2 (leading system + first user) marks
    // the user turn, never the steering message that follows it.
    let mut req = request(vec![
        Message::System { text: "sys".into() },
        Message::User {
            text: "task".into(),
        },
        Message::System {
            text: "steer".into(),
        },
        Message::User { text: "go".into() },
    ]);
    req.stable_prefix_messages = Some(2);
    let body = build_request_body(&req);
    assert_eq!(
        body["messages"][0]["content"][0]["cache_control"],
        json!({"type": "ephemeral"})
    );
    for turn in 1..3 {
        assert!(
            body["messages"][turn]["content"][0]
                .get("cache_control")
                .is_none()
        );
    }
}

#[test]
fn thinking_and_cache_creation_usage_are_accounted() {
    let mut assembler = AnthropicAssembler::default();
    for data in [
        r#"{"type":"message_start","message":{"usage":{"input_tokens":100,"cache_read_input_tokens":40,"cache_creation_input_tokens":10}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"ponder"}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-1"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":50,"output_tokens_details":{"thinking_tokens":30}}}"#,
        r#"{"type":"message_stop"}"#,
    ] {
        assembler.ingest(data).unwrap();
    }
    let response = assembler.finalize().unwrap();
    assert_eq!(
        response.message.reasoning.as_ref().map(|r| r.text.as_str()),
        Some("ponder")
    );
    // The full prompt: Anthropic's input_tokens excludes both
    // cache counters, so the kernel total is their sum.
    assert_eq!(response.usage.input_tokens, 150);
    assert_eq!(response.usage.cached_input_tokens, Some(40));
    assert_eq!(response.usage.cache_creation_input_tokens, Some(10));
    // Thinking tokens are an attribution split of the output total.
    assert_eq!(response.usage.thinking_tokens, Some(30));
    assert_eq!(response.usage.output_tokens, 50);
}

#[test]
fn statuses_map_to_stable_kinds() {
    assert_eq!(
        classify_status(reqwest::StatusCode::UNAUTHORIZED, "").kind,
        ModelErrorKind::Auth
    );
    assert_eq!(
        classify_status(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#
        )
        .kind,
        ModelErrorKind::RateLimited
    );
    assert_eq!(
        classify_status(
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: context_length_exceeded"}}"#
        )
        .kind,
        ModelErrorKind::ContextOverflow
    );
    assert_eq!(
        classify_status(
            reqwest::StatusCode::from_u16(529).unwrap(),
            r#"{"error":{"type":"overloaded_error","message":"overloaded"}}"#
        )
        .kind,
        ModelErrorKind::Overloaded
    );
}

#[test]
fn a_full_stream_assembles_into_a_response() {
    let mut assembler = AnthropicAssembler::default();
    let events: Vec<&str> = [
        r#"{"type":"message_start","message":{"usage":{"input_tokens":100,"output_tokens":1,"cache_read_input_tokens":40}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Rea"}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ding."}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read_file","input":{}}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
        r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"x.rs\"}"}}"#,
        r#"{"type":"content_block_stop","index":1}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":57}}"#,
        r#"{"type":"message_stop"}"#,
    ]
    .into();
    let mut deltas = 0;
    for data in events {
        for event in assembler.ingest(data).unwrap().0 {
            assert!(matches!(
                event,
                ModelEvent::TextDelta(_) | ModelEvent::ReasoningDelta(_)
            ));
            deltas += 1;
        }
    }
    assert_eq!(deltas, 2);
    let response = assembler.finalize().unwrap();
    assert_eq!(response.message.content, "Reading.");
    assert_eq!(response.message.stop_reason, StopReason::ToolCalls);
    assert_eq!(response.message.tool_calls.len(), 1);
    assert_eq!(response.message.tool_calls[0].name, "read_file");
    assert_eq!(response.usage.input_tokens, 140);
    assert_eq!(response.usage.cached_input_tokens, Some(40));
    assert_eq!(response.usage.output_tokens, 57);
    assert_eq!(response.usage.cost_usd, None);
}

#[test]
fn thinking_deltas_become_a_reasoning_trace() {
    let mut assembler = AnthropicAssembler::default();
    for data in [
        r#"{"type":"message_start","message":{"usage":{"input_tokens":1}}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"pondering"}}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}"#,
        r#"{"type":"message_stop"}"#,
    ] {
        assembler.ingest(data).unwrap();
    }
    let response = assembler.finalize().unwrap();
    assert_eq!(
        response.message.reasoning.as_ref().map(|r| r.text.as_str()),
        Some("pondering")
    );
    assert_eq!(response.message.stop_reason, StopReason::EndTurn);
}

#[test]
fn tool_use_with_zero_calls_is_a_protocol_error() {
    let mut assembler = AnthropicAssembler::default();
    for data in [
        r#"{"type":"message_start","message":{"usage":{}}}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{}}"#,
        r#"{"type":"message_stop"}"#,
    ] {
        assembler.ingest(data).unwrap();
    }
    assert_eq!(
        assembler.finalize().unwrap_err().kind,
        ModelErrorKind::Protocol
    );
}

#[test]
fn max_tokens_discards_partial_calls() {
    let mut assembler = AnthropicAssembler::default();
    for data in [
        r#"{"type":"message_start","message":{"usage":{}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"read_file","input":{}}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"pa"}}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{}}"#,
        r#"{"type":"message_stop"}"#,
    ] {
        assembler.ingest(data).unwrap();
    }
    let response = assembler.finalize().unwrap();
    assert_eq!(response.message.stop_reason, StopReason::MaxTokens);
    assert_eq!(response.message.tool_calls, Vec::new());
}

#[test]
fn malformed_argument_json_is_salvaged_not_fatal() {
    let mut assembler = AnthropicAssembler::default();
    for data in [
        r#"{"type":"message_start","message":{"usage":{}}}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"read_file","input":{}}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{not json"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{}}"#,
        r#"{"type":"message_stop"}"#,
    ] {
        assembler.ingest(data).unwrap();
    }
    let response = assembler.finalize().unwrap();
    assert_eq!(response.message.stop_reason, StopReason::ToolCalls);
    assert_eq!(response.message.tool_calls.len(), 1);
    let call = &response.message.tool_calls[0];
    assert_eq!(call.id.as_str(), "toolu_1");
    assert_eq!(call.name, "read_file");
    assert!(call.arguments_are_malformed());
}

#[test]
fn error_events_inside_the_stream_fail_the_request() {
    let mut assembler = AnthropicAssembler::default();
    let error = assembler
        .ingest(r#"{"type":"error","error":{"type":"overloaded_error","message":"overloaded"}}"#)
        .unwrap_err();
    assert_eq!(error.kind, ModelErrorKind::Overloaded);
}
