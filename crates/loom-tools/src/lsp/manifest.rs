//! `KNOWN_SERVERS` table + `.loom/lsp.toml` manifest (docs/lsp-design.md §1).
//!
//! Breadth comes from data, not per-language code: a generic client plus
//! a table of well-known servers (the nvim-lspconfig lesson). The
//! manifest overrides or extends the table with operator intent — the
//! same trust position as `.loom/commands.toml`.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

/// Relative manifest path, mirroring `.loom/commands.toml`.
pub const MANIFEST_RELATIVE_PATH: &str = ".loom/lsp.toml";

/// A known-server table entry: how to spawn it and when it applies.
#[derive(Debug, Clone)]
pub struct KnownServer {
    /// Server name (manifest key + menu label).
    pub name: &'static str,
    /// Default spawn argv (overridable via the manifest).
    pub command: &'static [&'static str],
    /// File suffixes (lowercase, dot included) this server owns.
    pub suffixes: &'static [&'static str],
    /// Marker files whose presence in the workspace root suggests the
    /// language is actually in use (auto-attach heuristic). Empty means
    /// the language has no canonical marker; the server then attaches
    /// when a file with one of its suffixes exists (bounded scan).
    pub markers: &'static [&'static str],
    /// Human language label for the menu.
    pub language: &'static str,
}

/// The built-in server table. Grows as data, never code.
pub const KNOWN_SERVERS: &[KnownServer] = &[
    KnownServer {
        name: "rust_analyzer",
        command: &["rust-analyzer"],
        suffixes: &[".rs"],
        markers: &["Cargo.toml"],
        language: "Rust",
    },
    KnownServer {
        name: "gopls",
        command: &["gopls"],
        suffixes: &[".go"],
        markers: &["go.mod"],
        language: "Go",
    },
    KnownServer {
        name: "clangd",
        command: &["clangd"],
        suffixes: &[".c", ".h", ".cpp", ".cc", ".hpp"],
        markers: &["compile_commands.json", "CMakeLists.txt", "Makefile"],
        language: "C/C++",
    },
    KnownServer {
        name: "pyright",
        command: &["pyright-langserver", "--stdio"],
        suffixes: &[".py"],
        markers: &["pyproject.toml", "requirements.txt", "setup.py"],
        language: "Python",
    },
    KnownServer {
        name: "typescript_language_server",
        command: &["typescript-language-server", "--stdio"],
        suffixes: &[".ts", ".tsx", ".js", ".jsx"],
        markers: &["tsconfig.json", "package.json"],
        language: "TypeScript/JavaScript",
    },
    KnownServer {
        name: "zls",
        command: &["zls"],
        suffixes: &[".zig"],
        markers: &["build.zig"],
        language: "Zig",
    },
    KnownServer {
        name: "lua_language_server",
        command: &["lua-language-server"],
        suffixes: &[".lua"],
        markers: &[".luarc.json"],
        language: "Lua",
    },
    KnownServer {
        name: "bash_language_server",
        command: &["bash-language-server", "start"],
        suffixes: &[".sh", ".bash", ".bashrc"],
        markers: &[],
        language: "Shell",
    },
    KnownServer {
        name: "shell_lsp",
        command: &["shell-lsp"],
        suffixes: &[".zsh", ".zshrc"],
        markers: &[],
        language: "Zsh",
    },
];

/// One `[servers.<name>]` manifest entry.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct LspServerDecl {
    /// Spawn argv; required in the manifest (the table supplies defaults).
    pub command: Vec<String>,
    /// Extra environment variables for the child (explicit allowlist).
    pub env: BTreeMap<String, String>,
    /// Allow network access for the server process (default false).
    #[serde(default)]
    pub network: bool,
    /// Settings served to this server's `workspace/configuration`
    /// requests (E2). Free-form table the server interprets; absent
    /// means `null` (server falls back to its own defaults).
    #[serde(default)]
    pub settings: Option<serde_json::Value>,
    /// File suffixes this server owns (e.g. `[".mojo"]`). A name that
    /// matches a [`KNOWN_SERVERS`] entry inherits the table suffixes
    /// when this is empty; a manifest-only server needs these to be
    /// reachable by the path-based tools.
    pub suffixes: Vec<String>,
}

/// The parsed `.loom/lsp.toml`.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct LspManifest {
    /// Explicit server declarations; overrides the table by name.
    pub servers: BTreeMap<String, LspServerDecl>,
}

