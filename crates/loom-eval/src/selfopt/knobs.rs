//! The sweepable knob table and overlay validation: every pure
//! `LOOM_*` seam, its bounds, and the deterministic labels overlays
//! are probed under.

use std::collections::BTreeMap;

/// One tunable `LOOM_*` seam the overlay may set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Knob {
    /// Environment variable the in-container engine reads.
    pub env: &'static str,
    /// Short name for labels and reports (e.g. `prune_trigger_percent`).
    pub name: &'static str,
    /// The value `ToolBudgets::default` / `PruneConfig::default`
    /// produce — the harness's shipped behavior, hardcoded here
    /// because the crate-private constants in `loom-tools` are not
    /// exported (and re-deriving them would couple eval to tools).
    pub default: u64,
    /// Inclusive lower bound for proposals.
    pub floor: u64,
    /// Inclusive upper bound for proposals.
    pub ceiling: u64,
    /// Unit for report rendering (`bytes`, `tokens`, `percent`,
    /// `turns`, `lines`).
    pub unit: &'static str,
    /// One line of what the knob trades off, shown to the model
    /// proposer so hypotheses name a mechanism, not just a direction.
    pub doc: &'static str,
}

/// The sweepable knob table: every pure env seam from roadmap item
/// 20.2. Kept in lockstep with the engine's `BUDGET_ENV_VARS` and
/// `PRUNE_ENV_VARS` — a seam added there without a row here is
/// invisible to the optimizer, and a row without a seam is inert.
pub const KNOBS: &[Knob] = &[
    Knob {
        env: "LOOM_READ_HEAD_BYTES",
        name: "read_head_bytes",
        default: 56 * 1024,
        floor: 8 * 1024,
        ceiling: 256 * 1024,
        unit: "bytes",
        doc: "head of a read file shown whole; smaller hides more of large files",
    },
    Knob {
        env: "LOOM_COMMAND_HEAD_BYTES",
        name: "command_head_bytes",
        default: 16 * 1024,
        floor: 4 * 1024,
        ceiling: 64 * 1024,
        unit: "bytes",
        doc: "head of command output kept; smaller truncates noisy tools sooner",
    },
    Knob {
        env: "LOOM_COMMAND_TAIL_BYTES",
        name: "command_tail_bytes",
        default: 16 * 1024,
        floor: 4 * 1024,
        ceiling: 64 * 1024,
        unit: "bytes",
        doc: "tail of command output kept alongside the head (errors live at the end)",
    },
    Knob {
        env: "LOOM_EDIT_ECHO_MAX_BYTES",
        name: "edit_echo_max_bytes",
        default: 8 * 1024,
        floor: 1024,
        ceiling: 32 * 1024,
        unit: "bytes",
        doc: "cap on the edited-region echo; smaller echoes save context per edit",
    },
    Knob {
        env: "LOOM_EDIT_ECHO_LINES",
        name: "edit_echo_lines",
        default: 3,
        floor: 1,
        ceiling: 16,
        unit: "lines",
        doc: "context lines around an edit in the echo; more lines, more orientation",
    },
    Knob {
        env: "LOOM_SYMBOLS_OVERVIEW_MAX_BYTES",
        name: "symbols_overview_max_bytes",
        default: 8 * 1024,
        floor: 1024,
        ceiling: 32 * 1024,
        unit: "bytes",
        doc: "cap on the symbol-tree overview; larger keeps more of the outline",
    },
    Knob {
        env: "LOOM_SKILL_HEAD_BYTES",
        name: "skill_head_bytes",
        default: 16 * 1024,
        floor: 4 * 1024,
        ceiling: 64 * 1024,
        unit: "bytes",
        doc: "head of a loaded skill body; smaller may cut instructions mid-way",
    },
    Knob {
        env: "LOOM_REPLACE_SYMBOL_HEAD_BYTES",
        name: "replace_symbol_head_bytes",
        default: 400,
        floor: 100,
        ceiling: 4 * 1024,
        unit: "bytes",
        doc: "head echoed after replace_symbol; confirmation context per edit",
    },
    Knob {
        env: "LOOM_MCP_IDLE_EVICT_TURNS",
        name: "mcp_idle_evict_turns",
        default: 8,
        floor: 1,
        ceiling: 64,
        unit: "turns",
        doc: "idle turns before a loaded MCP tool demotes back to the menu",
    },
    Knob {
        env: "LOOM_PRUNE_TRIGGER_PERCENT",
        name: "prune_trigger_percent",
        default: 80,
        floor: 50,
        ceiling: 95,
        unit: "percent",
        doc: "context fill that triggers pruning; lower prunes earlier, more often",
    },
    Knob {
        env: "LOOM_PRUNE_FLOOR_TOKENS",
        name: "prune_floor_tokens",
        default: 64_000,
        floor: 16_000,
        ceiling: 256_000,
        unit: "tokens",
        doc: "memory the compiler reclaims down to when pruning fires",
    },
    Knob {
        env: "LOOM_PRUNE_MIN_RECLAIM_TOKENS",
        name: "prune_min_reclaim_tokens",
        default: 20_000,
        floor: 5_000,
        ceiling: 100_000,
        unit: "tokens",
        doc: "reclaim below this is not worth a rewrite of the window",
    },
    Knob {
        env: "LOOM_PHASE_MASK_TURNS",
        name: "phase_mask_turns",
        default: 0,
        floor: 1,
        ceiling: 16,
        unit: "turns",
        doc: "0 = off; during the writing phase, stub exploration reads older than this window",
    },
    Knob {
        env: "LOOM_PRUNE_TAIL_TOKENS",
        name: "prune_tail_tokens",
        default: 20_000,
        floor: 4_000,
        ceiling: 80_000,
        unit: "tokens",
        doc: "recent turns the compiler never elides; larger protects live chains",
    },
    Knob {
        env: "LOOM_INITIAL_TURNS",
        name: "initial_turns",
        default: 50,
        floor: 8,
        ceiling: 50,
        unit: "turns",
        doc: "turns granted up front (engine default: the full ceiling); smaller grants cut wasted spins on hopeless tasks",
    },
    Knob {
        env: "LOOM_EXTENSION_TURNS",
        name: "extension_turns",
        default: 8,
        floor: 2,
        ceiling: 16,
        unit: "turns",
        doc: "turns added per progress-evidence grant (engine default: off); the staged-budget lever of item 26",
    },
    Knob {
        env: "LOOM_MAX_TURN_EXTENSIONS",
        name: "max_turn_extensions",
        default: 2,
        floor: 1,
        ceiling: 5,
        unit: "grants",
        doc: "extension grants per task (engine default: off); bounds how far initial_turns can grow toward the ceiling",
    },
];

