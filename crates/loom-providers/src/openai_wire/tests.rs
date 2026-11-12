use super::accumulate::{PartialToolCall, assemble_call};
use super::chunk::{
    ChunkChoice, ChunkDelta, GroqChunkEnvelope, ToolCallDelta, ToolCallFunctionDelta,
    WireCompletionDetails, WireError, WirePromptDetails, WireUsage,
};
use super::*;
use loom_kernel::model::{
    Message, ModelCapabilities, ModelErrorKind, ModelEvent, ModelRequest, StopReason, Usage,
};
use loom_kernel::tool::ToolSpec;
use loom_kernel::transcript::{
    AssistantMessage, MALFORMED_ARGUMENTS_KEY, ReasoningTrace, ToolArguments, ToolCall, ToolCallId,
    ToolResult,
};
use serde_json::json;

fn no_in_stream(_: &str) -> Option<ModelErrorKind> {
    None
}

fn policy(prefix: &str) -> FinalizePolicy<'_> {
    FinalizePolicy {
        classify_in_stream: &no_in_stream,
        classify_wire_error: &classify_wire_error,
        id_prefix: prefix,
        dialect_usage: &|_| DialectUsage::default(),
    }
}

/// A policy wired to DeepSeek's real dialect override, so the
/// `prompt_cache_hit_tokens` extraction is tested through the same
/// path the stream driver uses.
fn deepseek_policy(prefix: &str) -> FinalizePolicy<'_> {
    FinalizePolicy {
        classify_in_stream: &no_in_stream,
        classify_wire_error: &classify_wire_error,
        id_prefix: prefix,
        dialect_usage:
            &<crate::deepseek::DeepSeekProvider as crate::wire_stream::WireEndpoint>::dialect_usage,
    }
}

fn request() -> ModelRequest {
    ModelRequest {
        model: "deepseek-flash".into(),
        messages: vec![Message::User { text: "hi".into() }],
        tools: vec![],
        max_tokens: None,
        temperature: None,
        stable_prefix_messages: None,
        context_editing: None,
        reasoning_effort: None,
    }
}

fn wire_options() -> WireOptions {
    let mut extra_body = serde_json::Map::new();
    extra_body.insert("thinking".into(), json!({"type": "enabled"}));
    extra_body.insert("reasoning_effort".into(), json!("high"));
    WireOptions {
        extra_body,
        reasoning_field: None,
    }
}

#[test]
fn body_shape_minimal_request() {
    let body = build_request_body(&request(), &wire_options());
    assert_eq!(body["model"], "deepseek-flash");
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert_eq!(body["thinking"], json!({"type": "enabled"}));
    assert_eq!(body["reasoning_effort"], "high");
    assert!(body.get("tools").is_none());
    assert!(body.get("tool_choice").is_none());
    assert!(body.get("max_tokens").is_none());
    assert!(body.get("temperature").is_none());
}

#[test]
fn body_carries_tools_and_auto_choice_only_with_tools() {
    let body = build_request_body(
        &ModelRequest {
            tools: vec![ToolSpec {
                name: "read_file".into(),
                description: "reads".into(),
                parameters: json!({"type": "object"}),
            }],
            max_tokens: Some(1024),
            temperature: Some(0.2),
            stable_prefix_messages: None,
            context_editing: None,
            reasoning_effort: None,
            ..request()
        },
        &wire_options(),
    );
    assert_eq!(body["tool_choice"], "auto");
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["function"]["name"], "read_file");
    assert_eq!(body["max_tokens"], 1024);
    // f32 0.2 does not survive JSON as an exact decimal; compare
    // through the f32 the request carried.
    assert_eq!(body["temperature"].as_f64().map(|x| x as f32), Some(0.2f32));
}

