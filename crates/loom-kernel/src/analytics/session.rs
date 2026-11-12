//! The transcript-derived statistics pass: [`session_stats`] over one
//! persisted session and [`sessions_overview`] over the whole store.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::stats::{SessionStats, Spill};
use crate::id::SessionId;
use crate::session::Session;
use crate::store::{SessionStore, StoreError};
use crate::transcript::{Item, ToolStatus};

/// Computes [`SessionStats`] from a persisted session.
///
/// Pure: one pass over the transcript, no I/O, no clocks. Assistant
/// tool-call metadata (name, canonical arguments) is joined to each
/// result by call id, exactly as the projection does.
#[must_use]
pub fn session_stats(session: &Session) -> SessionStats {
    let mut stats = SessionStats::default();
    let mut calls: HashMap<&str, (&str, String)> = HashMap::new();
    // `load_skill` calls only: call id → (menu name, asked for a
    // bundled resource). Joined to results below so per-skill counters
    // reflect outcomes, not attempts.
    let mut skill_calls: HashMap<&str, (String, bool)> = HashMap::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();

    for item in &session.transcript {
        match item {
            Item::User { .. } => {}
            Item::Assistant(assistant) => {
                stats.turns += 1;
                for call in &assistant.tool_calls {
                    // Canonical key: serialized arguments with object
                    // keys byte-sorted (independent of serde_json map
                    // ordering), the same string the identical-call
                    // ledger keys on.
                    let canonical =
                        crate::canonical::canonical_json(&Value::from(call.arguments.clone()));
                    if !seen.insert((call.name.clone(), canonical.clone())) {
                        stats.identical_repeats += 1;
                    }
                    calls.insert(call.id.as_str(), (&call.name, canonical));
                    if call.name == "load_skill" {
                        let args = Value::from(call.arguments.clone());
                        let name = args
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("?")
                            .to_owned();
                        let resource = args.get("resource").is_some();
                        skill_calls.insert(call.id.as_str(), (name, resource));
                    }
                }
            }
            Item::ToolResult(result) => {
                static UNKNOWN: (&str, String) = ("?", String::new());
                let (name, _canonical) = calls.get(result.call_id.as_str()).unwrap_or(&UNKNOWN);
                let name = (*name).to_owned();

                let counts = stats.tools.entry(name.clone()).or_default();
                counts.record(&result.status);
                stats.totals.record(&result.status);
                stats.repairs += result.effects.repairs.len() as u64;

                if name == "edit_file" || name == "write_file" {
                    stats.edit_apply.attempts += 1;
                    if matches!(result.status, ToolStatus::Success) {
                        stats.edit_apply.applied += 1;
                    }
                }

                // Skill telemetry: outcome-joined per skill (roadmap
                // gap — skill lifecycle). Success pays its delivered
                // bytes into the skill's context cost; any error is a
                // failure for the name the model asked for. Cancelled
                // loads delivered and judged nothing.
                if name == "load_skill"
                    && let Some((skill, resource)) = skill_calls.get(result.call_id.as_str())
                {
                    let counts = stats.skills.entry(skill.clone()).or_default();
                    match &result.status {
                        ToolStatus::Success => {
                            if *resource {
                                counts.resources += 1;
                            } else {
                                counts.loads += 1;
                            }
                            counts.delivered_bytes += result.content.len() as u64;
                        }
                        ToolStatus::Error(_) => counts.failures += 1,
                        ToolStatus::Cancelled { .. } => {}
                    }
                }

                if let Some(reference) = &result.artifact {
                    stats.spills.push(Spill {
                        call_id: result.call_id.as_str().to_owned(),
                        tool: name,
                        delivered_bytes: result.content.len() as u64,
                        artifact_bytes: reference.bytes,
                        artifact_id: reference.id.to_string(),
                    });
                }
            }
        }
    }

    stats.malformed_calls = stats.totals.malformed();
    stats
}

/// One analyzed session from a [`sessions_overview`] pass.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionOverview {
    /// The session's id (v7, creation-ordered; also the label a
    /// frontend filters on).
    pub id: SessionId,
    /// Transcript-derived statistics for the session.
    pub stats: SessionStats,
}

/// Store-wide transcript analytics: every analyzed session plus the
/// totals folded across them.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionsOverview {
    /// One row per analyzed session, oldest creation order.
    pub rows: Vec<SessionOverview>,
    /// Every row folded into one value ([`SessionStats::add`]).
    pub totals: SessionStats,
}

/// Analyzes every session in `store`, optionally keeping only the ones
/// whose id contains `filter`.
///
/// This is the whole of `loom stats`' data step — the iteration,
/// filtering, and folding are policy, not presentation, so every
/// frontend lists sessions identically. Derives each row with
/// [`session_stats`]; an unreadable session fails the pass (the store
/// is the source of truth, and silently skipping a row would
/// undercount).
pub fn sessions_overview(
    store: &dyn SessionStore,
    filter: Option<&str>,
) -> Result<SessionsOverview, StoreError> {
    // v7 ids order by creation time; sort regardless so the listing is
    // deterministic whatever the store's iteration order.
    let mut ids = store.session_ids()?;
    ids.sort();

    let mut overview = SessionsOverview::default();
    for id in ids {
        let Some(session) = store.load(&id)? else {
            continue;
        };
        if let Some(filter) = filter
            && !id.to_string().contains(filter)
        {
            continue;
        }
        let stats = session_stats(&session);
        overview.totals.add(&stats);
        overview.rows.push(SessionOverview { id, stats });
    }
    Ok(overview)
}
