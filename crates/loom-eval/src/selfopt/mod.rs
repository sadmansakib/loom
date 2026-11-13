//! The offline self-optimization outer loop (roadmap item 20.3).
//!
//! Reads the artifacts a `swebench-run` sweep already produced — the
//! sweep report plus each run's NDJSON event stream — and treats the
//! harness itself as the search space: every `LOOM_*` override seam
//! from item 20.2 (`loom_core::engine::BUDGET_ENV_VARS` /
//! [`PRUNE_ENV_VARS`]) is a knob, and a *proposal* is an env overlay
//! the in-container engine consumes with zero script or binary change.
//!
//! The loop is the contrastive shape Harness² (arXiv 2610.14224)
//! describes — *propose, probe, compose* — restricted to what an
//! offline harness can measure honestly:
//!
//! 1. **Propose** — a deterministic rule proposer walks every knob
//!    (halve/double, clamped), and optionally a model proposer reads
//!    the baseline traces plus retained lessons and returns
//!    hypothesis-driven overlays, each tagged `invariant` (a
//!    context-management change that should hold across domains) or
//!    `domain` (task-class-specific), mirroring Harness²'s two
//!    candidate classes.
//! 2. **Probe** — re-run the *same* instances with one overlay per
//!    probe (`--jobs` containers at a time), the same model and image
//!    the baseline used. Paired by instance, so the signal is the
//!    per-instance delta, never a cross-run average.
//! 3. **Compose** — adopt a proposal only on evidence (median input
//!    tokens down ≥ [`ADOPT_MIN_TOKEN_IMPROVEMENT_PERCENT`], events
//!    not regressed beyond [`EVENTS_REGRESSION_MAX_PERCENT`], no new
//!    failures), then merge the adopted overlays — Harness²'s
//!    selective composition — and re-probe the merged overlay before
//!    it wins anything.
//!
//! GEPA (arXiv 2507.19457) supplies the lesson discipline: the model
//! proposer reads *full traces* (here: the per-run usage and failure
//! reductions, not raw transcripts), and a lesson enters the ledger
//! only when a probe contrast supports it — never on vibes.
//!
//! Everything here is offline-testable: the docker probes live in the
//! `selfopt` binary; this module owns the pure parts (knob table,
//! validation, proposals, paired comparison, composition, ledger).
//!
//! The replay pre-probe ([`pre_probe_proposal`]) sits between propose
//! and probe: a projection-affecting overlay is replayed offline
//! through `loom_core::engine::replay` against locally recorded trace
//! directories, and a proposal whose projected median input-token
//! improvement is below the adoption gate is rejected before any
//! docker probes are spent. Same gate constants as the real probe —
//! the pre-probe only ever tightens, never loosens.

// Harness internals: panicking on a malformed fixture is the intended
// behavior in tests.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(test)]
use crate::swebench::RunUsage;

mod baseline;
mod compare;
mod compose;
mod knobs;
mod ledger;
mod preprobe;
mod proposals;
mod proposer;

pub use baseline::{Baseline, RunStat, load_baseline};
pub use compare::{
    ADOPT_MIN_TOKEN_IMPROVEMENT_PERCENT, Comparison, Decision, EVENTS_REGRESSION_MAX_PERCENT,
    InstanceDelta, InstanceStat, compare, decide, median, order_instances, pareto_frontier,
    per_instance,
};
pub use compose::merge_overlays;
pub use knobs::{KNOBS, Knob, Overlay, knob_by_env, overlay_label, validate_overlay};
pub use ledger::{LedgerEntry, Lesson, append_ledger, load_ledger};
pub use preprobe::{PreProbeOutcome, pre_probe_proposal};
pub use proposals::{Proposal, ProposalClass, rule_proposals};
pub use proposer::{PROPOSER_SYSTEM_PROMPT, build_proposal_prompt, parse_proposal_response};
// ------------------------------------------------------------ tests ---

#[cfg(test)]
mod tests {
    use super::*;

