//! Artifact lifecycle: reference counting and a bounded GC (roadmap 2.17).
//!
//! Artifact bodies are immutable and content-addressed (ADR-012), and a
//! transcript reference is never dangling — the store writes the body
//! before the turn commits. The one leak the design permits is the
//! opposite: an orphan, a body written but never referenced (a crash
//! between the write and the commit, a tool result the loop replaced,
//! a session that was never saved). Orphans are harmless but permanent,
//! so this module gives them a lifecycle:
//!
//! - [`referenced_artifacts`] walks every stored session's transcript
//!   and collects the ids its tool results carry — the live set.
//! - [`collect`] deletes only ids outside that set, in two bounded
//!   passes: age (orphans older than [`GcBounds::max_age_millis`]) and
//!   total size (oldest orphans first until the store fits
//!   [`GcBounds::max_total_bytes`]). A referenced body is never
//!   deleted, whatever its age or the store's size.
//!
//! The bounds are a backstop, not a quota: bodies are small relative to
//! the sessions they serve, and the common case is a store that never
//! reaches either bound. The engine runs the GC at open, best-effort —
//! a failure warns and never blocks the run.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::id::ArtifactId;
use crate::store::{ArtifactStat, ArtifactStore, SessionStore, StoreError};
use crate::transcript::Item;

/// The GC's backstop bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct GcBounds {
    /// Orphans older than this (ms) are deleted outright.
    pub max_age_millis: u64,
    /// If the store still exceeds this after the age pass, the oldest
    /// remaining orphans are deleted until it fits. Referenced bodies
    /// never count as deletable, however far over the bound they are.
    pub max_total_bytes: u64,
}

impl Default for GcBounds {
    /// 30 days and 256 MiB: long enough that a working session never
    /// sees a deletion, tight enough that an abandoned database cannot
    /// grow unbounded between runs.
    fn default() -> Self {
        Self {
            max_age_millis: 30 * 24 * 60 * 60 * 1000,
            max_total_bytes: 256 * 1024 * 1024,
        }
    }
}

/// What one GC run did — serialized into trace/manifest output when the
/// engine logs the pass, so an operator can see what was reclaimed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GcReport {
    /// How many bodies the listing found.
    pub scanned: u64,
    /// How many of those the transcript scan marked live.
    pub referenced: u64,
    /// The bodies this run deleted, oldest first.
    pub deleted: Vec<ArtifactStat>,
    /// Total body bytes before the run.
    pub bytes_before: u64,
    /// Total body bytes after the run.
    pub bytes_after: u64,
}

/// Current time in milliseconds since the Unix epoch (saturating, so a
/// clock before the epoch reads as zero rather than panicking).
#[must_use]
pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}

/// Collects the ids every stored session's transcript still references.
///
/// Every stored session is a live transcript — there is no archive tier —
/// so this is the complete live set: an id is deletable only if no stored
/// tool result carries it.
pub fn referenced_artifacts(
    sessions: &dyn SessionStore,
) -> Result<HashSet<ArtifactId>, StoreError> {
    let mut referenced = HashSet::new();
    for session_id in sessions.session_ids()? {
        let Some(session) = sessions.load(&session_id)? else {
            continue;
        };
        for item in &session.transcript {
            let Item::ToolResult(result) = item else {
                continue;
            };
            if let Some(reference) = &result.artifact {
                referenced.insert(reference.id);
            }
            // Tier-2 pre-image bodies (rewind gap 1) are restore
            // promises: the transcript claims "this was the path's
            // content immediately before that call", and a rewind may
            // act on that claim at any later date. They ride the same
            // live set as tool-result spills so the GC can never break
            // a future restore.
            for image in &result.effects.before_images {
                if let Some(reference) = &image.content {
                    referenced.insert(reference.id);
                }
            }
        }
    }
    Ok(referenced)
}

