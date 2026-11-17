use super::{
    ChannelEventSink, EVENT_SCHEMA_VERSION, EventEnvelope, EventKind, EventSink, FanoutSink,
    PersistingEventSink, RetryPolicySnapshot, StatusRecorder, StatusSnapshot, StrictSink,
    TaskPhase, TurnBudgetReason, text_head,
};
use crate::context::{PruneReport, PruneTrigger, PrunedCall, StubReason};
use crate::id::{SessionId, TaskId};
use crate::model::{ModelErrorKind, ModelResponse, StopReason, Usage};
use crate::session::TodoItem;
use crate::transcript::{AssistantMessage, ToolCallId, ToolStatus};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[test]
fn sequences_are_contiguous_from_zero() {
    let (sink, mut receiver) = ChannelEventSink::new();
    let task = TaskId::new();
    sink.emit(task, EventKind::TaskStarted);
    sink.emit(task, EventKind::TurnStarted { turn: 1 });
    sink.emit(task, EventKind::ModelRequestStarted { attempt: 1 });
    sink.emit(task, EventKind::TextDelta { text: "hi".into() });
    sink.emit(task, EventKind::TaskCompleted);

    let mut expected = 0u64;
    while let Ok(event) = receiver.try_recv() {
        assert_eq!(event.sequence, expected);
        assert_eq!(event.schema_version, EVENT_SCHEMA_VERSION);
        assert_eq!(event.task_id, task);
        assert!(
            event.timestamp_millis.is_some(),
            "live envelopes carry their observation time"
        );
        assert_eq!(
            serde_json::to_value(&event).expect("event serializes")["timestamp_millis"],
            event.timestamp_millis.unwrap()
        );
        expected += 1;
    }
    assert_eq!(expected, 5);
    assert_eq!(sink.dropped(), 0);
}

#[test]
fn closed_receiver_drops_are_counted() {
    let (sink, receiver) = ChannelEventSink::new();
    drop(receiver); // consumer gone: every emit must drop, not fail

    let task = TaskId::new();
    sink.emit(task, EventKind::TaskStarted);
    sink.emit(task, EventKind::TaskStarted);
    sink.emit(task, EventKind::TaskCompleted);

    assert_eq!(sink.dropped(), 3);
}

/// Schema v3: `ContextPruned` serializes with the `snake_case` tag and
/// a fully-nested report, so an NDJSON consumer can attribute
/// savings per mechanism without kernel types.
#[test]
fn context_pruned_serializes_the_nested_report() {
    let report = PruneReport {
        trigger: PruneTrigger::AgeMask,
        calls: vec![PrunedCall {
            call_id: "c9".to_owned(),
            reason: StubReason::TurnAge,
            reclaimed_tokens: 1_234,
        }],
        reclaimed_tokens: 1_234,
    };
    let envelope = EventEnvelope {
        schema_version: EVENT_SCHEMA_VERSION,
        timestamp_millis: None,
        sequence: 7,
        task_id: TaskId::new(),
        kind: EventKind::ContextPruned { turn: 3, report },
    };
    let value = serde_json::to_value(&envelope).expect("serializes");
    assert_eq!(value["kind"]["type"], "context_pruned");
    assert_eq!(value["kind"]["turn"], 3);
    assert_eq!(value["kind"]["report"]["trigger"], "age_mask");
    assert_eq!(value["kind"]["report"]["reclaimed_tokens"], 1_234);
    assert_eq!(value["kind"]["report"]["calls"][0]["call_id"], "c9");
    assert_eq!(value["kind"]["report"]["calls"][0]["reason"], "TurnAge");
    assert_eq!(
        value["kind"]["report"]["calls"][0]["reclaimed_tokens"],
        1_234
    );
    let round: EventEnvelope = serde_json::from_value(value).expect("round-trips");
    assert_eq!(round, envelope);
}

/// Schema v4: `AwaitingInput` serializes with the `snake_case` tag and
/// the idle session's id, so a stream-only consumer can key its resume
/// loop on the marker alone.
#[test]
fn awaiting_input_serializes_the_session_id() {
    let session_id = SessionId::new();
    let envelope = EventEnvelope {
        schema_version: EVENT_SCHEMA_VERSION,
        timestamp_millis: None,
        sequence: 11,
        task_id: TaskId::new(),
        kind: EventKind::AwaitingInput { session_id },
    };
    let value = serde_json::to_value(&envelope).expect("serializes");
    assert_eq!(value["kind"]["type"], "awaiting_input");
    assert_eq!(
        value["kind"]["session_id"],
        serde_json::to_value(session_id).expect("id serializes")
    );
    let round: EventEnvelope = serde_json::from_value(value).expect("round-trips");
    assert_eq!(round, envelope);
}