    fn overlay(pairs: &[(&str, &str)]) -> Overlay {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn knob_table_covers_every_engine_env_seam() {
        // A seam the engine reads but the table lacks is invisible to
        // the optimizer; the inverse is inert. Both are drift.
        let mut expected: Vec<&str> = loom_core::engine::BUDGET_ENV_VARS.to_vec();
        expected.extend(loom_core::engine::PRUNE_ENV_VARS);
        expected.extend(loom_core::engine::TURN_BUDGET_ENV_VARS);
        let have: Vec<&str> = KNOBS.iter().map(|knob| knob.env).collect();
        assert_eq!(have.len(), expected.len());
        for env in expected {
            assert!(have.contains(&env), "missing knob: {env}");
        }
    }

    #[test]
    fn validate_rejects_unknown_out_of_range_and_non_integer() {
        assert!(validate_overlay(&overlay(&[("LOOM_PRUNE_TRIGGER_PERCENT", "70")])).is_ok());
        assert!(validate_overlay(&overlay(&[("LOOM_NOPE", "1")])).is_err());
        assert!(validate_overlay(&overlay(&[("LOOM_PRUNE_TRIGGER_PERCENT", "10")])).is_err());
        assert!(validate_overlay(&overlay(&[("LOOM_PRUNE_TRIGGER_PERCENT", "hot")])).is_err());
    }

    #[test]
    fn labels_are_deterministic_and_sorted() {
        let one = overlay(&[("LOOM_PRUNE_TRIGGER_PERCENT", "70")]);
        assert_eq!(overlay_label(&one), "prune_trigger_percent-70");
        let two = overlay(&[
            ("LOOM_READ_HEAD_BYTES", "32768"),
            ("LOOM_PRUNE_TRIGGER_PERCENT", "70"),
        ]);
        assert_eq!(
            overlay_label(&two),
            "prune_trigger_percent-70+read_head_bytes-32768"
        );
        assert_eq!(overlay_label(&Overlay::new()), "baseline");
    }

    #[test]
    fn rule_proposals_halve_and_double_within_bounds() {
        let proposals = rule_proposals(&Overlay::new(), 100);
        assert!(proposals.len() >= KNOBS.len(), "at least one per knob");
        assert!(proposals.len() <= KNOBS.len() * 2);
        for proposal in &proposals {
            assert_eq!(proposal.origin, "rule");
            assert_eq!(proposal.class, ProposalClass::Invariant);
            assert!(validate_overlay(&proposal.overlay).is_ok());
        }
        // Halving the trigger percent must appear (clamped up to the
        // knob floor of 50); doubling clamps to 95.
        let trigger = proposals
            .iter()
            .find(|p| p.overlay.contains_key("LOOM_PRUNE_TRIGGER_PERCENT"))
            .expect("trigger proposal");
        let value: u64 = trigger
            .overlay
            .get("LOOM_PRUNE_TRIGGER_PERCENT")
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            value == 50 || value == 95,
            "halve (50, floor-clamped) or clamped double (95), got {value}"
        );
        // The max cap is honored.
        assert!(rule_proposals(&Overlay::new(), 3).len() <= 3);
    }

    #[test]
    fn rule_proposals_respect_an_existing_baseline() {
        let base = overlay(&[("LOOM_PRUNE_TRIGGER_PERCENT", "95")]);
        let proposals = rule_proposals(&base, 100);
        // Doubling 95 clamps back to 95 == current: skipped; only the
        // halve proposal CHANGES that knob (every other proposal
        // merely rides the baseline value along).
        let triggers: Vec<&Proposal> = proposals
            .iter()
            .filter(|p| {
                p.overlay
                    .get("LOOM_PRUNE_TRIGGER_PERCENT")
                    .is_some_and(|value| value != "95")
            })
            .collect();
        assert_eq!(triggers.len(), 1);
        // Baseline keys ride along on every proposal except the one
        // that deliberately moves that knob.
        for proposal in &proposals {
            let value = proposal
                .overlay
                .get("LOOM_PRUNE_TRIGGER_PERCENT")
                .map(String::as_str);
            assert!(
                value == Some("95") || std::ptr::eq(proposal, triggers[0]),
                "baseline rides along, got {value:?}"
            );
        }
    }

