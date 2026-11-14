//! Operator trust for workspace manifests (review F-02/F-12).
//!
//! `.loom/commands.toml`, `.loom/verify.toml`, and `.loom/mcp.toml`
//! all grant execution power: policy rules can disable the sandbox,
//! a runner is executed *unsandboxed* on the host, and MCP servers
//! are spawned with declared environments. A cloned or downloaded
//! repository can ship all three — they sit inside the workspace, so
//! path confinement never questions them, and until PR1 the model
//! could write them too. Blocking model writes closed the live
//! channel; this module closes the shipped one:
//!
//! * A manifest is **honored in full** when the operator has pinned
//!   its SHA-256 in the trust store.
//! * A manifest that exists but is **not pinned** (or whose digest no
//!   longer matches the pin) loads *restrictively*: the policy keeps
//!   only safety-preserving fields, the runner and MCP servers fail
//!   closed.
//! * `loom trust` (CLI) reviews and pins the current digests.
//!
//! The store lives *outside* every workspace — `~/.config/loom/
//! trust.toml` — so no repository content can write it. `LOOM_TRUST_FILE`
//! overrides the location (tests, eval harnesses, portable installs).
//! The config-dir precedence mirrors `loom-core`'s `paths::config_dir`;
//! keep the two in sync.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// The manifests the gate knows, as (store key, workspace-relative
/// path) pairs. The store key is stable and short so the file stays
/// human-editable.
pub(crate) const MANIFESTS: [(&str, &str); 3] = [
    ("commands", ".loom/commands.toml"),
    ("verify", ".loom/verify.toml"),
    ("mcp", ".loom/mcp.toml"),
];

/// The trust store file: an explicit in-process override (tests,
/// harnesses), else `LOOM_TRUST_FILE`, else the config dir.
fn trust_file() -> PathBuf {
    if let Some(path) = store_override() {
        return path;
    }
    if let Some(path) = std::env::var_os("LOOM_TRUST_FILE") {
        return PathBuf::from(path);
    }
    // Mirrors loom_core::paths::config_dir (loom-tools cannot depend
    // on loom-core; the dependency points the other way).
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("loom/trust.toml")
}

/// In-process trust-store override, for test binaries and harnesses
/// that must not touch the operator's real store (`unsafe`
/// `set_var` is forbidden workspace-wide, so the environment route is
/// out). Production code never calls this.
static STORE_OVERRIDE: std::sync::RwLock<Option<PathBuf>> = std::sync::RwLock::new(None);

fn store_override() -> Option<PathBuf> {
    // A poisoned lock means a panic mid-write; the override is either
    // set or not, so recover the inner value rather than propagate.
    STORE_OVERRIDE
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Uses `path` as this process's trust store from now on. Test
/// binaries call this as their first action; eval harnesses point it
/// at a store inside their own state directory.
pub fn use_store(path: impl Into<PathBuf>) {
    // See `store_override` on lock poisoning.
    *STORE_OVERRIDE
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(path.into());
}

/// One workspace's pinned manifest digests, keyed by workspace path.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Store {
    workspace: BTreeMap<String, WorkspacePins>,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct WorkspacePins {
    commands: Option<String>,
    verify: Option<String>,
    mcp: Option<String>,
    /// Operator chose "never ask again" for file writes in this workspace.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    writes_allowed: bool,
}

/// What the gate decided about one manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Trust {
    /// No manifest exists — nothing to gate.
    Absent,
    /// The manifest's digest matches the operator's pin.
    Trusted,
    /// The manifest exists but is not pinned (or drifted). Callers
    /// load restrictively or fail closed.
    Untrusted,
}

/// Pins the digests of every present manifest under `workspace` in
/// the operator's trust store (`LOOM_TRUST_FILE`-aware). The surface
/// for `loom trust` and for harnesses that construct workspaces
/// programmatically (eval testbeds). Returns the relative manifest
/// paths pinned.
///
/// # Errors
///
/// Fails when the trust store cannot be written (permissions, disk).
pub fn pin(workspace: &Path) -> std::io::Result<Vec<String>> {
    TrustGate::open().pin(workspace)
}

/// Whether `workspace` has any pinned manifest in the operator's trust
/// store. A workspace with no pins is a first run: nothing is trusted yet.
#[must_use]
pub fn is_pinned(workspace: &Path) -> bool {
    TrustGate::open()
        .load()
        .workspace
        .contains_key(&TrustGate::workspace_key(workspace))
}

/// Whether the operator chose "never ask again" for file writes in
/// `workspace`. Survives restarts: the choice lives in the trust store.
#[must_use]
pub fn writes_allowed(workspace: &Path) -> bool {
    TrustGate::open()
        .load()
        .workspace
        .get(&TrustGate::workspace_key(workspace))
        .is_some_and(|pins| pins.writes_allowed)
}

/// Records "never ask again" for file writes in `workspace`.
///
/// # Errors
///
/// Fails when the trust store cannot be written (permissions, disk).
pub fn allow_writes(workspace: &Path) -> std::io::Result<()> {
    TrustGate::open().set_writes_allowed(workspace)
}