/// Schema v5: `TaskPaused` serializes under the `snake_case` tag so a
/// paused (resumable) ending is distinguishable from a mid-flight
/// cancellation on the event stream alone.
#[test]
fn task_paused_serializes_with_its_tag() {
    let envelope = EventEnvelope {
        schema_version: EVENT_SCHEMA_VERSION,
        timestamp_millis: None,
        sequence: 7,
        task_id: TaskId::new(),
        kind: EventKind::TaskPaused,
    };
    let value = serde_json::to_value(&envelope).expect("serializes");
    assert_eq!(value["kind"]["type"], "task_paused");
    let round: EventEnvelope = serde_json::from_value(value).expect("round-trips");
    assert_eq!(round, envelope);
}

/// Schema v8: the surfaced-decision kinds serialize under their
/// `snake_case` tags and round-trip, so NDJSON consumers can read
/// harness decisions straight off the event stream.
#[test]
fn surfaced_decisions_serialize_and_round_trip() {
    let cases = vec![
        (
            EventKind::FastPathDeclined {
                reason: "intent-rename-unsolved".to_owned(),
            },
            "fast_path_declined",
            serde_json::json!({ "reason": "intent-rename-unsolved" }),
        ),
        // v15: the rename decline reasons split per cause (item
        // 34); the wire stays a free string, so the new values
        // round-trip beside the legacy coarse one.
        (
            EventKind::FastPathDeclined {
                reason: "intent-rename-collision".to_owned(),
            },
            "fast_path_declined",
            serde_json::json!({ "reason": "intent-rename-collision" }),
        ),
        (
            EventKind::CapabilitiesResolved {
                model: "z-ai/glm-5.3-flash".to_owned(),
                source: "catalog".to_owned(),
            },
            "capabilities_resolved",
            serde_json::json!({
                "model": "z-ai/glm-5.3-flash",
                "source": "catalog"
            }),
        ),
        (
            EventKind::HarnessNotice {
                category: "sandbox.passthrough".to_owned(),
                detail: "\"echo\" ran unconfined".to_owned(),
            },
            "harness_notice",
            serde_json::json!({
                "category": "sandbox.passthrough",
                "detail": "\"echo\" ran unconfined"
            }),
        ),
    ];
    for (kind, tag, fields) in cases {
        let envelope = EventEnvelope {
            schema_version: EVENT_SCHEMA_VERSION,
            timestamp_millis: None,
            sequence: 7,
            task_id: TaskId::new(),
            kind,
        };
        let value = serde_json::to_value(&envelope).expect("serializes");
        assert_eq!(value["kind"]["type"], tag);
        for (field, expected) in fields.as_object().expect("object") {
            assert_eq!(&value["kind"][field], expected, "{tag}.{field}");
        }
        let round: EventEnvelope = serde_json::from_value(value).expect("round-trips");
        assert_eq!(round, envelope);
    }
}

#[test]
fn context_progress_serializes_budget_census_and_unbounded_resources() {
    use crate::{
        agent::RemainingBudget,
        context::{ContextBudget, ContextCensus},
    };

    let census: ContextCensus = serde_json::from_value(serde_json::json!({
        "system_prompt": 10,
        "repo_facts": 20,
        "user_items": 30,
        "assistant_history": 40,
        "reasoning_replay": 50,
        "tool_results": 60,
        "tool_specs": 70,
        "total": 280
    }))
    .expect("census fixture");
    let remaining: RemainingBudget = serde_json::from_value(serde_json::json!({
        "input_tokens": 0,
        "output_tokens": null,
        "total_tokens": 7,
        "tool_calls": null,
        "wall_time": null
    }))
    .expect("remaining-budget fixture");
    let envelope = EventEnvelope {
        schema_version: EVENT_SCHEMA_VERSION,
        timestamp_millis: None,
        sequence: 0,
        task_id: TaskId::new(),
        kind: EventKind::ContextProgress {
            turn: 2,
            context_budget: ContextBudget {
                max_input_tokens: 900,
                reserved_output_tokens: 100,
            },
            census,
            remaining_budget: remaining,
        },
    };

    let encoded = serde_json::to_value(&envelope).expect("event serializes");
    assert_eq!(encoded["schema_version"], EVENT_SCHEMA_VERSION);
    assert_eq!(encoded["kind"]["type"], "context_progress");
    assert_eq!(encoded["kind"]["census"]["total"], 280);
    assert_eq!(encoded["kind"]["remaining_budget"]["input_tokens"], 0);
    assert!(encoded["kind"]["remaining_budget"]["output_tokens"].is_null());
    let decoded: EventEnvelope = serde_json::from_value(encoded).expect("event round-trips");
    assert_eq!(decoded, envelope);
}