/// Looks a knob up by its env var name.
#[must_use]
pub fn knob_by_env(env: &str) -> Option<&'static Knob> {
    KNOBS.iter().find(|knob| knob.env == env)
}

/// An env overlay: knob env var → value. Ordered, so serialization is
/// deterministic.
pub type Overlay = BTreeMap<String, String>;

/// Validates an overlay against the knob table: every key must be a
/// known seam, every value an integer inside the knob's bounds.
///
/// # Errors
///
/// Returns the reason as a `String` when a key is not a known knob env
/// var, when a value is not an integer, or when a value falls outside
/// the knob's `[floor, ceiling]` bounds.
pub fn validate_overlay(overlay: &Overlay) -> Result<(), String> {
    for (env, value) in overlay {
        let knob = knob_by_env(env).ok_or_else(|| format!("unknown knob: {env}"))?;
        let parsed: u64 = value
            .parse()
            .map_err(|_| format!("{env}: not an integer: {value}"))?;
        if parsed < knob.floor || parsed > knob.ceiling {
            return Err(format!(
                "{env}: {parsed} outside [{}, {}]",
                knob.floor, knob.ceiling
            ));
        }
    }
    Ok(())
}

/// Deterministic, filesystem-safe label for an overlay — the knob
/// short names and values, joined: `prune_trigger_percent-70` or
/// `prune_trigger_percent-70+read_head_bytes-32768`. Used for probe
/// directories and report rows.
#[must_use]
pub fn overlay_label(overlay: &Overlay) -> String {
    if overlay.is_empty() {
        return "baseline".to_owned();
    }
    let mut parts: Vec<String> = overlay
        .iter()
        .map(|(env, value)| {
            let name = knob_by_env(env).map_or("knob", |knob: &Knob| knob.name);
            format!("{name}-{value}")
        })
        .collect();
    parts.sort();
    parts.join("+")
}
