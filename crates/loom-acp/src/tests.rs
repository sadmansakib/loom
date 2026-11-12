//! Unit tests for the mapping and bookkeeping seams of the server.

use std::sync::{Arc, Mutex};

use agent_client_protocol::v1::schema as acp;
use loom_core::kernel::agent::TaskError;
use loom_core::kernel::event::EventKind;
use loom_core::kernel::id::SessionId as KernelSessionId;
use loom_core::kernel::session::{TodoItem, TodoStatus};
use loom_core::kernel::transcript::ToolStatus;

use crate::mcp::forwarded_decls;
use crate::session::{SessionRegistry, SessionReservation, begin_prompt, kernel_session_id};
use crate::updates::{prompt_text, stop_reason, tool_kind, updates_for};

fn kinds(updates: &[acp::SessionUpdate]) -> String {
    updates
        .iter()
        .map(|u| match u {
            acp::SessionUpdate::AgentMessageChunk(_) => "message",
            acp::SessionUpdate::AgentThoughtChunk(_) => "thought",
            acp::SessionUpdate::ToolCall(_) => "tool_call",
            acp::SessionUpdate::ToolCallUpdate(_) => "tool_update",
            acp::SessionUpdate::Plan(_) => "plan",
            _ => "other",
        })
        .collect::<Vec<_>>()
        .join(",")
}

#[test]
fn text_and_reasoning_deltas_map_to_chunks() {
    let updates = updates_for(&EventKind::TextDelta {
        text: "hello".into(),
    });
    assert_eq!(kinds(&updates), "message");
    let acp::SessionUpdate::AgentMessageChunk(chunk) = &updates[0] else {
        panic!("must be a message chunk");
    };
    match &chunk.content {
        acp::ContentBlock::Text(text) => assert_eq!(text.text, "hello"),
        other => panic!("must be text, got {other:?}"),
    }

    let updates = updates_for(&EventKind::ReasoningDelta {
        text: "thinking".into(),
    });
    assert_eq!(kinds(&updates), "thought");
}

#[test]
fn tool_events_map_with_kind_status_and_content() {
    let call_id = loom_core::kernel::transcript::ToolCallId::try_from("call-1".to_owned())
        .expect("valid call id");
    let started = updates_for(&EventKind::ToolStarted {
        name: "run_command".into(),
        call_id: call_id.clone(),
        arguments: "{\"program\":\"cargo\"}".into(),
    });
    assert_eq!(kinds(&started), "tool_call");
    let acp::SessionUpdate::ToolCall(call) = &started[0] else {
        panic!("must be a tool call");
    };
    assert_eq!(call.title, "run_command");
    assert!(matches!(call.kind, acp::ToolKind::Execute));
    assert!(matches!(call.status, acp::ToolCallStatus::InProgress));
    assert_eq!(
        call.raw_input,
        Some(serde_json::json!({"program": "cargo"})),
        "tool arguments are structured ACP input, not a JSON-encoded string"
    );

    let completed = updates_for(&EventKind::ToolCompleted {
        name: "run_command".into(),
        call_id,
        status: ToolStatus::Error(loom_core::kernel::transcript::ToolFailure::UnknownTool),
        content_head: "boom".into(),
    });
    assert_eq!(kinds(&completed), "tool_update");
    let acp::SessionUpdate::ToolCallUpdate(update) = &completed[0] else {
        panic!("must be a tool update");
    };
    assert_eq!(
        update.fields.status,
        Some(acp::ToolCallStatus::Failed),
        "an errored call reads as failed"
    );
    let Some(content) = &update.fields.content else {
        panic!("content carried");
    };
    match &content[0] {
        acp::ToolCallContent::Content(block) => match &block.content {
            acp::ContentBlock::Text(text) => assert_eq!(text.text, "boom"),
            other => panic!("must be text, got {other:?}"),
        },
        other => panic!("must be content, got {other:?}"),
    }
}

#[test]
fn malformed_or_truncated_tool_arguments_are_omitted() {
    let call_id = loom_core::kernel::transcript::ToolCallId::try_from("call-2".to_owned())
        .expect("valid call id");
    let updates = updates_for(&EventKind::ToolStarted {
        name: "run_command".into(),
        call_id,
        arguments: "{\"program\":".into(),
    });
    let acp::SessionUpdate::ToolCall(call) = &updates[0] else {
        panic!("must be a tool call");
    };
    assert!(call.raw_input.is_none());
}