#[test]
fn dyn_sink_reports_real_drop_count() {
    // The agent loop will hold `&dyn EventSink`; the default
    // `dropped() == 0` must not mask a dropping sink's count.
    let (sink, receiver) = ChannelEventSink::new();
    drop(receiver);
    let dyn_sink: &dyn EventSink = &sink;
    dyn_sink.emit(TaskId::new(), EventKind::TaskStarted);
    assert_eq!(dyn_sink.dropped(), 1);
}

#[test]
fn slow_consumer_queues_instead_of_dropping() {
    // Unbounded by design (ADR-004): a consumer that never receives
    // while a token-rate burst is emitted must lose nothing — the
    // whole burst queues and drains in order afterwards.
    let (sink, mut receiver) = ChannelEventSink::new();
    let task = TaskId::new();
    for i in 0..1000 {
        sink.emit(
            task,
            EventKind::TextDelta {
                text: i.to_string(),
            },
        );
    }
    assert_eq!(sink.dropped(), 0);

    let mut expected = 0u64;
    while let Ok(event) = receiver.try_recv() {
        assert_eq!(event.sequence, expected);
        expected += 1;
    }
    assert_eq!(expected, 1000);
}

#[test]
fn clones_share_sequence_and_drop_count() {
    let (sink, mut receiver) = ChannelEventSink::new();
    let cloned = sink.clone();
    let task = TaskId::new();
    sink.emit(task, EventKind::TaskStarted);
    cloned.emit(task, EventKind::TaskStarted);

    let a = receiver.try_recv().unwrap();
    let b = receiver.try_recv().unwrap();
    assert_eq!((a.sequence, b.sequence), (0, 1));
    drop(receiver);
    cloned.emit(task, EventKind::TaskCancelled);
    assert_eq!(sink.dropped(), 1);
    assert_eq!(cloned.dropped(), 1);
}