#[test]
fn body_replays_reasoning_and_tool_results() {
    let call = ToolCall {
        id: ToolCallId::try_from("call_1".to_owned()).unwrap(),
        name: "read_file".into(),
        arguments: ToolArguments::try_from(json!({"path": "x.rs"})).unwrap(),
    };
    let body = build_request_body(
        &ModelRequest {
            messages: vec![
                Message::System { text: "sys".into() },
                Message::Assistant(AssistantMessage {
                    content: "reading".into(),
                    reasoning: Some(ReasoningTrace {
                        text: "  think … ".into(),
                        provider_data: None,
                    }),
                    tool_calls: vec![call],
                    stop_reason: StopReason::ToolCalls,
                }),
                Message::ToolResult(ToolResult::new(
                    ToolCallId::try_from("call_1".to_owned()).unwrap(),
                    loom_kernel::transcript::ToolStatus::Success,
                    "file body",
                    false,
                )),
            ],
            ..request()
        },
        &wire_options(),
    );
    let assistant = &body["messages"][1];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["reasoning_content"], "  think … ");
    assert_eq!(assistant["tool_calls"][0]["id"], "call_1");
    assert_eq!(
        assistant["tool_calls"][0]["function"]["arguments"],
        r#"{"path":"x.rs"}"#
    );
    let tool = &body["messages"][2];
    assert_eq!(tool["role"], "tool");
    assert_eq!(tool["tool_call_id"], "call_1");
    assert_eq!(tool["content"], "file body");
}

#[test]
fn capabilities_used_from_core() {
    // Guards the direction of the dependency: the wire module builds
    // neutral types only.
    let caps = ModelCapabilities::new(1_048_576).with_reasoning(true);
    assert!(caps.reasoning);
}

fn chunk_with(delta: ChunkDelta) -> ChatChunk {
    ChatChunk {
        choices: vec![ChunkChoice {
            delta,
            finish_reason: None,
        }],
        usage: None,
        error: None,
        x_groq: None,
    }
}
fn text_and_call_fragments() -> ChunkAccumulator {
    let mut acc = ChunkAccumulator::default();
    acc.ingest(chunk_with(ChunkDelta {
        tool_calls: vec![ToolCallDelta {
            index: Some(0),
            id: Some("call_1".into()),
            function: Some(ToolCallFunctionDelta {
                name: Some("read_file".into()),
                arguments: Some(r#"{"pa"#.into()),
            }),
        }],
        ..Default::default()
    }));
    acc.ingest(chunk_with(ChunkDelta {
        tool_calls: vec![ToolCallDelta {
            index: Some(0),
            id: None,
            function: Some(ToolCallFunctionDelta {
                name: None,
                arguments: Some(r#"th":"x.rs"}"#.into()),
            }),
        }],
        ..Default::default()
    }));
    acc
}

// A model that double-encodes the arguments object as a JSON string
// (`"{\"path\":\"x\"}"`) must not cost the run a non-retryable
// `Protocol` error; the object inside is what the model meant.
#[test]
fn assemble_call_unwraps_double_encoded_arguments() {
    let call = assemble_call(
        &PartialToolCall {
            id: Some("call-1".to_owned()),
            name: Some("read_file".to_owned()),
            arguments: "\"{\\\"path\\\":\\\"src/lib.rs\\\"}\"".to_owned(),
        },
        "p",
    );
    assert_eq!(call.arguments["path"], "src/lib.rs");
}

// An explicit `null` means the same as an absent arguments string.
#[test]
fn assemble_call_treats_null_arguments_as_empty() {
    let call = assemble_call(
        &PartialToolCall {
            id: Some("call-2".to_owned()),
            name: Some("list_files".to_owned()),
            arguments: "null".to_owned(),
        },
        "p",
    );
    assert_eq!(*call.arguments, serde_json::json!({}));
}

// Genuinely unparseable text is salvaged as a placeholder object so
// the turn can be committed and fed back to the model as an error
// result (the wire never raises a protocol error for it — the
// engine refuses dispatch and bounds the loop instead).
#[test]
fn assemble_call_salvages_unparseable_arguments() {
    let call = assemble_call(
        &PartialToolCall {
            id: Some("call-3".to_owned()),
            name: Some("read_file".to_owned()),
            arguments: "not json".to_owned(),
        },
        "p",
    );
    assert!(call.arguments_are_malformed());
    assert_eq!(
        *call.arguments,
        serde_json::json!({ MALFORMED_ARGUMENTS_KEY: "not json" })
    );
    assert_eq!(call.name, "read_file");
}

// A missing name is salvaged the same way; the engine's error
// result still tells the model which call to reissue.
#[test]
fn assemble_call_falls_back_to_an_unknown_name() {
    let call = assemble_call(
        &PartialToolCall {
            id: None,
            name: None,
            arguments: "not json".to_owned(),
        },
        "p",
    );
    assert_eq!(call.name, "unknown");
    assert!(call.arguments_are_malformed());
    assert!(call.id.as_str().starts_with("p-call-"));
}

fn finished(mut acc: ChunkAccumulator, reason: &str) -> ChunkAccumulator {
    acc.ingest(ChatChunk {
        choices: vec![ChunkChoice {
            delta: ChunkDelta::default(),
            finish_reason: Some(reason.into()),
        }],
        usage: Some(WireUsage {
            prompt_tokens: Some(10),
            completion_tokens: Some(5),
            prompt_tokens_details: Some(WirePromptDetails {
                cached_tokens: Some(4),
            }),
            completion_tokens_details: None,
            extra: Default::default(),
        }),
        error: None,
        x_groq: None,
    });
    acc
}

#[test]
fn unknown_finish_reason_with_calls_classifies_structurally() {
    // Vendors mint new benign reasons ("eos", "end_of_turn", …);
    // the run must complete, not die on an unrecognized word.
    let acc = finished(text_and_call_fragments(), "eos_custom");
    let response = acc.finalize(true, &policy("test")).unwrap();
    assert_eq!(response.message.stop_reason, StopReason::ToolCalls);
    assert_eq!(response.message.tool_calls.len(), 1);
}

#[test]
fn unknown_finish_reason_without_calls_is_end_turn() {
    let mut acc = ChunkAccumulator::default();
    acc.ingest(chunk_with(ChunkDelta {
        content: Some("hello".into()),
        ..Default::default()
    }));
    let acc = finished(acc, "eos_custom");
    let response = acc.finalize(true, &policy("test")).unwrap();
    assert_eq!(response.message.stop_reason, StopReason::EndTurn);
    assert_eq!(response.message.content, "hello");
}

#[test]
fn stop_with_calls_normalizes_to_tool_calls() {
    let acc = finished(text_and_call_fragments(), "stop");
    let response = acc.finalize(true, &policy("test")).unwrap();
    assert_eq!(response.message.stop_reason, StopReason::ToolCalls);
    assert_eq!(response.message.tool_calls.len(), 1);
    assert_eq!(response.message.tool_calls[0].name, "read_file");
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 10,
            output_tokens: 5,
            cached_input_tokens: Some(4),
            thinking_tokens: None,
            cache_creation_input_tokens: None,
            cost_usd: None,
        }
    );
}