    #[test]
    #[allow(clippy::float_cmp)] // exact medians of exact inputs
    fn median_handles_odd_even_and_empty() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), 2.5);
        assert_eq!(median(&[]), 0.0);
    }

    fn run_stat(instance: &str, run: u64, input: u64, events: u64, failed: bool) -> RunStat {
        RunStat {
            instance: instance.to_owned(),
            run,
            events,
            turns: events / 3,
            usage: RunUsage {
                input_tokens: input,
                cached_input_tokens: None,
                output_tokens: input / 4,
                model_requests: 1,
            },
            task_failed: failed.then(|| "boom".to_owned()),
        }
    }

    #[test]
    #[allow(clippy::float_cmp)] // exact medians of exact inputs
    fn per_instance_takes_medians_over_runs() {
        let runs = vec![
            run_stat("a", 1, 100, 30, false),
            run_stat("a", 2, 200, 30, false),
            run_stat("a", 3, 10_000, 30, true), // outlier run, median unaffected
            run_stat("b", 1, 50, 20, false),
        ];
        let stats = per_instance(&runs);
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].instance, "a");
        assert_eq!(stats[0].median_input_tokens, 200.0);
        assert_eq!(stats[0].failed_runs, 1);
        assert_eq!(stats[1].median_input_tokens, 50.0);
    }

    #[test]
    fn compare_pairs_by_instance_and_decides_on_the_gate() {
        let baseline = vec![
            run_stat("a", 1, 1000, 30, false),
            run_stat("b", 1, 2000, 40, false),
        ];
        // Probe: 20% fewer tokens everywhere, same events.
        let probe = vec![
            run_stat("a", 1, 800, 30, false),
            run_stat("b", 1, 1600, 40, false),
        ];
        let comparison = compare(&baseline, &probe);
        assert!((comparison.median_input_improvement_pct - 20.0).abs() < 1e-9);
        assert_eq!(comparison.baseline_failures, 0);
        let decision = decide("p", &comparison);
        assert!(decision.adopted, "{}", decision.reason);

        // Token win but events regress beyond the gate: rejected, but
        // promoted (input and failure gates passed).
        let flailing = vec![
            run_stat("a", 1, 500, 60, false),
            run_stat("b", 1, 1000, 80, false),
        ];
        let comparison = compare(&baseline, &flailing);
        let decision = decide("p", &comparison);
        assert!(!decision.adopted);
        assert!(decision.promoted, "{}", decision.reason);

        // New failures: rejected regardless of tokens, not promoted.
        let dying = vec![
            run_stat("a", 1, 100, 10, true),
            run_stat("b", 1, 100, 10, true),
        ];
        let comparison = compare(&baseline, &dying);
        let decision = decide("p", &comparison);
        assert!(!decision.adopted);
        assert!(!decision.promoted);
    }

    #[test]
    fn pareto_frontier_drops_dominated_overlays() {
        let decision = |label: &str, input: f64, events: f64, failures: usize| Decision {
            label: label.to_owned(),
            adopted: true,
            input_improvement_pct: input,
            events_improvement_pct: events,
            probe_failures: failures,
            ..Decision::default()
        };
        // b beats a on everything; c trades failures for events.
        let decisions = vec![
            decision("a", 10.0, 5.0, 2),
            decision("b", 20.0, 6.0, 1),
            decision("c", 12.0, 0.0, 0),
            Decision {
                label: "rejected".to_owned(),
                adopted: false,
                input_improvement_pct: 99.0,
                ..Decision::default()
            },
        ];
        let frontier = pareto_frontier(&decisions);
        assert_eq!(frontier, vec!["b".to_owned(), "c".to_owned()]);
    }

    #[test]
    fn order_instances_puts_failures_first_then_cost() {
        let runs = vec![
            run_stat("cheap", 1, 100, 10, false),
            run_stat("hard", 1, 500, 30, true),
            run_stat("mid", 1, 300, 20, false),
            run_stat("mid", 2, 100, 20, false), // runs aggregate
        ];
        assert_eq!(
            order_instances(&runs),
            vec!["hard".to_owned(), "mid".to_owned(), "cheap".to_owned()]
        );
    }

    #[test]
    fn merge_overlays_conflicts_are_named() {
        let a = overlay(&[("LOOM_PRUNE_TRIGGER_PERCENT", "70")]);
        let b = overlay(&[("LOOM_READ_HEAD_BYTES", "32768")]);
        let merged = merge_overlays(&[&a, &b]).expect("disjoint merges");
        assert_eq!(merged.len(), 2);
        let c = overlay(&[("LOOM_PRUNE_TRIGGER_PERCENT", "90")]);
        let error = merge_overlays(&[&a, &c]).expect_err("conflict");
        assert!(error.contains("LOOM_PRUNE_TRIGGER_PERCENT"));
    }

    #[test]
    fn parse_tolerates_fences_and_drops_invalid() {
        let raw = "```json\n[\n{\"overlay\":{\"LOOM_PRUNE_TRIGGER_PERCENT\":\"70\"},\"class\":\"domain\",\"hypothesis\":\"h\",\"rationale\":\"r\"},\n{\"overlay\":{\"LOOM_NOPE\":\"1\"},\"class\":\"domain\",\"hypothesis\":\"h\",\"rationale\":\"r\"}]\n```";
        let (proposals, dropped) = parse_proposal_response(raw, 10);
        assert_eq!(proposals.len(), 1);
        assert_eq!(dropped, 1);
        assert_eq!(proposals[0].class, ProposalClass::Domain);
        assert_eq!(proposals[0].origin, "model");
        assert_eq!(proposals[0].label, "prune_trigger_percent-70");
    }

    #[test]
    fn parse_survives_prose_around_the_array() {
        let raw = "Here you go:\n[{\"overlay\":{\"LOOM_PRUNE_TAIL_TOKENS\":\"8000\"}}]\nGood luck!";
        let (proposals, _) = parse_proposal_response(raw, 10);
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0].hypothesis, "unstated");
    }

    #[test]
    fn ledger_round_trips_and_missing_is_empty() {
        let dir = std::env::temp_dir().join(format!("loom-selfopt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ledger.json");
        assert!(load_ledger(&path).unwrap().is_empty());
        let entry = LedgerEntry {
            timestamp: "2026-10-25T00:00:00".to_owned(),
            sweep: "sweep.json".to_owned(),
            provider: "deepseek".to_owned(),
            model: "deepseek-flash".to_owned(),
            base_overlay: Overlay::new(),
            proposals: Vec::new(),
            decisions: vec![Decision {
                label: "p".to_owned(),
                adopted: true,
                reason: "input +6.0%".to_owned(),
                ..Decision::default()
            }],
            adopted: vec!["p".to_owned()],
            promoted: vec!["q".to_owned()],
            merged: overlay(&[("LOOM_PRUNE_TRIGGER_PERCENT", "70")]),
            lessons: vec![Lesson {
                statement: "prune earlier on test-loop-heavy tasks".to_owned(),
                evidence: "p: input +6.0%".to_owned(),
            }],
        };
        append_ledger(&path, &entry).unwrap();
        let loaded = load_ledger(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].lessons[0].statement, entry.lessons[0].statement);
        assert_eq!(loaded[0].merged, entry.merged);
        // Old ledgers without `promoted` load as empty.
        let legacy = serde_json::json!([{
            "timestamp": "t", "sweep": "s", "provider": "p", "model": "m",
            "base_overlay": {}, "proposals": [], "decisions": [],
            "adopted": [], "merged": {}, "lessons": []
        }]);
        std::fs::write(dir.join("legacy.json"), legacy.to_string()).unwrap();
        assert!(
            load_ledger(&dir.join("legacy.json")).unwrap()[0]
                .promoted
                .is_empty()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prompt_carries_knobs_stats_and_lessons() {
        let baseline = Baseline {
            provider: "deepseek".to_owned(),
            model: "deepseek-flash".to_owned(),
            base_url: None,
            arm: "full".to_owned(),
            runs: Vec::new(),
        };
        let stats = vec![InstanceStat {
            instance: "org/one-1".to_owned(),
            median_input_tokens: 12_345.0,
            median_events: 40.0,
            median_turns: 12.0,
            failed_runs: 0,
            runs: 5,
        }];
        let lessons = vec![Lesson {
            statement: "s".to_owned(),
            evidence: "e".to_owned(),
        }];
        let prompt = build_proposal_prompt(&baseline, &stats, &lessons);
        assert!(prompt.contains("LOOM_PRUNE_TRIGGER_PERCENT"));
        assert!(prompt.contains("org/one-1"));
        assert!(prompt.contains("12"));
        assert!(prompt.contains("\"statement\": \"s\""));
    }

    #[test]
    fn baseline_load_reads_report_rows_and_run_json_turns() {
        let dir = std::env::temp_dir().join(format!("loom-selfopt-base-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let instances = dir.join("instances/org-one-1/run-1");
        std::fs::create_dir_all(&instances).unwrap();
        // turns come from run.json (2 turn_started lines).
        std::fs::write(
            instances.join("run.json"),
            "{\"kind\":{\"type\":\"turn_started\"}}\n{\"kind\":{\"type\":\"turn_started\"}}\n",
        )
        .unwrap();
        let report = serde_json::json!({
            "provider": "deepseek",
            "model": "deepseek-flash",
            "base_url": null,
            "arm": "full",
            "instances": [{
                "instance_id": "org/one-1",
                "runs": [{
                    "run": 1,
                    "events": 7,
                    "task_failed": null,
                    "usage": {"input_tokens": 1000, "output_tokens": 200,
                              "model_requests": 4}
                }]
            }]
        });
        let sweep_path = dir.join("swebench-sweep-test.json");
        std::fs::write(&sweep_path, serde_json::to_string(&report).unwrap()).unwrap();
        let baseline = load_baseline(&sweep_path).unwrap();
        assert_eq!(baseline.provider, "deepseek");
        assert_eq!(baseline.runs.len(), 1);
        assert_eq!(baseline.runs[0].turns, 2);
        assert_eq!(baseline.runs[0].events, 7);
        assert_eq!(baseline.runs[0].usage.input_tokens, 1000);
        std::fs::remove_dir_all(&dir).ok();
    }

    // ------------------------------------------------- pre-probe ---

    /// Writes a minimal `loom run --trace`-shaped directory: a user
    /// task, `reads` rounds of a `read_file` call + result of roughly
    /// `read_bytes` bytes each, and a final end turn. No summary, no
    /// events — the pre-probe needs neither.
    fn write_replay_trace(
        dir: &std::path::Path,
        reads: usize,
        read_bytes: usize,
    ) -> std::path::PathBuf {
        use loom_core::kernel::id::{SessionId, TaskId};
        use loom_core::kernel::model::StopReason;
        use loom_core::kernel::session::TaskRecord;
        use loom_core::kernel::transcript::{
            AssistantMessage, Item, ToolArguments, ToolCall, ToolCallId, ToolResult, ToolStatus,
        };

        let mut items = vec![Item::User {
            text: "read big.txt over and over".to_owned(),
        }];
        let body = "lorem ipsum dolor sit amet\n".repeat(read_bytes / 27);
        for index in 0..reads {
            let call = ToolCall {
                id: ToolCallId::try_from(format!("c{index}")).unwrap(),
                name: "read_file".to_owned(),
                arguments: ToolArguments::try_from(serde_json::json!({
                    "path": "big.txt"
                }))
                .unwrap(),
            };
            items.push(Item::Assistant(AssistantMessage {
                content: String::new(),
                reasoning: None,
                tool_calls: vec![call.clone()],
                stop_reason: StopReason::ToolCalls,
            }));
            items.push(Item::ToolResult(ToolResult::new(
                call.id.clone(),
                ToolStatus::Success,
                body.clone(),
                false,
            )));
        }
        items.push(Item::Assistant(AssistantMessage {
            content: "done".to_owned(),
            reasoning: None,
            tool_calls: Vec::new(),
            stop_reason: StopReason::EndTurn,
        }));
        // `turns` must be non-zero or the replay treats the task as a
        // fast-path solve and compiles nothing.
        let mut task = TaskRecord::new(TaskId::new(), 0);
        task.turns = u32::try_from(reads + 1).unwrap_or(u32::MAX);
        let manifest = serde_json::json!({
            "harness": {"provider": "deepseek", "model": "m"},
            "task_id": task.id.to_string(),
        });
        let transcript = serde_json::json!({
            "session_id": SessionId::new().to_string(),
            "items": items
                .iter()
                .enumerate()
                .map(|(index, item)| serde_json::json!({"index": index, "item": item}))
                .collect::<Vec<_>>(),
            "tasks": [task],
        });
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("manifest.json"), manifest.to_string()).unwrap();
        std::fs::write(dir.join("transcript.json"), transcript.to_string()).unwrap();
        dir.to_owned()
    }

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("loom-preprobe-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn pre_probe_keeps_budget_only_overlays_without_pricing() {
        let overlay = overlay(&[("LOOM_READ_HEAD_BYTES", "8192")]);
        let outcome = pre_probe_proposal(&overlay, &[], 16_384, 1_024);
        assert!(outcome.kept, "{}", outcome.note);
        assert!(outcome.note.contains("cannot price"));
        assert_eq!(outcome.projected_improvement_pct, None);
    }

    #[test]
    fn pre_probe_rejects_overlays_the_projection_cannot_pay_for() {
        let root = temp_root("reject");
        let trace = write_replay_trace(&root.join("t1"), 2, 2_000);
        // A floor so high the trigger is unreachable: the overlay arm
        // compiles exactly like the baseline, so the projected
        // improvement is zero and the gate rejects it before docker.
        let floor = overlay(&[("LOOM_PRUNE_FLOOR_TOKENS", "256000")]);
        let outcome = pre_probe_proposal(&floor, std::slice::from_ref(&trace), 16_384, 1_024);
        assert!(!outcome.kept, "{}", outcome.note);
        assert_eq!(outcome.projected_improvement_pct, Some(0.0));
        assert!(outcome.note.contains("rejected before docker"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn pre_probe_keeps_an_overlay_that_stubs_more() {
        let root = temp_root("keep");
        // Nine reads of ~9KB under a 32K window: the baseline (provider
        // defaults, 80% trigger) never approaches its trigger, while the
        // aggressive overlay triggers at 30% and — as reads age out of
        // the protected tail turns — freezes and stubs them. Enough
        // turns run AFTER the first freeze that the median (the gate
        // statistic) shifts, not just the totals.
        let trace = write_replay_trace(&root.join("t1"), 9, 9_000);
        let aggressive = overlay(&[
            ("LOOM_PRUNE_TRIGGER_PERCENT", "30"),
            ("LOOM_PRUNE_FLOOR_TOKENS", "100"),
            ("LOOM_PRUNE_MIN_RECLAIM_TOKENS", "100"),
            ("LOOM_PRUNE_TAIL_TOKENS", "200"),
            ("LOOM_AGE_MASK_TURNS", "1"),
        ]);
        let outcome = pre_probe_proposal(&aggressive, std::slice::from_ref(&trace), 32_768, 512);
        assert!(outcome.kept, "{}", outcome.note);
        assert!(
            outcome.projected_improvement_pct.unwrap_or(0.0) >= ADOPT_MIN_TOKEN_IMPROVEMENT_PERCENT,
            "{}",
            outcome.note
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn pre_probe_keeps_everything_without_usable_traces() {
        let root = temp_root("unusable");
        let not_a_trace = root.join("empty");
        std::fs::create_dir_all(&not_a_trace).unwrap();
        let floor = overlay(&[("LOOM_PRUNE_FLOOR_TOKENS", "256000")]);
        let outcome = pre_probe_proposal(&floor, std::slice::from_ref(&not_a_trace), 16_384, 1_024);
        assert!(outcome.kept, "{}", outcome.note);
        assert!(outcome.note.contains("without pre-probe evidence"));
        std::fs::remove_dir_all(&root).ok();
    }
}