#[test]
fn concurrent_emitters_arrive_in_sequence_order() {
    // The send-side lock spans numbering and send, so even with
    // several threads emitting, the receiver sees sequences in
    // strictly increasing order — no buffering-to-sort needed.
    let (sink, mut receiver) = ChannelEventSink::new();
    let threads = 4;
    let per_thread = 250;

    let handles: Vec<_> = (0..threads)
        .map(|_| {
            let sink = sink.clone();
            std::thread::spawn(move || {
                let task = TaskId::new();
                for _ in 0..per_thread {
                    sink.emit(
                        task,
                        EventKind::TextDelta {
                            text: String::new(),
                        },
                    );
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }
    assert_eq!(sink.dropped(), 0);

    let mut last = None;
    let mut count = 0u64;
    while let Ok(event) = receiver.try_recv() {
        if let Some(last) = last {
            assert_eq!(event.sequence, last + 1, "arrival order broke");
        } else {
            assert_eq!(event.sequence, 0);
        }
        last = Some(event.sequence);
        count += 1;
    }
    assert_eq!(count, threads * per_thread);
    assert_eq!(last, Some(count - 1));
}

#[test]
fn envelope_round_trips_through_json() {
    let response = ModelResponse {
        message: AssistantMessage {
            content: "done".into(),
            reasoning: None,
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
        },
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
            cached_input_tokens: Some(3),
            thinking_tokens: None,
            cache_creation_input_tokens: None,
            cost_usd: None,
        },
    };
    let envelope = EventEnvelope {
        schema_version: EVENT_SCHEMA_VERSION,
        timestamp_millis: None,
        sequence: 7,
        task_id: TaskId::new(),
        kind: EventKind::ModelCompleted { response },
    };

    let json = serde_json::to_string(&envelope).unwrap();
    let back: EventEnvelope = serde_json::from_str(&json).unwrap();
    assert_eq!(envelope, back);
}

#[test]
fn event_kinds_use_snake_case_tags() {
    let task = TaskId::new();
    let retry = EventEnvelope {
        schema_version: EVENT_SCHEMA_VERSION,
        timestamp_millis: None,
        sequence: 0,
        task_id: task,
        kind: EventKind::ModelRetry {
            attempt: 2,
            kind: ModelErrorKind::RateLimited,
            delay_ms: 4000,
            response_started: true,
            http_status: Some(429),
            policy: Some(RetryPolicySnapshot {
                max_attempts: 4,
                initial_backoff_ms: 2000,
                backoff_factor: 2,
                max_backoff_ms: 30_000,
                jitter_percent: 25,
            }),
        },
    };
    let json = serde_json::to_value(&retry).unwrap();
    assert_eq!(json["kind"]["type"], "model_retry");
    assert_eq!(json["kind"]["kind"], "rate_limited");
    assert_eq!(json["kind"]["attempt"], 2);
    assert_eq!(json["kind"]["delay_ms"], 4000);
    assert_eq!(json["kind"]["response_started"], true);
    assert_eq!(json["kind"]["http_status"], 429);
    assert_eq!(json["kind"]["policy"]["max_attempts"], 4);
    assert_eq!(json["kind"]["policy"]["jitter_percent"], 25);
}

#[test]
fn task_failed_keeps_structured_failure_metadata() {
    let envelope = EventEnvelope {
        schema_version: EVENT_SCHEMA_VERSION,
        timestamp_millis: None,
        sequence: 0,
        task_id: TaskId::new(),
        kind: EventKind::TaskFailed {
            reason: "model request failed: [quota_exhausted] HTTP 402".to_owned(),
            model_kind: Some(ModelErrorKind::QuotaExhausted),
            http_status: Some(402),
            stop_reason: None,
        },
    };
    let back: EventEnvelope =
        serde_json::from_value(serde_json::to_value(envelope).unwrap()).unwrap();
    let EventKind::TaskFailed {
        model_kind,
        http_status,
        stop_reason,
        ..
    } = back.kind
    else {
        panic!("expected task_failed");
    };
    assert_eq!(model_kind, Some(ModelErrorKind::QuotaExhausted));
    assert_eq!(http_status, Some(402));
    assert_eq!(stop_reason, None);
}

#[test]
fn strict_sink_cancels_the_task_on_the_first_drop() {
    // The motivating consumer (ADR-004 addendum, item 3): evaluation
    // must not silently accept a truncated trace.
    let (inner, receiver) = ChannelEventSink::new();
    drop(receiver); // consumer gone: the next emit drops
    let token = CancellationToken::new();
    let sink = StrictSink::new(inner, token.clone());

    assert!(!token.is_cancelled());
    sink.emit(TaskId::new(), EventKind::TaskStarted);
    assert!(token.is_cancelled(), "loss must trip the task's token");
    assert_eq!(sink.dropped(), 1);
}

#[test]
fn strict_sink_is_a_transparent_passthrough_when_nothing_is_dropped() {
    let (inner, mut receiver) = ChannelEventSink::new();
    let token = CancellationToken::new();
    let sink = StrictSink::new(inner, token.clone());
    let task = TaskId::new();

    sink.emit(task, EventKind::TaskStarted);
    sink.emit(task, EventKind::TaskCompleted);

    assert!(!token.is_cancelled());
    assert_eq!(sink.dropped(), 0);
    assert_eq!(receiver.try_recv().unwrap().sequence, 0);
    assert_eq!(receiver.try_recv().unwrap().sequence, 1);
}

#[test]
fn strict_sink_never_cancels_for_a_sink_that_cannot_drop() {
    // Wrapping an in-process tally (whose `dropped()` is the default 0)
    // must not spuriously cancel.
    struct Null;
    impl EventSink for Null {
        fn emit(&self, _task_id: TaskId, _kind: EventKind) {}
    }

    let token = CancellationToken::new();
    let sink = StrictSink::new(Null, token.clone());
    for _ in 0..10 {
        sink.emit(TaskId::new(), EventKind::TurnStarted { turn: 1 });
    }
    assert!(!token.is_cancelled());
    assert_eq!(sink.dropped(), 0);
}

/// An event store that always fails, to pin the best-effort
/// contract: persistence failures are counted, never propagated.
struct FailingStore;

impl crate::store::EventStore for FailingStore {
    fn append_event(
        &self,
        _session: &SessionId,
        _envelope: &EventEnvelope,
    ) -> Result<(), crate::store::StoreError> {
        Err(crate::store::StoreError::Backend("disk gone".to_owned()))
    }

    fn load_events(
        &self,
        _session: &SessionId,
    ) -> Result<Vec<EventEnvelope>, crate::store::StoreError> {
        Ok(Vec::new())
    }
}

#[test]
fn persisting_sink_round_trips_through_the_store() {
    use crate::store::{EventStore, MemoryEventStore};

    let session = SessionId::new();
    let store = std::sync::Arc::new(MemoryEventStore::with_session(session));
    let (inner, mut receiver) = ChannelEventSink::new();

    let sink = PersistingEventSink::new(store.clone(), session, &inner);
    let task = TaskId::new();
    sink.emit(task, EventKind::TaskStarted);
    sink.emit(task, EventKind::TurnStarted { turn: 1 });
    sink.emit(
        task,
        EventKind::ModelRetry {
            attempt: 1,
            kind: ModelErrorKind::RateLimited,
            delay_ms: 500,
            response_started: false,
            http_status: None,
            policy: None,
        },
    );

    // The inner consumer still receives everything, in order.
    let mut seen = 0u64;
    while let Ok(event) = receiver.try_recv() {
        assert_eq!(event.sequence, seen);
        seen += 1;
    }
    assert_eq!(seen, 3);

    // The stored stream round-trips intact: same envelopes, same
    // order, sequence contiguous from zero.
    let stored = store.load_events(&session).unwrap();
    assert_eq!(stored.len(), 3);
    for (index, event) in stored.iter().enumerate() {
        assert_eq!(event.sequence, index as u64);
        assert_eq!(event.task_id, task);
        assert_eq!(event.schema_version, EVENT_SCHEMA_VERSION);
        assert!(
            event.timestamp_millis.is_some(),
            "persisted envelopes keep their observation time"
        );
    }
    assert_eq!(
        stored[2].kind,
        EventKind::ModelRetry {
            attempt: 1,
            kind: ModelErrorKind::RateLimited,
            delay_ms: 500,
            response_started: false,
            http_status: None,
            policy: None,
        }
    );
    assert_eq!(sink.persist_failures(), 0);
    assert_eq!(sink.dropped(), 0);
}

#[test]
fn token_deltas_stream_but_never_persist() {
    // Review S4: TextDelta/ReasoningDelta dominated the events
    // table (one synchronous INSERT per provider chunk). They
    // forward live — the inner consumer must still number them —
    // while the store only sees the durable kinds, contiguously.
    use crate::store::{EventStore, MemoryEventStore};

    let session = SessionId::new();
    let store = std::sync::Arc::new(MemoryEventStore::with_session(session));
    let (inner, mut receiver) = ChannelEventSink::new();

    let sink = PersistingEventSink::new(store.clone(), session, &inner);
    let task = TaskId::new();
    sink.emit(task, EventKind::TaskStarted);
    sink.emit(task, EventKind::TextDelta { text: "tok".into() });
    sink.emit(
        task,
        EventKind::ReasoningDelta {
            text: "thinking".into(),
        },
    );
    sink.emit(task, EventKind::TaskCompleted);

    // Live: all four arrive, numbered by the inner sink.
    let mut seen = 0u64;
    while let Ok(event) = receiver.try_recv() {
        assert_eq!(event.sequence, seen);
        seen += 1;
    }
    assert_eq!(seen, 4);

    // Durable: only the two non-delta kinds, contiguous from zero.
    let stored = store.load_events(&session).unwrap();
    assert_eq!(stored.len(), 2, "{stored:?}");
    assert!(matches!(stored[0].kind, EventKind::TaskStarted));
    assert!(matches!(stored[1].kind, EventKind::TaskCompleted));
    assert_eq!(stored[1].sequence, 1, "no sequence gaps in the store");
    assert_eq!(sink.persist_failures(), 0);
    assert_eq!(sink.dropped(), 0);
}

#[test]
fn persisting_failures_are_counted_and_never_fatal() {
    let (inner, receiver) = ChannelEventSink::new();
    drop(receiver); // the inner consumer is gone too: both loss paths fire

    let sink =
        PersistingEventSink::new(std::sync::Arc::new(FailingStore), SessionId::new(), &inner);
    let task = TaskId::new();
    sink.emit(task, EventKind::TaskStarted);
    sink.emit(task, EventKind::TaskCompleted);

    assert_eq!(sink.persist_failures(), 2);
    assert_eq!(sink.dropped(), 4, "two persist failures + two inner drops");
}

#[test]
fn persisting_sink_delegates_drops_when_the_store_is_healthy() {
    use crate::store::MemoryEventStore;

    // A memory store cannot fail, but the session is unknown: that
    // is a Corrupt append failure — the exact partial-outage shape
    // the counter must catch without failing the emit.
    let (inner, mut receiver) = ChannelEventSink::new();
    let sink = PersistingEventSink::new(
        std::sync::Arc::new(MemoryEventStore::new()),
        SessionId::new(),
        &inner,
    );
    sink.emit(TaskId::new(), EventKind::TaskStarted);
    assert_eq!(sink.persist_failures(), 1, "unknown session is a failure");
    assert!(receiver.try_recv().is_ok(), "the event still forwarded");
}

#[test]
fn fanout_delivers_every_event_to_every_consumer() {
    let (a, mut rx_a) = ChannelEventSink::new();
    let (b, mut rx_b) = ChannelEventSink::new();
    let sink = FanoutSink::new().with(a).with(b);
    let task = TaskId::new();

    sink.emit(task, EventKind::TaskStarted);
    sink.emit(task, EventKind::TextDelta { text: "x".into() });
    sink.emit(task, EventKind::TaskCompleted);

    // Each consumer numbers its own stream from zero.
    for rx in [&mut rx_a, &mut rx_b] {
        let mut expected = 0u64;
        while let Ok(event) = rx.try_recv() {
            assert_eq!(event.sequence, expected);
            assert_eq!(event.task_id, task);
            expected += 1;
        }
        assert_eq!(expected, 3);
    }
    assert_eq!(sink.dropped(), 0);
}

#[test]
fn fanout_counts_drops_but_keeps_healthy_consumers_fed() {
    let (dead, receiver) = ChannelEventSink::new();
    drop(receiver); // this consumer is gone: its events must drop
    let (live, mut rx_live) = ChannelEventSink::new();
    let sink = FanoutSink::new().with(dead).with(live);
    let task = TaskId::new();

    sink.emit(task, EventKind::TaskStarted);
    sink.emit(task, EventKind::TaskCompleted);

    assert_eq!(sink.dropped(), 2, "one drop per event on the closed child");
    assert_eq!(
        rx_live.try_recv().unwrap().kind,
        EventKind::TaskStarted,
        "a dead sibling must not starve healthy consumers"
    );
    assert_eq!(rx_live.try_recv().unwrap().kind, EventKind::TaskCompleted);
}

#[test]
fn empty_fanout_observes_nothing() {
    let sink = FanoutSink::new();
    sink.emit(TaskId::new(), EventKind::TaskStarted);
    assert_eq!(sink.dropped(), 0);
}

#[test]
fn fanout_shares_children_through_arc_handles() {
    // `with_shared` is for callers that keep a handle to one child
    // (status polls); the child must stay shared, not owned.
    let (child, mut rx) = ChannelEventSink::new();
    let shared: Arc<dyn EventSink> = Arc::new(child);
    let sink = FanoutSink::new().with_shared(shared.clone());

    sink.emit(TaskId::new(), EventKind::TaskStarted);
    assert!(rx.try_recv().is_ok(), "shared child receives");
    assert_eq!(shared.dropped(), 0, "handle observes the child state");

    drop(sink);
    assert_eq!(
        Arc::strong_count(&shared),
        1,
        "caller keeps the child alive"
    );
}

#[test]
fn status_recorder_tracks_a_task_lifecycle() {
    let recorder = StatusRecorder::new();
    let task = TaskId::new();
    let call = ToolCallId::try_from("call_1".to_owned()).unwrap();

    // Before anything: idle with no fields set.
    let idle = recorder.snapshot();
    assert_eq!(idle.phase, TaskPhase::Idle);
    assert_eq!(idle.task_id, None);

    recorder.emit(task, EventKind::TaskStarted);
    assert_eq!(recorder.snapshot().phase, TaskPhase::Running);

    recorder.emit(task, EventKind::TurnStarted { turn: 1 });
    assert_eq!(recorder.snapshot().turn, Some(1));

    recorder.emit(task, EventKind::ModelRequestStarted { attempt: 1 });
    assert_eq!(recorder.snapshot().attempt, Some(1));

    recorder.emit(task, EventKind::TextDelta { text: "…".into() });
    assert_eq!(
        recorder.snapshot().attempt,
        Some(1),
        "deltas keep the attempt"
    );

    let response = ModelResponse {
        message: AssistantMessage {
            content: String::new(),
            reasoning: None,
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
        },
        usage: Usage {
            input_tokens: 1,
            output_tokens: 1,
            cached_input_tokens: None,
            thinking_tokens: None,
            cache_creation_input_tokens: None,
            cost_usd: None,
        },
    };
    recorder.emit(task, EventKind::ModelCompleted { response });
    assert_eq!(
        recorder.snapshot().attempt,
        None,
        "completed clears the attempt"
    );

    recorder.emit(
        task,
        EventKind::ToolStarted {
            name: "read_file".into(),
            call_id: call.clone(),
            arguments: "{}".into(),
        },
    );
    assert_eq!(
        recorder.snapshot().in_flight_tools,
        vec!["read_file".to_owned()]
    );

    recorder.emit(
        task,
        EventKind::ToolCompleted {
            name: "read_file".into(),
            call_id: call,
            status: ToolStatus::Success,
            content_head: "ok".into(),
        },
    );
    assert_eq!(recorder.snapshot().in_flight_tools, Vec::<String>::new());

    recorder.emit(task, EventKind::TaskCompleted);
    let done = recorder.snapshot();
    assert_eq!(done.phase, TaskPhase::Completed);
    assert_eq!(done.task_id, Some(task));
    assert_eq!(done.turn, Some(1), "fields persist past the terminal event");

    let session = SessionId::new();
    recorder.emit(
        task,
        EventKind::AwaitingInput {
            session_id: session,
        },
    );
    let idle = recorder.snapshot();
    assert_eq!(idle.phase, TaskPhase::Idle);
    assert_eq!(idle.session_id, Some(session));
}

#[test]
fn status_recorder_shares_one_snapshot_across_clones() {
    // The driving pattern: fan a clone out to the task, keep the
    // original for polling — both must see the same fold state.
    let recorder = StatusRecorder::new();
    let (channel, mut receiver) = ChannelEventSink::new();
    let sink = FanoutSink::new().with(recorder.clone()).with(channel);
    let task = TaskId::new();

    sink.emit(task, EventKind::TaskStarted);
    sink.emit(task, EventKind::TurnStarted { turn: 2 });

    assert_eq!(recorder.snapshot().phase, TaskPhase::Running);
    assert_eq!(recorder.snapshot().turn, Some(2));
    assert_eq!(sink.dropped(), 0, "the tally cannot drop");
    assert_eq!(
        receiver.try_recv().unwrap().kind,
        EventKind::TaskStarted,
        "siblings still receive"
    );
}

#[test]
fn status_recorder_keeps_concurrent_tool_calls_distinct() {
    let recorder = StatusRecorder::new();
    let task = TaskId::new();
    let a = ToolCallId::try_from("call_a".to_owned()).unwrap();
    let b = ToolCallId::try_from("call_b".to_owned()).unwrap();

    recorder.emit(
        task,
        EventKind::ToolStarted {
            name: "shell".into(),
            call_id: a.clone(),
            arguments: "{}".into(),
        },
    );
    recorder.emit(
        task,
        EventKind::ToolStarted {
            name: "shell".into(),
            call_id: b.clone(),
            arguments: "{}".into(),
        },
    );
    assert_eq!(
        recorder.snapshot().in_flight_tools,
        vec!["shell".to_owned(), "shell".to_owned()]
    );

    recorder.emit(
        task,
        EventKind::ToolCompleted {
            name: "shell".into(),
            call_id: a,
            status: ToolStatus::Success,
            content_head: "ok".into(),
        },
    );
    assert_eq!(
        recorder.snapshot().in_flight_tools,
        vec!["shell".to_owned()],
        "completion correlates by call id, not name"
    );
    recorder.emit(
        task,
        EventKind::ToolCompleted {
            name: "shell".into(),
            call_id: b,
            status: ToolStatus::Success,
            content_head: "ok".into(),
        },
    );
    assert_eq!(recorder.snapshot().in_flight_tools, Vec::<String>::new());
}

#[test]
fn status_snapshot_survives_a_json_round_trip() {
    // IPC-ready: a GUI may shuttle snapshots across a process
    // boundary; the shape must round-trip like the events it
    // derives from.
    let snapshot = StatusSnapshot {
        task_id: Some(TaskId::new()),
        session_id: Some(SessionId::new()),
        turn: Some(3),
        in_flight_tools: vec!["edit_file".to_owned()],
        attempt: Some(1),
        phase: TaskPhase::Running,
    };
    let value = serde_json::to_value(&snapshot).expect("serializes");
    assert_eq!(value["phase"], "running");
    assert_eq!(value["in_flight_tools"][0], "edit_file");
    let round: StatusSnapshot = serde_json::from_value(value).expect("round-trips");
    assert_eq!(round, snapshot);
}

/// Schema v9: `TodoUpdated` serializes with the `snake_case` tag and
/// the complete item list, so an NDJSON consumer can reconstruct the
/// session's todo state without kernel types beyond serde.
#[test]
fn todo_updated_serializes_the_full_list() {
    let envelope = EventEnvelope {
        schema_version: EVENT_SCHEMA_VERSION,
        timestamp_millis: None,
        sequence: 7,
        task_id: TaskId::new(),
        kind: EventKind::TodoUpdated {
            revision: 3,
            items: vec![TodoItem {
                id: "1".to_owned(),
                text: "write the test".to_owned(),
                status: crate::session::TodoStatus::Done,
            }],
        },
    };
    let value = serde_json::to_value(&envelope).expect("serializes");
    assert_eq!(value["kind"]["type"], "todo_updated");
    assert_eq!(value["kind"]["revision"], 3);
    assert_eq!(value["kind"]["items"][0]["id"], "1");
    assert_eq!(value["kind"]["items"][0]["status"], "done");
    let round: EventEnvelope = serde_json::from_value(value).expect("round-trips");
    assert_eq!(round, envelope);
}

/// Roadmap item 8 phase B, replay: folding a task's `TodoUpdated`
/// Schema v12: `TurnBudgetExtended` serializes with the `snake_case`
/// tag and a `snake_case` reason, so an NDJSON consumer can count
/// extensions and their evidence without kernel types.
#[test]
fn turn_budget_extended_serializes_snake_case() {
    let envelope = EventEnvelope {
        schema_version: EVENT_SCHEMA_VERSION,
        timestamp_millis: None,
        sequence: 3,
        task_id: TaskId::new(),
        kind: EventKind::TurnBudgetExtended {
            turn: 4,
            reason: TurnBudgetReason::UnverifiedWork,
            new_limit: 12,
        },
    };
    let value = serde_json::to_value(&envelope).expect("json");
    assert_eq!(value["kind"]["type"], "turn_budget_extended");
    assert_eq!(value["kind"]["turn"], 4);
    assert_eq!(value["kind"]["reason"], "unverified_work");
    assert_eq!(value["kind"]["new_limit"], 12);
}

/// events in sequence order reconstructs the list — and each event
/// corresponds to exactly one tail re-append, so a replayer renders
/// the same byte-stable blocks the transcript carries.
#[test]
fn todo_updated_events_replay_into_the_list_and_its_blocks() {
    let mut live = crate::session::TodoList::default();
    let mut emitted: Vec<EventKind> = Vec::new();

    // The loop's emission shape: after each mutating batch, the
    // event carries the whole list.
    live.add("write the test");
    live.add("fix the bug");
    emitted.push(EventKind::TodoUpdated {
        revision: live.revision,
        items: live.items.clone(),
    });
    live.mark("1", crate::session::TodoStatus::Done);
    emitted.push(EventKind::TodoUpdated {
        revision: live.revision,
        items: live.items.clone(),
    });

    // Replay: the last event's payload is the current list.
    let EventKind::TodoUpdated { revision: _, items } = emitted.last().unwrap() else {
        panic!("expected a TodoUpdated event");
    };
    let replayed = crate::session::TodoList {
        items: items.clone(),
        ..Default::default()
    };
    assert_eq!(replayed.items, live.items);
    assert_eq!(
        replayed.render(),
        "## todo (kernel)\n- [x] 1: write the test\n- [ ] 2: fix the bug",
        "replay renders the same byte-stable block the loop appended"
    );
    assert_eq!(
        emitted.len(),
        2,
        "one event per change, none per quiet turn"
    );
}

#[test]
fn text_head_passes_short_text_through_verbatim() {
    assert_eq!(text_head("hello", 2_000), "hello");
    assert_eq!(text_head("", 1_000), "");
}

#[test]
fn text_head_truncates_on_a_char_boundary_with_a_marker() {
    let long = "ab".repeat(3_000);
    let head = text_head(&long, 1_000);
    assert!(head.len() <= 1_000 + "[truncated]".len());
    assert!(head.ends_with("[truncated]"));
    assert!(head.starts_with("abab"));
}

#[test]
fn text_head_never_splits_a_multibyte_char() {
    // Each 'あ' is three UTF-8 bytes; a byte budget that lands
    // mid-character must back off to the boundary.
    let wide = "あ".repeat(600);
    let head = text_head(&wide, 1_000);
    assert!(head.ends_with("[truncated]"));
    let kept = head.trim_end_matches("[truncated]");
    assert_eq!(kept.len() % 3, 0, "no partial character survives");
    assert!(std::str::from_utf8(kept.as_bytes()).is_ok());
}

#[test]
fn enriched_tool_events_and_loop_nudge_round_trip() {
    // Schema 11: tool events carry argument heads and result heads,
    // and loop-owned nudges serialize — the eval forensics the old
    // shape was blind to (2026-10-23 misdiagnosis).
    let call = ToolCallId::try_from("call_1".to_owned()).unwrap();
    let started = EventKind::ToolStarted {
        name: "run_command".into(),
        call_id: call.clone(),
        arguments: "{\"command\":[\"pytest\",\"-q\"]}".into(),
    };
    let completed = EventKind::ToolCompleted {
        name: "run_command".into(),
        call_id: call,
        status: ToolStatus::Success,
        content_head: "$ pytest -q\nexit status: 0".into(),
    };
    let nudge = EventKind::LoopNudge {
        text: "Hold on — these files changed".into(),
    };
    for kind in [started, completed, nudge] {
        let json = serde_json::to_string(&kind).unwrap();
        let back: EventKind = serde_json::from_str(&json).unwrap();
        let round = serde_json::to_string(&back).unwrap();
        assert_eq!(json, round, "event {json} must round-trip");
    }
    let nudge_json = serde_json::to_string(&EventKind::LoopNudge { text: "x".into() }).unwrap();
    assert!(
        nudge_json.contains("\"loop_nudge\""),
        "kind tag is snake_case: {nudge_json}"
    );
}