/// One manifest's trust state, for `loom trust --status`.
#[derive(Debug, Clone)]
pub struct ManifestStatus {
    /// The workspace-relative manifest path.
    pub relative: String,
    /// The manifest exists on disk.
    pub present: bool,
    /// The digest matches the operator's pin (absent manifests count
    /// as trusted: nothing to gate).
    pub trusted: bool,
    /// The current SHA-256, when the manifest exists.
    pub digest: Option<String>,
}

/// Reports every gated manifest's trust state for `workspace`.
#[must_use]
pub fn status(workspace: &Path) -> Vec<ManifestStatus> {
    let gate = TrustGate::open();
    MANIFESTS
        .iter()
        .map(|(_, relative)| {
            let digest = TrustGate::digest_of(&workspace.join(relative));
            let present = digest.is_some();
            let trusted = gate.decide(workspace, relative) != Trust::Untrusted;
            ManifestStatus {
                relative: (*relative).to_owned(),
                present,
                trusted,
                digest,
            }
        })
        .collect()
}

/// Reads and writes the operator's manifest trust store. Cheap to
/// construct; every question re-reads the store so out-of-band edits
/// (an operator re-running `loom trust`, a rotated manifest) are
/// picked up without an engine restart.
#[derive(Debug, Clone)]
pub(crate) struct TrustGate {
    path: PathBuf,
}

impl TrustGate {
    /// Opens the trust store at its resolved location.
    pub(crate) fn open() -> Self {
        Self { path: trust_file() }
    }

    /// A gate pointed at an explicit store file — in-crate tests
    /// pinning the tempdir workspaces they construct.
    #[cfg(test)]
    pub(crate) fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    fn load(&self) -> Store {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Store::default();
        };
        toml::from_str(&text).unwrap_or_else(|error| {
            // A corrupt store must not widen execution: treat it as
            // pinning nothing and say so loudly.
            tracing::warn!(path = %self.path.display(), %error, "trust store unreadable");
            Store::default()
        })
    }

    /// SHA-256 of the file's bytes, hex-encoded; `None` when the
    /// file does not exist.
    fn digest_of(path: &Path) -> Option<String> {
        use sha2::{Digest, Sha256};
        let bytes = std::fs::read(path).ok()?;
        Some(
            Sha256::digest(&bytes)
                .iter()
                .fold(String::new(), |mut hex, byte| {
                    let _ = write!(hex, "{byte:02x}");
                    hex
                }),
        )
    }

    /// Decides `manifest` (a `MANIFESTS` relative path) for
    /// `workspace`.
    pub(crate) fn decide(&self, workspace: &Path, relative: &str) -> Trust {
        let path = workspace.join(relative);
        let Some(actual) = Self::digest_of(&path) else {
            return Trust::Absent;
        };
        let key = Self::key_for(relative);
        let pinned = self
            .load()
            .workspace
            .get(&Self::workspace_key(workspace))
            .and_then(|pins| pins.field(key).cloned());
        match pinned {
            Some(expected) if expected == actual => Trust::Trusted,
            _ => Trust::Untrusted,
        }
    }

    /// The store key for a workspace: its canonical path when it
    /// exists, else the path as supplied. Both `decide` and `pin` go
    /// through this, so a caller that canonicalizes (the engine) and
    /// one that does not (a test) agree.
    fn workspace_key(workspace: &Path) -> String {
        workspace
            .canonicalize()
            .unwrap_or_else(|_| workspace.to_path_buf())
            .display()
            .to_string()
    }

    fn key_for(relative: &str) -> &'static str {
        MANIFESTS
            .iter()
            .find(|(_, path)| *path == relative)
            .map_or("commands", |(key, _)| *key)
    }

    /// Pins the digests of every present manifest under `workspace`,
    /// creating the store on first use. Returns the relative paths
    /// pinned, so the CLI can show exactly what it did.
    ///
    /// The read-modify-write is serialized in-process (a mutex) and
    /// lands via an atomic rename, so concurrent pins — parallel test
    /// threads sharing a store, a harness pinning testbeds while an
    /// engine reads — cannot drop each other's entries or tear a
    /// reader's view.
    /// Records the "never ask again" write choice for `workspace`, under
    /// the same lock and atomic rename as [`TrustGate::pin`].
    pub(crate) fn set_writes_allowed(&self, workspace: &Path) -> std::io::Result<()> {
        static PIN_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = PIN_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut store = self.load();
        store
            .workspace
            .entry(Self::workspace_key(workspace))
            .or_default()
            .writes_allowed = true;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(&store).map_err(std::io::Error::other)?;
        crate::atomic::atomic_write(&self.path, text.as_bytes())
    }

    pub(crate) fn pin(&self, workspace: &Path) -> std::io::Result<Vec<String>> {
        static PIN_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = PIN_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = Self::workspace_key(workspace);
        let mut store = self.load();
        let entry = store.workspace.entry(root).or_default();
        let mut pinned = Vec::new();
        for (key, relative) in MANIFESTS {
            if let Some(digest) = Self::digest_of(&workspace.join(relative)) {
                entry.set_field(key, digest);
                pinned.push(relative.to_owned());
            }
        }
        if pinned.is_empty() {
            return Ok(pinned);
        }
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(&store).map_err(std::io::Error::other)?;
        crate::atomic::atomic_write(&self.path, text.as_bytes())?;
        Ok(pinned)
    }
}

