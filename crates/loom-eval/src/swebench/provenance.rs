//! Sweep provenance: the engine-build and script digests stamped into
//! every sweep report, plus the mixed-build aggregation guard.

use std::path::Path;

use super::container::{AgentKind, container_script};
#[cfg(test)]
use super::container::{CONTAINER_SCRIPT, MINI_CONTAINER_SCRIPT};

/// The frozen prompt's version marker (doc comment on
/// [`crate::swebench::build_prompt`] is the changelog; this is the machine-readable
/// digest input). Bumping it invalidates every recorded baseline.
pub const PROMPT_VERSION: &str = "5";

/// Provenance stamped into every sweep report so a headline number is
/// attributable to one engine build, one container script, and one
/// prompt version (honesty rules in docs/eval.md). The binary digest
/// is the strong claim: the staged linux `loom` is hashed on the host
/// (executing the ELF is impossible there), so a rebuilt binary can
/// never silently extend a sweep recorded under the old build — the
/// exact failure behind the v3-100 headline spanning two builds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepProvenance {
    /// sha256 of the staged linux loom binary, hex. `None` for agent
    /// arms that run no loom build (mini) — there is no engine build
    /// to attribute, and the span guard treats it like the
    /// pre-provenance legacy case.
    pub loom_sha256: Option<String>,
    /// Size of the staged binary in bytes (cheap cross-check).
    pub loom_bytes: Option<u64>,
    /// sha256 of the frozen container script, hex.
    pub container_script_sha256: String,
    /// [`PROMPT_VERSION`] at sweep time.
    pub prompt_version: &'static str,
    /// loom-eval's own package version (the runner's identity).
    pub runner_version: &'static str,
    /// Reasoning-effort schedule the sweep ran under (roadmap item 9):
    /// `fixed` or `progressive`. Recorded so an effort A/B's arms are
    /// identifiable from the report alone.
    pub reasoning_schedule: String,
}

/// sha256 of `bytes` as lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Reads and hashes the staged binary (when the arm runs one); fails
/// loudly rather than stamping a report with no provenance.
pub fn sweep_provenance(
    loom_bin: Option<&Path>,
    agent: AgentKind,
    reasoning_schedule: &str,
) -> std::io::Result<SweepProvenance> {
    let loom_sha256 = match loom_bin {
        Some(loom_bin) => {
            let bytes = std::fs::read(loom_bin)?;
            Some((sha256_hex(&bytes), bytes.len() as u64))
        }
        None => None,
    };
    Ok(SweepProvenance {
        loom_sha256: loom_sha256.as_ref().map(|(digest, _)| digest.clone()),
        loom_bytes: loom_sha256.map(|(_, bytes)| bytes),
        container_script_sha256: sha256_hex(container_script(agent).as_bytes()),
        prompt_version: PROMPT_VERSION,
        runner_version: env!("CARGO_PKG_VERSION"),
        reasoning_schedule: reasoning_schedule.to_owned(),
    })
}

/// The digest recorded in a sweep report's `provenance.loom_sha256`
/// (absent in pre-2026-10-25 reports).
pub fn recorded_loom_digest(sweep_json: &serde_json::Value) -> Option<&str> {
    sweep_json.get("provenance")?.get("loom_sha256")?.as_str()
}

/// One headline, one engine build: aggregating (contrasting, summing)
/// sweep reports that record *different* binary digests mixes builds
/// into one number and is refused. Reports without provenance
/// predate the rule and pass — callers warn on them separately.
pub fn span_one_build(left: &serde_json::Value, right: &serde_json::Value) -> Result<(), String> {
    let (Some(left), Some(right)) = (recorded_loom_digest(left), recorded_loom_digest(right))
    else {
        return Ok(());
    };
    if left == right {
        Ok(())
    } else {
        Err(format!(
            "sweep reports span two engine builds ({left} vs {right}); \
             a headline number must come from one build — re-run the \
             later batch under a single binary before aggregating"
        ))
    }
}

// ------------------------------------------------------------ tests ---

#[test]
fn sha256_matches_the_known_vector() {
    // FIPS 180-4 test vector: sha256("abc").
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}

#[test]
fn sweep_provenance_hashes_the_staged_binary_and_script() {
    let dir = std::env::temp_dir().join(format!("loom-prov-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("loom");
    std::fs::write(&bin, b"pretend elf").unwrap();
    let provenance = sweep_provenance(Some(&bin), AgentKind::Loom, "fixed").unwrap();
    assert_eq!(
        provenance.loom_sha256.as_deref(),
        Some(sha256_hex(b"pretend elf").as_str())
    );
    assert_eq!(provenance.loom_bytes, Some(11));
    assert_eq!(
        provenance.container_script_sha256,
        sha256_hex(CONTAINER_SCRIPT.as_bytes())
    );
    assert_eq!(provenance.prompt_version, PROMPT_VERSION);
    // A different binary must yield a different digest — the whole
    // point of the mixed-build guard.
    std::fs::write(&bin, b"pretend elf v2").unwrap();
    let rebuilt = sweep_provenance(Some(&bin), AgentKind::Loom, "fixed").unwrap();
    assert_ne!(rebuilt.loom_sha256, provenance.loom_sha256);
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(test)]
fn sweep_with_digest(digest: Option<&str>) -> serde_json::Value {
    let mut report = serde_json::json!({"model": "m", "instances": []});
    if let Some(digest) = digest {
        report["provenance"]["loom_sha256"] = serde_json::json!(digest);
    }
    report
}

#[test]
fn span_one_build_accepts_matching_and_legacy_reports() {
    // Same build twice — fine.
    assert!(
        span_one_build(
            &sweep_with_digest(Some("aa")),
            &sweep_with_digest(Some("aa"))
        )
        .is_ok()
    );
    // Pre-provenance (legacy) reports carry no digest — allowed, the
    // caller warns.
    assert!(span_one_build(&sweep_with_digest(Some("aa")), &sweep_with_digest(None)).is_ok());
    assert!(span_one_build(&sweep_with_digest(None), &sweep_with_digest(None)).is_ok());
}

#[test]
fn span_one_build_refuses_mixed_builds() {
    let error = span_one_build(
        &sweep_with_digest(Some("aa")),
        &sweep_with_digest(Some("bb")),
    )
    .unwrap_err();
    assert!(error.contains("two engine builds"), "{error}");
    assert!(error.contains("aa") && error.contains("bb"), "{error}");
}

#[test]
fn provenance_hashes_the_selected_agents_script() {
    let dir = std::env::temp_dir().join(format!("loom-prov-mini-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("loom");
    std::fs::write(&bin, b"pretend elf").unwrap();
    let loom = sweep_provenance(Some(&bin), AgentKind::Loom, "fixed").expect("provenance");
    let mini = sweep_provenance(Some(&bin), AgentKind::Mini, "fixed").expect("provenance");
    assert_eq!(
        loom.container_script_sha256,
        sha256_hex(CONTAINER_SCRIPT.as_bytes())
    );
    assert_eq!(
        mini.container_script_sha256,
        sha256_hex(MINI_CONTAINER_SCRIPT.as_bytes())
    );
    assert_ne!(loom.container_script_sha256, mini.container_script_sha256);
}