impl LspManifest {
    /// Parses manifest text. Invalid content is an error — attach fails
    /// closed rather than silently running local-only (MCP semantics).
    ///
    /// # Errors
    ///
    /// Fails when the text is not valid TOML or does not match the
    /// manifest schema.
    pub fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| format!("invalid {MANIFEST_RELATIVE_PATH}: {e}"))
    }
}

/// Loads the manifest from `root/.loom/lsp.toml` if present.
///
/// # Errors
///
/// Fails when the manifest cannot be read or fails to parse.
pub fn manifest_for(root: &Path) -> Result<Option<LspManifest>, String> {
    let path = root.join(MANIFEST_RELATIVE_PATH);
    match std::fs::read_to_string(&path) {
        Ok(text) => LspManifest::parse(&text).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {MANIFEST_RELATIVE_PATH}: {e}")),
    }
}

/// True when `argv[0]` resolves on PATH to something executable.
/// Servers that are not installed are skipped with a warning, never
/// fatal (LSP is an accelerator).
#[must_use]
pub fn binary_available(argv: &[String]) -> bool {
    let Some(program) = argv.first() else {
        return false;
    };
    if program.contains('/') {
        return executable(Path::new(program));
    }
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    path.split(':')
        .any(|dir| executable(&Path::new(dir).join(program)))
}

/// A file that can actually be run: a non-executable document that
/// happens to share the server's name must not count as installed.
fn executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// A resolved candidate after table + manifest merge.
#[derive(Debug, Clone)]
pub struct ServerCandidate {
    /// Table entry when this came from `KNOWN_SERVERS`, else None.
    pub known: Option<&'static KnownServer>,
    /// Final name (manifest key or table name).
    pub name: String,
    /// Final argv (manifest command or table default).
    pub command: Vec<String>,
    /// File suffixes this server owns (normalized: lowercase, dot first).
    pub suffixes: Vec<String>,
    /// Environment overrides.
    pub env: BTreeMap<String, String>,
    /// Network grant.
    pub network: bool,
}

/// Merges the `KNOWN_SERVERS` table with the manifest and the workspace's
/// language markers into the attach candidate list.
///
/// Rules (docs/lsp-design.md §1): manifest entries always attach when
/// their binary exists (explicit operator intent); table entries
/// auto-attach only when the binary is on PATH and either a marker file
/// is present or — for server entries without markers — a file with one
/// of the server's suffixes exists nearby. A table entry overridden by
/// the manifest is replaced.
pub fn candidates_for(root: &Path, manifest: &LspManifest) -> Vec<ServerCandidate> {
    let mut out: Vec<ServerCandidate> = Vec::new();
    // Manifest declarations win over the table.
    for (name, decl) in &manifest.servers {
        if decl.command.is_empty() {
            tracing::warn!(server = %name, "lsp manifest entry has empty command; skipping");
            continue;
        }
        let known = KNOWN_SERVERS.iter().find(|s| s.name == *name);
        let suffixes = if decl.suffixes.is_empty() {
            known.map_or_else(Vec::new, |s| {
                s.suffixes.iter().map(|s| (*s).to_owned()).collect()
            })
        } else {
            decl.suffixes
                .iter()
                .filter_map(|raw| normalize_suffix(raw))
                .collect()
        };
        out.push(ServerCandidate {
            known,
            name: name.clone(),
            command: decl.command.clone(),
            suffixes,
            env: decl.env.clone(),
            network: decl.network,
        });
    }
    for server in KNOWN_SERVERS {
        if out.iter().any(|c| c.name == server.name) {
            continue; // overridden by manifest
        }
        if !table_server_applies(server, root) {
            continue;
        }
        let command = server
            .command
            .iter()
            .copied()
            .map(str::to_owned)
            .collect::<Vec<String>>();
        if !binary_available(&command) {
            tracing::warn!(
                server = server.name,
                "language detected but server binary not on PATH; skipping"
            );
            continue;
        }
        out.push(ServerCandidate {
            known: Some(server),
            name: server.name.to_string(),
            command,
            suffixes: server.suffixes.iter().map(|s| (*s).to_owned()).collect(),
            env: BTreeMap::new(),
            network: false,
        });
    }
    out
}

/// Depth and entry caps for the suffix discovery scan: deep enough for
/// `scripts/`, `.github/scripts/` and dotfile trees, cheap enough to run
/// at every attach.
const DISCOVERY_MAX_DEPTH: usize = 4;
const DISCOVERY_MAX_ENTRIES: usize = 4_000;
const DISCOVERY_SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", ".venv", ".cache"];