/// Runs the two-pass GC, deleting only unreferenced bodies.
///
/// Pure given its inputs — the clock arrives as `now_millis`, the live
/// set as `referenced` — so tests can place artifacts in the past or
/// future without touching the store's write path.
pub fn collect(
    artifacts: &dyn ArtifactStore,
    referenced: &HashSet<ArtifactId>,
    now_millis: u64,
    bounds: GcBounds,
) -> Result<GcReport, StoreError> {
    let stats = artifacts.list()?;
    let scanned = stats.len() as u64;
    let referenced_count = stats
        .iter()
        .filter(|stat| referenced.contains(&stat.id))
        .count() as u64;
    let bytes_before: u64 = stats.iter().map(|stat| stat.bytes).sum();
    let mut bytes_after = bytes_before;
    let mut deleted = Vec::new();

    // Both passes walk the listing oldest-first, so the age pass and the
    // size pass agree on "oldest" without a second sort.
    for stat in &stats {
        if referenced.contains(&stat.id) {
            continue;
        }
        let age = now_millis.saturating_sub(stat.created_at);
        if age >= bounds.max_age_millis {
            artifacts.delete(&stat.id)?;
            deleted.push(stat.clone());
            bytes_after = bytes_after.saturating_sub(stat.bytes);
        }
    }
    for stat in &stats {
        if referenced.contains(&stat.id) || deleted.iter().any(|gone| gone.id == stat.id) {
            continue;
        }
        if bytes_after <= bounds.max_total_bytes {
            break;
        }
        artifacts.delete(&stat.id)?;
        deleted.push(stat.clone());
        bytes_after = bytes_after.saturating_sub(stat.bytes);
    }
    Ok(GcReport {
        scanned,
        referenced: referenced_count,
        deleted,
        bytes_before,
        bytes_after,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::ArtifactRef;
    use crate::id::SessionId;
    use crate::model::StopReason;
    use crate::session::Session;
    use crate::store::{MemoryArtifactStore, MemorySessionStore, StoredArtifact};
    use crate::tool::BeforeImage;
    use crate::transcript::{
        AssistantMessage, ToolArguments, ToolCall, ToolCallId, ToolResult, ToolStatus,
    };

    fn call_id(id: &str) -> ToolCallId {
        ToolCallId::try_from(id.to_owned()).unwrap()
    }

    fn assistant(text: &str, calls: Vec<ToolCall>) -> AssistantMessage {
        AssistantMessage {
            content: text.to_owned(),
            reasoning: None,
            stop_reason: if calls.is_empty() {
                StopReason::EndTurn
            } else {
                StopReason::ToolCalls
            },
            tool_calls: calls,
        }
    }

    fn referencing_session(reference: ArtifactRef) -> MemorySessionStore {
        let mut result = ToolResult::new(call_id("call_1"), ToolStatus::Success, "reduced", false);
        result.artifact = Some(reference);
        session_with(result)
    }

    /// A session whose only artifact reference is a tier-2 pre-image
    /// riding the tool effects (no result spill at all).
    fn pre_image_session(reference: ArtifactRef) -> MemorySessionStore {
        let mut result = ToolResult::new(call_id("call_1"), ToolStatus::Success, "wrote", false);
        result.effects = result.effects.with_file_written("src/lib.rs");
        result
            .effects
            .before_images
            .push(BeforeImage::captured("src/lib.rs", reference));
        session_with(result)
    }

    fn session_with(result: ToolResult) -> MemorySessionStore {
        let session = Session {
            id: SessionId::new(),
            transcript: vec![
                Item::User {
                    text: "hi".to_owned(),
                },
                Item::Assistant(assistant(
                    "working",
                    vec![ToolCall {
                        id: call_id("call_1"),
                        name: "run_command".to_owned(),
                        arguments: ToolArguments::try_from(serde_json::json!({})).unwrap(),
                    }],
                )),
                Item::ToolResult(result),
            ],
            tasks: Vec::new(),
            todos: crate::session::TodoList::default(),
        };
        MemorySessionStore::with_session(session)
    }

    fn put(artifacts: &MemoryArtifactStore, id: ArtifactId, body: &[u8]) {
        let stored = StoredArtifact::new(
            id,
            "tool_output",
            crate::artifact::sha256_hex(body),
            body.to_vec(),
        );
        artifacts.put(&stored).unwrap();
    }

    fn reference_to(id: ArtifactId, bytes: u64) -> ArtifactRef {
        ArtifactRef::new(id, "tool_output", bytes, "deadbeef")
    }

    #[test]
    fn an_empty_store_collects_nothing() {
        let artifacts = MemoryArtifactStore::new();
        let report = collect(
            &artifacts,
            &HashSet::new(),
            now_millis(),
            GcBounds::default(),
        )
        .unwrap();
        assert_eq!(report.scanned, 0);
        assert_eq!(report.referenced, 0);
        assert!(report.deleted.is_empty());
        assert_eq!(report.bytes_before, 0);
        assert_eq!(report.bytes_after, 0);
    }

    #[test]
    fn the_scan_finds_transcript_references() {
        let id = ArtifactId::new();
        let sessions = referencing_session(reference_to(id, 10));
        let referenced = referenced_artifacts(&sessions).unwrap();
        assert!(referenced.contains(&id));
    }

    #[test]
    fn a_referenced_body_survives_any_age_and_size() {
        let artifacts = MemoryArtifactStore::new();
        let id = ArtifactId::new();
        put(&artifacts, id, &[7; 512]);
        let sessions = referencing_session(reference_to(id, 512));
        let referenced = referenced_artifacts(&sessions).unwrap();

        // Far past the age bound and over the size bound: still live.
        let bounds = GcBounds {
            max_age_millis: 1,
            max_total_bytes: 1,
        };
        let now = now_millis() + bounds.max_age_millis + 1;
        let report = collect(&artifacts, &referenced, now, bounds).unwrap();
        assert_eq!(report.scanned, 1);
        assert_eq!(report.referenced, 1);
        assert!(report.deleted.is_empty());
        assert!(artifacts.get(&id).unwrap().is_some());
    }

    #[test]
    fn orphans_past_the_age_bound_are_deleted() {
        let artifacts = MemoryArtifactStore::new();
        let first = ArtifactId::new();
        let second = ArtifactId::new();
        put(&artifacts, first, &[1; 128]);
        put(&artifacts, second, &[2; 128]);

        let bounds = GcBounds {
            max_age_millis: 10_000,
            max_total_bytes: u64::MAX,
        };
        // The memory store stamps first-write times with the wall clock,
        // so the test places the run far in the future: both orphans are
        // then past the age bound and both go.
        let now = now_millis() + bounds.max_age_millis + 1;
        let report = collect(&artifacts, &HashSet::new(), now, bounds).unwrap();
        assert_eq!(report.deleted.len(), 2);
        assert!(artifacts.get(&first).unwrap().is_none());
        assert!(artifacts.get(&second).unwrap().is_none());
    }

    #[test]
    fn a_young_orphan_within_bounds_survives() {
        let artifacts = MemoryArtifactStore::new();
        let id = ArtifactId::new();
        put(&artifacts, id, &[1; 128]);

        let report = collect(
            &artifacts,
            &HashSet::new(),
            now_millis(),
            GcBounds::default(),
        )
        .unwrap();
        assert!(report.deleted.is_empty());
        assert!(artifacts.get(&id).unwrap().is_some());
    }

    #[test]
    fn the_size_pass_deletes_oldest_orphans_first() {
        let artifacts = MemoryArtifactStore::new();
        let oldest = ArtifactId::new();
        let middle = ArtifactId::new();
        let newest = ArtifactId::new();
        put(&artifacts, oldest, &[1; 300]);
        put(&artifacts, middle, &[2; 300]);
        put(&artifacts, newest, &[3; 300]);

        let bounds = GcBounds {
            max_age_millis: u64::MAX,
            max_total_bytes: 600,
        };
        let report = collect(&artifacts, &HashSet::new(), now_millis(), bounds).unwrap();
        assert_eq!(report.deleted.len(), 1);
        assert_eq!(report.deleted[0].id, oldest);
        assert_eq!(report.bytes_before, 900);
        assert_eq!(report.bytes_after, 600);
        assert!(artifacts.get(&oldest).unwrap().is_none());
        assert!(artifacts.get(&middle).unwrap().is_some());
        assert!(artifacts.get(&newest).unwrap().is_some());
    }

    #[test]
    fn over_cap_with_only_referenced_bodies_keeps_all() {
        let artifacts = MemoryArtifactStore::new();
        let id = ArtifactId::new();
        put(&artifacts, id, &[9; 1024]);
        let sessions = referencing_session(reference_to(id, 1024));
        let referenced = referenced_artifacts(&sessions).unwrap();

        let bounds = GcBounds {
            max_age_millis: u64::MAX,
            max_total_bytes: 1,
        };
        let report = collect(&artifacts, &referenced, now_millis(), bounds).unwrap();
        assert!(report.deleted.is_empty());
        assert!(artifacts.get(&id).unwrap().is_some());
    }

    #[test]
    fn a_pre_image_body_is_live_and_survives_any_bound() {
        // A pre-image is a restore promise recorded in effects, not a
        // result spill; the GC must honor it however old or large.
        let artifacts = MemoryArtifactStore::new();
        let id = ArtifactId::new();
        put(&artifacts, id, &[5; 256]);
        let sessions = pre_image_session(reference_to(id, 256));
        let referenced = referenced_artifacts(&sessions).unwrap();
        assert!(referenced.contains(&id), "effects must count as live");

        let bounds = GcBounds {
            max_age_millis: 1,
            max_total_bytes: 1,
        };
        let now = now_millis() + bounds.max_age_millis + 1;
        let report = collect(&artifacts, &referenced, now, bounds).unwrap();
        assert!(report.deleted.is_empty());
        assert!(artifacts.get(&id).unwrap().is_some());
    }
}
