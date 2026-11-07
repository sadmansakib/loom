//! Routing slice zero (roadmap item 9): task-start model selection.
//!
//! The gateway catalog offers price tiers on one provider — a cheap
//! model, the run's default, and a strong one. This module is the
//! deterministic half of routing: a coarse classifier over the task
//! text picks a tier, and tier → model is operator configuration
//! ([`RoutingConfig`]). The funded-model classifier (the
//! "cheap model picks the tier" design in the roadmap) replaces
//! [`classify`] later without touching any other seam.
//!
//! Honesty about what the classifier is: a prior, not intelligence.
//! Three properties it must keep —
//!
//! * **Deterministic**: same text, same tier, always. Sweeps and
//!   replays must be reproducible from the transcript alone.
//! * **Cost-biased**: the default tier is Medium (the run's own
//!   model). Only an unambiguous read-only shape routes down
//!   ([`Tier::Easy`]), and routing up ([`Tier::Hard`]) is a separate
//!   opt-in model, so a configured `easy` alone can only reduce
//!   spend, never redirect a run to the expensive tier.
//! * **No false confidence**: ties and middling scores stay Medium.
//!   A wrong Easy answer is recoverable by asking again; a wrong
//!   Hard assignment is silent money.

use serde::{Deserialize, Serialize};

/// A price/capability tier a task can be routed to. `Medium` is the
/// engine's configured model — the tier every task gets unless
/// routing is both configured and confident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// The cheap model, for unambiguous read-only questions.
    Easy,
    /// The run's own model. The default and the safe middle.
    Medium,
    /// The strong model, opt-in only, for long build-shaped work.
    Hard,
}

/// Tier → model wiring. The provider stays whatever the run opened
/// with; only the model id changes, so credentials, headers, and the
/// gateway identity are inherited from the run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingConfig {
    /// Model id for [`Tier::Easy`]; `None` never routes down.
    pub easy: Option<String>,
    /// Model id for [`Tier::Hard`]; `None` never routes up.
    pub hard: Option<String>,
}

impl RoutingConfig {
    /// Whether any tier is wired.
    pub fn active(&self) -> bool {
        self.easy.is_some() || self.hard.is_some()
    }

    /// The model a tier maps to. Medium always maps to the run's own
    /// model, so the config stores nothing for it.
    pub fn model_for(&self, tier: Tier) -> Option<&str> {
        match tier {
            Tier::Easy => self.easy.as_deref(),
            Tier::Medium => None,
            Tier::Hard => self.hard.as_deref(),
        }
    }
}

impl Tier {
    /// Whether this tier is the run's own model.
    pub fn is_medium(self) -> bool {
        self == Tier::Medium
    }

    /// The tier name as written in notices and traces.
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Easy => "easy",
            Tier::Medium => "medium",
            Tier::Hard => "hard",
        }
    }
}

/// Words that signal build-shaped (not read-shaped) work.
const HARD_MARKERS: [&str; 10] = [
    "refactor",
    "migrate",
    "rewrite",
    "redesign",
    "architect",
    "implement",
    "regression",
    "failing",
    "flaky",
    "debug why",
];

/// Verbs that mean the task edits the workspace — they veto an Easy
/// routing even on short, question-shaped text.
const TOOL_VERBS: [&str; 12] = [
    "fix", "edit", "write", "create", "add", "delete", "remove", "move", "rename", "change",
    "update", "install",
];