/// Whether a table entry applies to this workspace: a marker file exists,
/// or (markerless entries only) a file with one of its suffixes exists.
fn table_server_applies(server: &KnownServer, root: &Path) -> bool {
    if server.markers.iter().any(|m| root.join(m).exists()) {
        return true;
    }
    server.markers.is_empty() && has_suffix_file(root, server.suffixes)
}

/// Bounded walk for a file whose name ends with one of `suffixes`.
/// Dependency/build trees are skipped; hidden directories are scanned so
/// dotfile repos work.
fn has_suffix_file(root: &Path, suffixes: &[&str]) -> bool {
    fn walk(dir: &Path, depth: usize, budget: &mut usize, suffixes: &[&str]) -> bool {
        if depth == 0 || *budget == 0 {
            return false;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            if *budget == 0 {
                return false;
            }
            *budget -= 1;
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if kind.is_dir() {
                if DISCOVERY_SKIP_DIRS.contains(&name.as_str()) {
                    continue;
                }
                if walk(&entry.path(), depth - 1, budget, suffixes) {
                    return true;
                }
            } else if suffixes.iter().any(|suffix| name.ends_with(suffix)) {
                return true;
            }
        }
        false
    }
    let mut budget = DISCOVERY_MAX_ENTRIES;
    walk(root, DISCOVERY_MAX_DEPTH, &mut budget, suffixes)
}

/// Normalizes one manifest suffix to lowercase with a leading dot.
fn normalize_suffix(raw: &str) -> Option<String> {
    let suffix = raw.trim().to_ascii_lowercase();
    if suffix.is_empty() {
        return None;
    }
    Some(if suffix.starts_with('.') {
        suffix
    } else {
        format!(".{suffix}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parses_server_decl() {
        let m = LspManifest::parse("[servers.rust_analyzer]\ncommand = [\"ra\"]\n").expect("valid");
        assert_eq!(m.servers["rust_analyzer"].command, vec!["ra"]);
        assert!(!m.servers["rust_analyzer"].network);
    }

    #[test]
    fn manifest_and_table_suffixes_merge() {
        let none = Path::new("/nonexistent-loom-lsp-root");
        let manifest = LspManifest::parse(
            "[servers.mojo]\ncommand = [\"mojo-lsp\"]\nsuffixes = [\"Mojo\", \".x\"]\n",
        )
        .expect("valid");
        let out = candidates_for(none, &manifest);
        assert_eq!(out.len(), 1);
        assert!(out[0].known.is_none());
        assert_eq!(out[0].suffixes, vec![".mojo", ".x"]);

        let manifest =
            LspManifest::parse("[servers.rust_analyzer]\ncommand = [\"ra\"]\n").expect("valid");
        let out = candidates_for(none, &manifest);
        assert_eq!(out[0].suffixes, vec![".rs"]);
    }

    #[cfg(unix)]
    #[test]
    fn executable_requires_the_exec_bit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("loom-lsp-exec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("server");
        std::fs::write(&file, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!executable(&file));
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(executable(&file));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_rejects_unknown_fields() {
        assert!(LspManifest::parse("[servers.x]\ncommand = [\"x\"]\nbogus = 1\n").is_err());
    }

    #[test]
    fn markerless_table_servers_discover_by_suffix() {
        let dir = std::env::temp_dir().join(format!("loom-lsp-discover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::write(dir.join("scripts/build.sh"), "echo hi\n").unwrap();
        let table = |name: &str| {
            KNOWN_SERVERS
                .iter()
                .find(|s| s.name == name)
                .expect("table")
        };

        assert!(table_server_applies(table("bash_language_server"), &dir));
        assert!(!table_server_applies(table("shell_lsp"), &dir));
        assert!(!table_server_applies(table("rust_analyzer"), &dir));

        std::fs::create_dir_all(dir.join(".config/zsh")).unwrap();
        std::fs::write(dir.join(".config/zsh/prompt.zsh"), "PS1='%# '\n").unwrap();
        assert!(table_server_applies(table("shell_lsp"), &dir));

        let skip_root =
            std::env::temp_dir().join(format!("loom-lsp-discover-skip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&skip_root);
        std::fs::create_dir_all(skip_root.join("target")).unwrap();
        std::fs::write(skip_root.join("target/generated.sh"), "echo hi\n").unwrap();
        assert!(!has_suffix_file(&skip_root, &[".sh"]));

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&skip_root);
    }

    #[test]
    fn table_entries_have_unique_names() {
        let mut names: Vec<&str> = KNOWN_SERVERS.iter().map(|s| s.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len());
    }
}
