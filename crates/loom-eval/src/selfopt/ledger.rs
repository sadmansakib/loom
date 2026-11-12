//! The lessons ledger: contrast-supported lessons and one record per
//! loop iteration, loaded and appended atomically.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::compare::Decision;
use super::knobs::Overlay;
use super::proposals::Proposal;

/// A retained lesson — GEPA's discipline: only contrast-supported
/// statements survive. The evidence string names the probes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lesson {
    /// One-sentence imperative ("prune at 70% for long test-loop tasks").
    pub statement: String,
    /// The probe contrasts that back it (labels + numbers).
    pub evidence: String,
}

/// One loop iteration's record — the ledger entry is the unit a later
/// run reads (via [`build_proposal_prompt`]) instead of re-deriving.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    /// RFC 3339-ish local timestamp (the binary formats it).
    pub timestamp: String,
    /// Sweep report path the baseline came from.
    pub sweep: String,
    /// Provider / model the probes ran.
    pub provider: String,
    /// Model the probes ran.
    pub model: String,
    /// The overlay the baseline ran with (usually empty = shipped).
    pub base_overlay: Overlay,
    /// Proposals considered (label, class, hypothesis).
    pub proposals: Vec<Proposal>,
    /// Per-proposal decisions.
    pub decisions: Vec<Decision>,
    /// Labels adopted into the merged overlay.
    pub adopted: Vec<String>,
    /// Labels promoted but not deployed (events gate blocked): kept
    /// as candidates for later sweeps. Serde-defaulted — old ledgers
    /// predate the promotion/deployment split.
    #[serde(default)]
    pub promoted: Vec<String>,
    /// The merged overlay that resulted (empty when nothing adopted).
    pub merged: Overlay,
    /// Contrast-supported lessons only.
    pub lessons: Vec<Lesson>,
}

/// Reads the ledger; a missing file is an empty ledger (first run).
pub fn load_ledger(path: &Path) -> std::io::Result<Vec<LedgerEntry>> {
    let Ok(body) = std::fs::read_to_string(path) else {
        return Ok(Vec::new());
    };
    serde_json::from_str(&body).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{}: {error}", path.display()),
        )
    })
}

/// Appends one entry, rewriting the whole ledger atomically (write to
/// a sibling temp file, rename over) so a crash never truncates it.
pub fn append_ledger(path: &Path, entry: &LedgerEntry) -> std::io::Result<()> {
    let mut entries = load_ledger(path)?;
    entries.push(entry.clone());
    let body = serde_json::to_string_pretty(&entries).map_err(std::io::Error::other)?;
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, body + "\n")?;
    std::fs::rename(&temp, path)
}