#[test]
fn prompt_reservation_is_exclusive_and_released() {
    let slot = Arc::new(Mutex::new(None));
    let Some((_, active)) = begin_prompt(&slot) else {
        panic!("first prompt reserves an idle session");
    };
    assert!(begin_prompt(&slot).is_none(), "concurrent prompt rejected");
    drop(active);
    assert!(
        begin_prompt(&slot).is_some(),
        "completed prompt releases slot"
    );
}

#[test]
fn failed_session_load_releases_its_reservation() {
    let id = acp::SessionId::new("01900000-0000-7000-8000-000000000000");
    let registry = Arc::new(Mutex::new(SessionRegistry::default()));
    let reservation =
        SessionReservation::reserve(&registry, id.clone()).expect("first open reserves the id");
    assert!(SessionReservation::reserve(&registry, id.clone()).is_none());
    drop(reservation);
    assert!(SessionReservation::reserve(&registry, id).is_some());
}

#[test]
fn tool_kind_table_covers_the_registry() {
    assert!(matches!(tool_kind("read_file"), acp::ToolKind::Read));
    assert!(matches!(tool_kind("list_dir"), acp::ToolKind::Read));
    assert!(matches!(tool_kind("artifact_read"), acp::ToolKind::Read));
    assert!(matches!(tool_kind("search"), acp::ToolKind::Search));
    assert!(matches!(tool_kind("write_file"), acp::ToolKind::Edit));
    assert!(matches!(tool_kind("edit_file"), acp::ToolKind::Edit));
    assert!(matches!(tool_kind("replace_symbol"), acp::ToolKind::Edit));
    assert!(matches!(tool_kind("run_command"), acp::ToolKind::Execute));
    assert!(matches!(tool_kind("verify"), acp::ToolKind::Execute));
    assert!(matches!(tool_kind("todo"), acp::ToolKind::Think));
    assert!(matches!(tool_kind("mystery"), acp::ToolKind::Other));
}

#[test]
fn todo_updates_map_to_plan_entries() {
    let updates = updates_for(&EventKind::TodoUpdated {
        revision: 2,
        items: vec![
            TodoItem {
                id: "1".into(),
                text: "first".into(),
                status: TodoStatus::Done,
            },
            TodoItem {
                id: "2".into(),
                text: "second".into(),
                status: TodoStatus::InProgress,
            },
        ],
    });
    assert_eq!(kinds(&updates), "plan");
    let acp::SessionUpdate::Plan(plan) = &updates[0] else {
        panic!("must be a plan");
    };
    assert_eq!(plan.entries.len(), 2);
    assert!(matches!(
        plan.entries[0].status,
        acp::PlanEntryStatus::Completed
    ));
    assert!(matches!(
        plan.entries[1].status,
        acp::PlanEntryStatus::InProgress
    ));
}

#[test]
fn harness_telemetry_maps_to_nothing() {
    for kind in [
        EventKind::TurnStarted { turn: 1 },
        EventKind::ModelCompleted {
            response: loom_core::kernel::model::ModelResponse {
                message: loom_core::kernel::transcript::AssistantMessage {
                    content: String::new(),
                    reasoning: None,
                    tool_calls: Vec::new(),
                    stop_reason: loom_core::kernel::model::StopReason::EndTurn,
                },
                usage: Default::default(),
            },
        },
        EventKind::LoopNudge {
            text: "nudge".into(),
        },
    ] {
        assert!(
            updates_for(&kind).is_empty(),
            "internal telemetry must not reach the editor"
        );
    }
}

#[test]
fn stop_reasons_map_onto_the_acp_set() {
    assert!(matches!(
        stop_reason(&TaskError::Cancelled),
        Some(acp::StopReason::Cancelled)
    ));
    assert!(matches!(
        stop_reason(&TaskError::MaxTurns { max_turns: 3 }),
        Some(acp::StopReason::MaxTurnRequests)
    ));
    assert!(matches!(
        stop_reason(&TaskError::BudgetExhausted {
            limit: "input_tokens"
        }),
        Some(acp::StopReason::MaxTokens)
    ));
    assert!(matches!(
        stop_reason(&TaskError::Paused),
        Some(acp::StopReason::EndTurn)
    ));
    assert!(
        stop_reason(&TaskError::Internal("engine".into())).is_none(),
        "hard failures surface as errors, not stop reasons"
    );
}

#[test]
fn prompt_text_flattens_text_blocks_only() {
    let blocks = vec![
        text_content("hello "),
        acp::ContentBlock::ResourceLink(acp::ResourceLink::new("link", "file:///x")),
        text_content("world"),
    ];
    assert_eq!(prompt_text(&blocks), "hello world");
}