impl WorkspacePins {
    fn field(&self, key: &str) -> Option<&String> {
        match key {
            "verify" => self.verify.as_ref(),
            "mcp" => self.mcp.as_ref(),
            _ => self.commands.as_ref(),
        }
    }

    fn set_field(&mut self, key: &str, digest: String) {
        match key {
            "verify" => self.verify = Some(digest),
            "mcp" => self.mcp = Some(digest),
            _ => self.commands = Some(digest),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate_and_workspace() -> (tempfile::TempDir, TrustGate) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = dir.path().join("trust.toml");
        (dir, TrustGate { path: store })
    }

    #[test]
    fn absent_manifest_is_nothing_to_gate() {
        let (dir, gate) = gate_and_workspace();
        assert_eq!(
            gate.decide(dir.path(), ".loom/commands.toml"),
            Trust::Absent
        );
    }

    #[test]
    fn unpinned_manifest_is_untrusted() {
        let (dir, gate) = gate_and_workspace();
        std::fs::create_dir_all(dir.path().join(".loom")).expect("dir");
        std::fs::write(dir.path().join(".loom/commands.toml"), "[[command]]\n").expect("write");
        assert_eq!(
            gate.decide(dir.path(), ".loom/commands.toml"),
            Trust::Untrusted
        );
    }

    #[test]
    fn pinning_then_deciding_round_trips() {
        let (dir, gate) = gate_and_workspace();
        std::fs::create_dir_all(dir.path().join(".loom")).expect("dir");
        std::fs::write(
            dir.path().join(".loom/verify.toml"),
            "runner = { command = \"x\" }\n",
        )
        .expect("write");
        let pinned = gate.pin(dir.path()).expect("pin");
        assert_eq!(pinned, vec![".loom/verify.toml"]);
        assert_eq!(gate.decide(dir.path(), ".loom/verify.toml"), Trust::Trusted);
        // Drift breaks trust: the pin is a digest, not a blanket yes.
        std::fs::write(
            dir.path().join(".loom/verify.toml"),
            "runner = { command = \"y\" }\n",
        )
        .expect("rewrite");
        assert_eq!(
            gate.decide(dir.path(), ".loom/verify.toml"),
            Trust::Untrusted
        );
        // Re-pinning records the new digest.
        gate.pin(dir.path()).expect("re-pin");
        assert_eq!(gate.decide(dir.path(), ".loom/verify.toml"), Trust::Trusted);
    }

    #[test]
    fn workspace_with_no_pins_is_a_first_run() {
        let (dir, gate) = gate_and_workspace();
        let key = TrustGate::workspace_key(dir.path());
        assert!(!gate.load().workspace.contains_key(&key));
        std::fs::create_dir_all(dir.path().join(".loom")).expect("dir");
        std::fs::write(dir.path().join(".loom/mcp.toml"), "[mcp]\n").expect("write");
        gate.pin(dir.path()).expect("pin");
        assert!(gate.load().workspace.contains_key(&key));
    }

    #[test]
    fn pinning_spares_other_workspaces() {
        let (dir, gate) = gate_and_workspace();
        std::fs::create_dir_all(dir.path().join(".loom")).expect("dir");
        std::fs::write(dir.path().join(".loom/mcp.toml"), "[mcp]\n").expect("write");
        gate.pin(dir.path()).expect("pin");
        let other = tempfile::tempdir().expect("other");
        std::fs::create_dir_all(other.path().join(".loom")).expect("dir");
        std::fs::write(other.path().join(".loom/mcp.toml"), "[mcp]\n").expect("write");
        // Byte-identical manifest, unpinned workspace: still untrusted.
        assert_eq!(
            gate.decide(other.path(), ".loom/mcp.toml"),
            Trust::Untrusted
        );
    }

    #[test]
    fn writes_choice_survives_a_reopen() {
        let (dir, gate) = gate_and_workspace();
        assert!(
            !gate
                .load()
                .workspace
                .values()
                .any(|pins| pins.writes_allowed)
        );
        gate.set_writes_allowed(dir.path()).expect("set");
        // A fresh gate on the same file models a restart.
        let reopened = TrustGate { path: gate.path };
        let key = TrustGate::workspace_key(dir.path());
        assert!(reopened.load().workspace[&key].writes_allowed);
    }

    #[test]
    fn pinning_without_manifests_writes_nothing() {
        let (dir, gate) = gate_and_workspace();
        let pinned = gate.pin(dir.path()).expect("pin");
        assert_eq!(pinned.len(), 0);
        assert!(!gate.path.exists(), "empty store must not be created");
    }
}