#[test]
fn done_without_finish_reason_normalizes_structurally() {
    let mut bare = text_and_call_fragments();
    bare.ingest(ChatChunk {
        choices: vec![ChunkChoice {
            delta: ChunkDelta::default(),
            finish_reason: None,
        }],
        usage: None,
        error: None,
        x_groq: None,
    });
    let response = bare.finalize(true, &policy("test")).unwrap();
    assert_eq!(response.message.stop_reason, StopReason::ToolCalls);
}

#[test]
fn done_without_finish_reason_or_calls_is_end_turn() {
    let mut acc = ChunkAccumulator::default();
    acc.ingest(chunk_with(ChunkDelta {
        content: Some("hello".into()),
        ..Default::default()
    }));
    let response = acc.finalize(true, &policy("test")).unwrap();
    assert_eq!(response.message.stop_reason, StopReason::EndTurn);
    assert_eq!(response.message.content, "hello");
}

#[test]
fn early_close_is_transport() {
    let acc = text_and_call_fragments();
    let error = acc.finalize(false, &policy("test")).unwrap_err();
    assert_eq!(error.kind, ModelErrorKind::Transport);
    assert!(error.is_retryable());
}

#[test]
fn tool_calls_with_zero_calls_is_protocol() {
    let mut acc = ChunkAccumulator::default();
    acc.ingest(ChatChunk {
        choices: vec![ChunkChoice {
            delta: ChunkDelta::default(),
            finish_reason: Some("tool_calls".into()),
        }],
        usage: None,
        error: None,
        x_groq: None,
    });
    let error = acc.finalize(true, &policy("test")).unwrap_err();
    assert_eq!(error.kind, ModelErrorKind::Protocol);
    assert!(!error.is_retryable());
}

#[test]
fn duplicate_tool_call_ids_fail_loudly() {
    // Two distinct calls (distinct indexes) carrying the same id:
    // every id-keyed downstream surface — the transcript pairs
    // tool results with calls by id — would be ambiguous, so
    // finalize refuses instead of dispatching both.
    let mut acc = ChunkAccumulator::default();
    for index in 0..2u32 {
        acc.ingest(chunk_with(ChunkDelta {
            tool_calls: vec![ToolCallDelta {
                index: Some(index),
                id: Some("call_same".into()),
                function: Some(ToolCallFunctionDelta {
                    name: Some("read_file".into()),
                    arguments: Some("{}".into()),
                }),
            }],
            ..Default::default()
        }));
    }
    let acc = finished(acc, "tool_calls");
    let error = acc.finalize(true, &policy("test")).unwrap_err();
    assert_eq!(error.kind, ModelErrorKind::Protocol);
    assert!(!error.is_retryable());
    assert!(
        error.message.contains("call_same"),
        "names the reused id: {}",
        error.message
    );
}