#[test]
fn forwarded_stdio_servers_map_to_decls() {
    let server = acp::McpServerStdio::new("mock", mock_command())
        .args(vec!["--serve".into()])
        .env(vec![acp::EnvVariable::new("KEY", "value")]);
    let decls =
        forwarded_decls(&[acp::McpServer::Stdio(server)]).expect("stdio servers are supported");
    let decl = &decls["mock"];
    assert_eq!(
        decl.command,
        [
            mock_command().to_string_lossy().into_owned(),
            "--serve".into()
        ]
    );
    assert_eq!(
        decl.env.get("KEY").map(String::as_str),
        Some("value"),
        "the forwarded environment rides the scrubbed spawn"
    );
    assert!(decl.network, "the operator grant implies network");
    assert!(decl.always_load.is_empty());
}

#[test]
fn forwarded_mcp_rejects_duplicate_names_and_environment_keys() {
    let first = acp::McpServer::Stdio(acp::McpServerStdio::new("mock", mock_command()));
    let second = acp::McpServer::Stdio(acp::McpServerStdio::new("mock", mock_command()));
    assert!(forwarded_decls(&[first, second]).is_err());

    let duplicate_env =
        acp::McpServer::Stdio(acp::McpServerStdio::new("mock", mock_command()).env(vec![
            acp::EnvVariable::new("KEY", "one"),
            acp::EnvVariable::new("KEY", "two"),
        ]));
    assert!(forwarded_decls(&[duplicate_env]).is_err());
}

#[test]
fn forwarded_mcp_rejects_invalid_commands_and_environment() {
    let relative_command = acp::McpServer::Stdio(acp::McpServerStdio::new("mock", "mock-mcp"));
    assert!(forwarded_decls(&[relative_command]).is_err());

    for name in ["", "=BAD", "BAD\0NAME"] {
        let invalid_env = acp::McpServer::Stdio(
            acp::McpServerStdio::new("mock", mock_command())
                .env(vec![acp::EnvVariable::new(name, "value")]),
        );
        assert!(forwarded_decls(&[invalid_env]).is_err());
    }

    let nul_value = acp::McpServer::Stdio(
        acp::McpServerStdio::new("mock", mock_command())
            .env(vec![acp::EnvVariable::new("KEY", "bad\0value")]),
    );
    assert!(forwarded_decls(&[nul_value]).is_err());
}

#[test]
fn forwarded_mcp_rejects_platform_equivalent_environment_keys() {
    let server = acp::McpServer::Stdio(acp::McpServerStdio::new("mock", mock_command()).env(vec![
        acp::EnvVariable::new("PATH", "first"),
        acp::EnvVariable::new("Path", "second"),
    ]));
    assert_eq!(
        forwarded_decls(&[server]).is_err(),
        cfg!(windows),
        "environment names are case-insensitive only on Windows"
    );
}

#[test]
fn http_and_sse_forwarded_servers_are_rejected() {
    let http = acp::McpServer::Http(acp::McpServerHttp::new("h", "https://x"));
    let sse = acp::McpServer::Sse(acp::McpServerSse::new("s", "https://y"));
    for server in [http, sse] {
        let error = forwarded_decls(&[server]).expect_err("only stdio is supported");
        assert!(
            error.contains("stdio"),
            "error names the constraint: {error}"
        );
    }
}

#[test]
fn acp_session_ids_round_trip_with_kernel_ids() {
    let kernel = KernelSessionId::new();
    let parsed = kernel_session_id(&acp::SessionId::new(kernel.as_uuid().to_string()))
        .expect("v7 ids round-trip");
    assert_eq!(parsed, kernel);
    assert!(
        kernel_session_id(&acp::SessionId::new("not-a-uuid")).is_err(),
        "garbage is rejected"
    );
    let uppercase = acp::SessionId::new(kernel.as_uuid().to_string().to_uppercase());
    assert!(
        kernel_session_id(&uppercase).is_err(),
        "non-canonical spellings cannot alias a live session"
    );
    // A v4 id parses as a UUID but fails the v7 rule.
    let v4 = acp::SessionId::new("00000000-0000-4000-8000-000000000000");
    assert!(kernel_session_id(&v4).is_err(), "non-v7 ids are rejected");
}

fn text_content(text: &str) -> acp::ContentBlock {
    acp::ContentBlock::Text(acp::TextContent::new(text.to_owned()))
}

fn mock_command() -> std::path::PathBuf {
    if cfg!(windows) {
        std::path::PathBuf::from(r"C:\mock-mcp.exe")
    } else {
        std::path::PathBuf::from("/usr/bin/mock-mcp")
    }
}
