use super::*;
use crate::context::ContextCompiler;
use crate::context::messages_serialized;
use crate::context::pruning::projection::handoff_session;

/// The handoff view leads with the opening brief, carries the
/// handoff as a user item, then keeps the tail from `upto_item` on.
#[test]
fn handoff_session_is_brief_then_handoff_then_tail() {
    let items = vec![
        Item::User {
            text: "brief".to_owned(),
        },
        assistant_text("middle one"),
        assistant_text("middle two"),
        assistant_text("the tail"),
    ];
    let view = handoff_session(&session_with(items), "HANDOFF", 2);
    assert_eq!(view.transcript.len(), 4);
    assert!(matches!(&view.transcript[0], Item::User { text } if text == "brief"));
    assert!(matches!(&view.transcript[1], Item::User { text } if text == "HANDOFF"));
    assert!(matches!(&view.transcript[2], Item::Assistant(a) if a.content == "middle two"));
    assert!(matches!(&view.transcript[3], Item::Assistant(a) if a.content == "the tail"));
}

/// A transcript that does not open on a user item still gets a valid
/// view (handoff alone leads), and `upto_item` clamps to the end.
#[test]
fn handoff_session_handles_odd_openings_and_clamps() {
    let items = vec![
        assistant_text("first"),
        assistant_text("second"),
        Item::User {
            text: "brief".to_owned(),
        },
    ];
    let view = handoff_session(&session_with(items), "HANDOFF", 99);
    assert_eq!(view.transcript.len(), 1);
    assert!(matches!(&view.transcript[0], Item::User { text } if text == "HANDOFF"));
}

/// An LLM handoff checkpoint projects the view (brief + handoff +
/// tail), applies only call-id-keyed elisions — ordinal-keyed ones
/// would misfire on the renumbered view — and stays byte-stable.
#[test]
fn handoff_checkpoint_projects_view_and_skips_ordinal_elisions() {
    let items = vec![
        Item::User {
            text: "the brief".to_owned(),
        },
        assistant_text(&big("forgotten prose", 2)),
        assistant_calls("reading", vec![read_call("c1", "src/a.rs")]),
        result("c1", &big("gone", 2)),
        assistant_calls("tail read", vec![read_call("c2", "src/tail.rs")]),
        result("c2", &big("tail content", 1)),
        assistant_text("done"),
    ];
    let session = session_with(items);
    let checkpoint = PruneCheckpoint {
        upto_item: 4,
        stubs: Vec::new(),
        elided_args: vec![ArgElision {
            call_id: "c2".to_owned(),
            arguments: ToolArguments::try_from(json!({
                "path": "src/tail.rs",
                "content": "[pruned 1229 chars]"
            }))
            .expect("args"),
        }],
        // Original ordinal 1 is the handoff item's ordinal in the
        // view: applying ordinal elisions under handoff would eat
        // the handoff itself.
        elided_users: vec![1],
        elided_reasoning: Vec::new(),
        elided_assistant_text: Vec::new(),
        handoff: Some("HANDOFF SUMMARY".to_owned()),
    };
    let config = PruneConfig::default();
    let compiled = compiler(config)
        .compile(input(&session, Some(&checkpoint)))
        .expect("compiles");
    let projected = messages_serialized(&compiled.request.messages);
    assert!(projected.contains("the brief"), "brief leads the view");
    assert!(projected.contains("HANDOFF SUMMARY"), "handoff survives");
    assert!(
        projected.contains("tail contentxxxx"),
        "tail from upto_item on is kept"
    );
    assert!(
        !projected.contains("forgotten prosex"),
        "pre-handoff history is summarized away"
    );
    assert!(
        projected.contains("[pruned 1229 chars]"),
        "call-id-keyed arg elision applied"
    );
    // Byte-stable: the same checkpoint compiles to the same bytes.
    let again = compiler(config)
        .compile(input(&session, Some(&checkpoint)))
        .expect("compiles");
    assert_eq!(projected, messages_serialized(&again.request.messages));
    // The checkpoint is preserved (handoff and frozen elisions
    // alike), never churned by a re-decide over the view.
    assert_eq!(again.prune_checkpoint.as_ref(), Some(&checkpoint));
    assert_eq!(compiled.prune_checkpoint.as_ref(), Some(&checkpoint));
}

/// Checkpoints written before item 12 (no `handoff`, no
/// `elided_assistant_text`) replay unchanged via serde defaults.
#[test]
fn old_checkpoint_json_defaults_handoff_none() {
    let legacy = serde_json::json!({
        "upto_item": 3,
        "stubs": [],
        "elided_args": [],
        "elided_users": [],
        "elided_reasoning": [],
    });
    let checkpoint: PruneCheckpoint =
        serde_json::from_value(legacy).expect("legacy checkpoint replays");
    assert_eq!(checkpoint.handoff, None);
    assert!(checkpoint.elided_assistant_text.is_empty());
}