#[test]
fn distinct_tool_call_ids_still_assemble() {
    let mut acc = ChunkAccumulator::default();
    for (index, id) in [(0, "call_a"), (1, "call_b")] {
        acc.ingest(chunk_with(ChunkDelta {
            tool_calls: vec![ToolCallDelta {
                index: Some(index),
                id: Some(id.into()),
                function: Some(ToolCallFunctionDelta {
                    name: Some("read_file".into()),
                    arguments: Some("{}".into()),
                }),
            }],
            ..Default::default()
        }));
    }
    let acc = finished(acc, "tool_calls");
    let response = acc.finalize(true, &policy("test")).unwrap();
    assert_eq!(response.message.tool_calls.len(), 2);
}

#[test]
fn mid_stream_client_errors_are_permanent() {
    // An unknown 4xx reported as a stream error object is a
    // request rejection — retrying re-sends the same rejected
    // request. Known codes keep their specific kinds; absent or
    // odd codes stay retryable.
    assert_eq!(
        classify_wire_error(Some(400), "bad request"),
        ModelErrorKind::InvalidRequest
    );
    assert_eq!(
        classify_wire_error(Some(404), "no route"),
        ModelErrorKind::InvalidRequest
    );
    assert!(!classify_wire_error(Some(422), "nope").is_retryable());
    assert_eq!(
        classify_wire_error(Some(429), "slow down"),
        ModelErrorKind::RateLimited
    );
    assert_eq!(
        classify_wire_error(Some(502), "boom"),
        ModelErrorKind::Server
    );
    assert_eq!(classify_wire_error(None, "mystery"), ModelErrorKind::Server);
    assert!(classify_wire_error(None, "mystery").is_retryable());
}