/// Classifies a task into a tier. Pure function of the text.
pub fn classify(task: &str) -> Tier {
    let text = task.trim();
    let lower = text.to_lowercase();
    let len = text.chars().count();

    let mut score = 0i32;

    // Length prior: build-shaped asks arrive long.
    if len >= 600 {
        score += 2;
    } else if len >= 250 {
        score += 1;
    }

    // One hit is enough; the markers travel together.
    if HARD_MARKERS.iter().any(|word| lower.contains(word)) {
        score += 2;
    }

    // Structure: code fences or a multi-step list read as a build.
    if text.contains("```") {
        score += 1;
    }
    let steps = ["\n1.", "\n1)", "\n- ", "\n* "]
        .iter()
        .filter(|marker| text.contains(*marker))
        .count();
    if steps >= 2 {
        score += 1;
    }

    // Question shape pulls toward Easy.
    let question_start = [
        "what",
        "why",
        "where",
        "who",
        "when",
        "how does",
        "how do",
        "how is",
        "how are",
        "is ",
        "are ",
        "does ",
        "do ",
        "can ",
        "should ",
        "explain",
        "summarize",
        "describe",
    ]
    .iter()
    .any(|prefix| lower.starts_with(*prefix));
    if text.ends_with('?') || question_start {
        score -= 2;
    }
    // Short read-only ask with no editing verb: the easy shape.
    let has_tool_verb = TOOL_VERBS.iter().any(|verb| lower.contains(verb));
    if len <= 160 && !has_tool_verb {
        score -= 1;
    }
    // An editing verb vetoes Easy outright: "summarize X then add Y"
    // is build work regardless of how it opens, and a wrong Easy
    // routing is silent cost transferred, not saved.
    if has_tool_verb {
        score = score.max(0);
    }

    if score >= 4 {
        Tier::Hard
    } else if score <= -2 {
        Tier::Easy
    } else {
        Tier::Medium
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tier(text: &str) -> Tier {
        classify(text)
    }

    #[test]
    fn read_only_questions_route_easy() {
        assert_eq!(tier("What does this repo do?"), Tier::Easy);
        assert_eq!(
            tier("explain the retry policy in the anthropic adapter"),
            Tier::Easy
        );
        assert_eq!(tier("where is the container script assembled?"), Tier::Easy);
        assert_eq!(
            tier("does the gateway support reasoning_effort?"),
            Tier::Easy
        );
    }

    #[test]
    fn editing_verbs_veto_easy_even_when_short_and_question_shaped() {
        assert_eq!(tier("can you fix the flaky timeout test?"), Tier::Medium);
        assert_eq!(tier("add a --route-easy flag to the CLI"), Tier::Medium);
        assert_eq!(
            tier("how do I rename the account? please rename it"),
            Tier::Medium
        );
    }

    #[test]
    fn plain_work_orders_stay_medium() {
        assert_eq!(tier("Fix the failing test in the db shell"), Tier::Medium);
        assert_eq!(
            tier("Summarize the compiler pipeline and then add a cache"),
            Tier::Medium
        );
    }

    #[test]
    fn long_structured_builds_route_hard() {
        // Long + hard marker + code fence: the build shape.
        let long = format!(
            "Refactor the context compiler to support per-turn effort. {}\n```rust\nfn main() {{}}\n```\n1. keep the cache prefix stable\n2. add the field\n3. port the tests",
            "context ".repeat(40)
        );
        assert_eq!(tier(&long), Tier::Hard);
        // But only with the opt-in hard model configured does that
        // tier mean anything — `model_for` returns None otherwise.
        let config = RoutingConfig::default();
        assert_eq!(config.model_for(Tier::Hard), None);
    }

    #[test]
    fn medium_requires_nothing_and_costs_nothing() {
        let config = RoutingConfig::default();
        assert!(!config.active());
        assert_eq!(config.model_for(Tier::Medium), None);
    }

    #[test]
    fn config_wiring_maps_only_the_configured_tiers() {
        let config = RoutingConfig {
            easy: Some("cheap-model".to_owned()),
            hard: None,
        };
        assert!(config.active());
        assert_eq!(config.model_for(Tier::Easy), Some("cheap-model"));
        assert_eq!(config.model_for(Tier::Hard), None);
    }

    #[test]
    fn classification_is_deterministic() {
        let text = "Implement the routing classifier and add unit tests for it";
        assert_eq!(classify(text), classify(text));
    }
}