#[test]
fn length_with_partial_args_discards_calls() {
    let mut acc = text_and_call_fragments();
    // Corrupt the assembled arguments: second fragment missing.
    acc.ingest(chunk_with(ChunkDelta {
        tool_calls: vec![ToolCallDelta {
            index: Some(0),
            id: Some("call_1".into()),
            function: Some(ToolCallFunctionDelta {
                name: Some("read_file".into()),
                arguments: Some(r#"{"path": "x.rs"#.into()), // truncated JSON
            }),
        }],
        ..Default::default()
    }));
    let acc = finished(acc, "length");
    let response = acc.finalize(true, &policy("test")).unwrap();
    assert_eq!(response.message.stop_reason, StopReason::MaxTokens);
    assert!(response.message.tool_calls.is_empty(), "calls discarded");
}

#[test]
fn length_with_complete_call_still_discards() {
    // Truncation can land after a short, fully parseable call
    // (e.g. `{}`); Invariant 22 still forbids executing or
    // replaying it.
    let acc = finished(text_and_call_fragments(), "length");
    let response = acc.finalize(true, &policy("test")).unwrap();
    assert_eq!(response.message.stop_reason, StopReason::MaxTokens);
    assert!(
        response.message.tool_calls.is_empty(),
        "complete calls must be discarded too"
    );
}

#[test]
fn unparseable_args_is_salvaged_not_fatal() {
    let mut acc = text_and_call_fragments();
    acc.ingest(chunk_with(ChunkDelta {
        tool_calls: vec![ToolCallDelta {
            index: Some(1),
            id: Some("call_2".into()),
            function: Some(ToolCallFunctionDelta {
                name: Some("read_file".into()),
                arguments: Some(r#"{"path": "#.into()), // truncated JSON
            }),
        }],
        ..Default::default()
    }));
    let acc = finished(acc, "stop");
    let response = acc.finalize(true, &policy("test")).unwrap();
    // The malformed call is committed as a placeholder; the engine
    // refuses to dispatch it and feeds the error back as a tool
    // result instead of killing the run.
    assert_eq!(response.message.stop_reason, StopReason::ToolCalls);
    let salvaged = response
        .message
        .tool_calls
        .iter()
        .find(|call| call.id.as_str() == "call_2")
        .expect("salvaged call is committed");
    assert!(salvaged.arguments_are_malformed());
    assert_eq!(salvaged.name, "read_file");
}

#[test]
fn in_stream_failure_classified_before_early_close() {
    let mut acc = text_and_call_fragments();
    acc.ingest(ChatChunk {
        choices: vec![ChunkChoice {
            delta: ChunkDelta::default(),
            finish_reason: Some("aborted".into()),
        }],
        usage: None,
        error: None,
        x_groq: None,
    });
    fn classify(reason: &str) -> Option<ModelErrorKind> {
        match reason {
            "aborted" => Some(ModelErrorKind::Transport),
            _ => None,
        }
    }
    let error = acc
        .finalize(
            false,
            &FinalizePolicy {
                classify_in_stream: &classify,
                classify_wire_error: &classify_wire_error,
                id_prefix: "test",
                dialect_usage: &|_| DialectUsage::default(),
            },
        )
        .unwrap_err();
    assert_eq!(error.kind, ModelErrorKind::Transport);
}

#[test]
fn mid_stream_error_object_is_classified() {
    let mut acc = ChunkAccumulator::default();
    acc.ingest(ChatChunk {
        choices: vec![ChunkChoice {
            delta: ChunkDelta::default(),
            finish_reason: None,
        }],
        usage: None,
        error: Some(WireError {
            message: Some("upstream provider is overloaded".into()),
            code: Some(json!(503)),
        }),
        x_groq: None,
    });
    let error = acc.finalize(true, &policy("test")).unwrap_err();
    assert_eq!(error.kind, ModelErrorKind::Overloaded);
    assert!(error.message.contains("upstream provider is overloaded"));
}

#[test]
fn mid_stream_error_without_status_is_retryable_server() {
    let mut acc = ChunkAccumulator::default();
    acc.ingest(ChatChunk {
        choices: vec![],
        usage: None,
        error: Some(WireError {
            message: Some("the selected model is unavailable".into()),
            code: None,
        }),
        x_groq: None,
    });
    let error = acc.finalize(true, &policy("test")).unwrap_err();
    assert_eq!(error.kind, ModelErrorKind::Server);
    assert!(error.is_retryable());
}

#[test]
fn reported_cost_passes_through_usage() {
    let mut acc = ChunkAccumulator::default();
    acc.ingest(chunk_with(ChunkDelta {
        content: Some("hello".into()),
        ..Default::default()
    }));
    acc.ingest(ChatChunk {
        choices: vec![],
        usage: Some(WireUsage {
            prompt_tokens: Some(100),
            completion_tokens: Some(20),
            prompt_tokens_details: None,
            completion_tokens_details: None,
            extra: Default::default(),
        }),
        error: None,
        x_groq: None,
    });
    // OpenRouter's `cost` key is a dialect field: the policy's
    // dialect hook supplies it, the generic wire only carries it.
    let policy = FinalizePolicy {
        classify_in_stream: &no_in_stream,
        classify_wire_error: &classify_wire_error,
        id_prefix: "openrouter",
        dialect_usage: &|_| DialectUsage {
            cost_usd: Some(0.000123),
            ..Default::default()
        },
    };
    let response = acc.finalize(true, &policy).unwrap();
    assert_eq!(response.usage.cost_usd, Some(0.000123));
    assert_eq!(response.usage.input_tokens, 100);
}

#[test]
fn synthesized_id_when_provider_sent_none() {
    let mut acc = ChunkAccumulator::default();
    acc.ingest(chunk_with(ChunkDelta {
        tool_calls: vec![ToolCallDelta {
            index: Some(0),
            id: None,
            function: Some(ToolCallFunctionDelta {
                name: Some("list_dir".into()),
                arguments: Some("{}".into()),
            }),
        }],
        ..Default::default()
    }));
    let acc = finished(acc, "tool_calls");
    let response = acc.finalize(true, &policy("deepseek")).unwrap();
    assert!(
        response.message.tool_calls[0]
            .id
            .as_str()
            .starts_with("deepseek-call-"),
        "got {}",
        response.message.tool_calls[0].id.as_str()
    );

    // Uniqueness is a transcript invariant, so a second message with
    // no ids must not reuse the first message's synthesized id.
    let second = {
        let mut acc = ChunkAccumulator::default();
        acc.ingest(chunk_with(ChunkDelta {
            tool_calls: vec![ToolCallDelta {
                index: Some(0),
                id: None,
                function: Some(ToolCallFunctionDelta {
                    name: Some("list_dir".into()),
                    arguments: Some("{}".into()),
                }),
            }],
            ..Default::default()
        }));
        let acc = finished(acc, "tool_calls");
        acc.finalize(true, &policy("deepseek")).unwrap()
    };
    assert_ne!(
        response.message.tool_calls[0].id, second.message.tool_calls[0].id,
        "synthesized ids must be unique across messages"
    );
}

#[test]
fn indexless_parallel_calls_routed_by_id() {
    // A gateway that omits `index` but sends `id`s must not merge
    // parallel calls into one slot (gluing their arguments).
    let mut acc = ChunkAccumulator::default();
    acc.ingest(chunk_with(ChunkDelta {
        tool_calls: vec![
            ToolCallDelta {
                index: None,
                id: Some("a".into()),
                function: Some(ToolCallFunctionDelta {
                    name: Some("read_file".into()),
                    arguments: Some(r#"{"path":"x"}"#.into()),
                }),
            },
            ToolCallDelta {
                index: None,
                id: Some("b".into()),
                function: Some(ToolCallFunctionDelta {
                    name: Some("list_dir".into()),
                    arguments: Some("{}".into()),
                }),
            },
        ],
        ..Default::default()
    }));
    let acc = finished(acc, "tool_calls");
    let response = acc.finalize(true, &policy("deepseek")).unwrap();
    let calls = &response.message.tool_calls;
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].id.as_str(), "a");
    assert_eq!(calls[0].arguments.to_string(), r#"{"path":"x"}"#);
    assert_eq!(calls[1].id.as_str(), "b");
    assert_eq!(calls[1].arguments.to_string(), "{}");
}

#[test]
fn indexless_continuation_appends_to_sole_call() {
    let mut acc = ChunkAccumulator::default();
    acc.ingest(chunk_with(ChunkDelta {
        tool_calls: vec![ToolCallDelta {
            index: None,
            id: Some("a".into()),
            function: Some(ToolCallFunctionDelta {
                name: Some("read_file".into()),
                arguments: Some(r#"{"pa"#.into()),
            }),
        }],
        ..Default::default()
    }));
    acc.ingest(chunk_with(ChunkDelta {
        tool_calls: vec![ToolCallDelta {
            index: None,
            id: None,
            function: Some(ToolCallFunctionDelta {
                name: None,
                arguments: Some(r#"th":"x"}"#.into()),
            }),
        }],
        ..Default::default()
    }));
    let acc = finished(acc, "tool_calls");
    let response = acc.finalize(true, &policy("deepseek")).unwrap();
    let calls = &response.message.tool_calls;
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].id.as_str(), "a");
    assert_eq!(calls[0].arguments.to_string(), r#"{"path":"x"}"#);
}

#[test]
fn ambiguous_indexless_continuation_is_protocol_error() {
    // With several calls pending, an id-less index-less fragment
    // cannot be correlated; silently merging it could corrupt
    // arguments, so finalize must fail loudly.
    let mut acc = ChunkAccumulator::default();
    acc.ingest(chunk_with(ChunkDelta {
        tool_calls: vec![
            ToolCallDelta {
                index: None,
                id: Some("a".into()),
                function: Some(ToolCallFunctionDelta {
                    name: Some("read_file".into()),
                    arguments: Some("{}".into()),
                }),
            },
            ToolCallDelta {
                index: None,
                id: Some("b".into()),
                function: Some(ToolCallFunctionDelta {
                    name: Some("list_dir".into()),
                    arguments: Some("{}".into()),
                }),
            },
        ],
        ..Default::default()
    }));
    acc.ingest(chunk_with(ChunkDelta {
        tool_calls: vec![ToolCallDelta {
            index: None,
            id: None,
            function: Some(ToolCallFunctionDelta {
                name: None,
                arguments: Some(r#"{"glue"}"#.into()),
            }),
        }],
        ..Default::default()
    }));
    let acc = finished(acc, "tool_calls");
    let error = acc.finalize(true, &policy("deepseek")).unwrap_err();
    assert_eq!(error.kind, ModelErrorKind::Protocol);
}

#[test]
fn empty_arguments_string_becomes_empty_object() {
    let mut acc = ChunkAccumulator::default();
    acc.ingest(chunk_with(ChunkDelta {
        tool_calls: vec![ToolCallDelta {
            index: Some(0),
            id: Some("call_1".into()),
            function: Some(ToolCallFunctionDelta {
                name: Some("list_dir".into()),
                arguments: Some(String::new()),
            }),
        }],
        ..Default::default()
    }));
    let acc = finished(acc, "tool_calls");
    let response = acc.finalize(true, &policy("test")).unwrap();
    assert_eq!(response.message.tool_calls[0].arguments.to_string(), "{}");
}

#[test]
fn usage_only_chunk_produces_no_deltas_but_sets_usage() {
    let mut acc = ChunkAccumulator::default();
    let deltas = acc.ingest(ChatChunk {
        choices: vec![],
        usage: Some(WireUsage {
            prompt_tokens: Some(3),
            completion_tokens: Some(1),
            prompt_tokens_details: None,
            completion_tokens_details: None,
            extra: Default::default(),
        }),
        error: None,
        x_groq: None,
    });
    assert!(deltas.is_empty());
    let response = acc.finalize(true, &policy("test")).unwrap();
    assert_eq!(response.usage.cached_input_tokens, None);
}

#[test]
fn groq_dialect_streams_reasoning_from_the_plain_field() {
    // Groq names the trace `delta.reasoning` (live-verified
    // 2026-10-18) and rejects `reasoning_content` on replay, so a
    // Groq-configured accumulator must read the other key.
    let mut acc = ChunkAccumulator::default().with_reasoning_field(Some(REASONING_FIELD_PLAIN));
    let deltas = acc.ingest(ChatChunk {
        choices: vec![ChunkChoice {
            delta: ChunkDelta {
                reasoning: Some("weighing options".into()),
                ..Default::default()
            },
            finish_reason: None,
        }],
        usage: None,
        error: None,
        x_groq: None,
    });
    assert!(
        matches!(deltas.as_slice(), [ModelEvent::ReasoningDelta(text)] if text == "weighing options")
    );
    acc.ingest(ChatChunk {
        choices: vec![ChunkChoice {
            delta: ChunkDelta {
                reasoning: Some(" and the tradeoffs".into()),
                ..Default::default()
            },
            finish_reason: Some("stop".into()),
        }],
        usage: None,
        error: None,
        x_groq: None,
    });
    let response = acc.finalize(true, &policy("test")).unwrap();
    assert_eq!(
        response.message.reasoning.as_ref().map(|r| r.text.as_str()),
        Some("weighing options and the tradeoffs")
    );
}

#[test]
fn the_default_dialect_ignores_the_groq_field() {
    // Same bytes, default accumulator: `reasoning` is an unknown
    // field on the DeepSeek/GLM/NIM dialect and must not leak in.
    let mut acc = ChunkAccumulator::default();
    acc.ingest(chunk_with(ChunkDelta {
        reasoning: Some("not our dialect".into()),
        ..Default::default()
    }));
    let response = acc.finalize(true, &policy("test")).unwrap();
    assert!(response.message.reasoning.is_none());
}

#[test]
fn groq_envelope_error_classifies_like_a_standard_one() {
    // Groq reports a mid-stream failure as
    // `{"x_groq": {"error": {...}}}`, then ends the stream.
    let mut acc = ChunkAccumulator::default();
    acc.ingest(ChatChunk {
        choices: vec![],
        usage: None,
        error: None,
        x_groq: Some(GroqChunkEnvelope {
            error: Some(WireError {
                message: Some("The selected model is overloaded".into()),
                code: Some(json!("rate_limit_exceeded")),
            }),
        }),
    });
    let error = acc.finalize(true, &policy("test")).unwrap_err();
    assert!(
        error.message.contains("overloaded"),
        "the envelope message must classify: {error:?}"
    );
}

#[test]
fn reasoning_tokens_map_to_the_thinking_attribution() {
    // OpenAI-family `completion_tokens_details.reasoning_tokens`
    // lands in the provider-neutral `thinking_tokens` split.
    let mut acc = ChunkAccumulator::default();
    acc.ingest(ChatChunk {
        choices: vec![],
        usage: Some(WireUsage {
            prompt_tokens: Some(50),
            completion_tokens: Some(20),
            prompt_tokens_details: None,
            completion_tokens_details: Some(WireCompletionDetails {
                reasoning_tokens: Some(12),
            }),
            extra: Default::default(),
        }),
        error: None,
        x_groq: None,
    });
    let response = acc.finalize(true, &policy("test")).unwrap();
    assert_eq!(response.usage.thinking_tokens, Some(12));
    // The split is already inside the output total.
    assert_eq!(response.usage.output_tokens, 20);
}

#[test]
fn openai_dialect_cached_tokens_map_when_the_deepseek_field_is_absent() {
    // Z.ai/GLM reports cache hits via `prompt_tokens_details
    // .cached_tokens` instead of DeepSeek's `prompt_cache_hit_tokens`;
    // the DeepSeek field stays authoritative when both appear.
    let mut acc = ChunkAccumulator::default();
    acc.ingest(ChatChunk {
        choices: vec![],
        usage: Some(WireUsage {
            prompt_tokens: Some(100),
            completion_tokens: Some(10),
            prompt_tokens_details: Some(WirePromptDetails {
                cached_tokens: Some(40),
            }),
            completion_tokens_details: None,
            extra: Default::default(),
        }),
        error: None,
        x_groq: None,
    });
    let response = acc.finalize(true, &policy("test")).unwrap();
    assert_eq!(response.usage.cached_input_tokens, Some(40));

    // DeepSeek names the same count `prompt_cache_hit_tokens`; its
    // dialect override wins when both forms appear in one block.
    let mut acc = ChunkAccumulator::default();
    acc.ingest(ChatChunk {
        choices: vec![],
        usage: Some(WireUsage {
            prompt_tokens: Some(100),
            completion_tokens: Some(10),
            prompt_tokens_details: Some(WirePromptDetails {
                cached_tokens: Some(40),
            }),
            completion_tokens_details: None,
            extra: {
                let mut extra = serde_json::Map::new();
                extra.insert(
                    "prompt_cache_hit_tokens".to_string(),
                    serde_json::Value::from(7u64),
                );
                extra
            },
        }),
        error: None,
        x_groq: None,
    });
    let response = acc.finalize(true, &deepseek_policy("test")).unwrap();
    assert_eq!(response.usage.cached_input_tokens, Some(7));

    // With only the generic form present, DeepSeek's dialect
    // falls back to it unchanged.
    let mut acc = ChunkAccumulator::default();
    acc.ingest(ChatChunk {
        choices: vec![],
        usage: Some(WireUsage {
            prompt_tokens: Some(100),
            completion_tokens: Some(10),
            prompt_tokens_details: Some(WirePromptDetails {
                cached_tokens: Some(40),
            }),
            completion_tokens_details: None,
            extra: Default::default(),
        }),
        error: None,
        x_groq: None,
    });
    let response = acc.finalize(true, &deepseek_policy("test")).unwrap();
    assert_eq!(response.usage.cached_input_tokens, Some(40));
}

#[test]
fn null_list_fields_parse_as_default() {
    // Some OpenAI-compatible servers send explicit nulls; the chunk
    // must still parse instead of failing the whole stream.
    let chunk: ChatChunk = serde_json::from_str(r#"{"choices":null,"usage":null}"#).unwrap();
    assert!(chunk.choices.is_empty());
    let chunk: ChatChunk =
        serde_json::from_str(r#"{"choices":[{"delta":null,"finish_reason":null}],"usage":null}"#)
            .unwrap();
    assert_eq!(chunk.choices.len(), 1);
    let delta: ChunkDelta = serde_json::from_str(r#"{"tool_calls":null}"#).unwrap();
    assert!(delta.tool_calls.is_empty());
}

#[test]
fn content_filter_discards_tool_calls() {
    let acc = finished(text_and_call_fragments(), "content_filter");
    let response = acc.finalize(true, &policy("test")).unwrap();
    assert_eq!(response.message.stop_reason, StopReason::ContentFilter);
    assert!(
        response.message.tool_calls.is_empty(),
        "calls must not survive into a filtered message"
    );
}

#[test]
fn content_filter_maps_through() {
    let mut acc = ChunkAccumulator::default();
    acc.ingest(ChatChunk {
        choices: vec![ChunkChoice {
            delta: ChunkDelta::default(),
            finish_reason: Some("content_filter".into()),
        }],
        usage: None,
        error: None,
        x_groq: None,
    });
    let response = acc.finalize(true, &policy("test")).unwrap();
    assert_eq!(response.message.stop_reason, StopReason::ContentFilter);
}

// Pins the fix's replacement of the historical behavior: an
// unknown finish_reason ("quantum_decay" et al.) used to fail the
// run with a non-retryable Protocol error; it now classifies
// structurally (see the unknown_finish_reason_* tests above).
// Failure-type reasons still error, classified by the dialect
// hook in `terminal_error` before normalization runs.
