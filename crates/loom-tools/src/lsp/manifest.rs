//! KNOWN_SERVERS table + `.loom/lsp.toml` manifest (docs/lsp-design.md §1).
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
    /// language is actually in use (auto-attach heuristic).
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
        suffixes: &[".sh", ".bash"],
        markers: &[],
        language: "Shell",
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
    pub fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| format!("invalid {MANIFEST_RELATIVE_PATH}: {e}"))
    }
}

/// Loads the manifest from `root/.loom/lsp.toml` if present.
pub fn manifest_for(root: &Path) -> Result<Option<LspManifest>, String> {
    let path = root.join(MANIFEST_RELATIVE_PATH);
    match std::fs::read_to_string(&path) {
        Ok(text) => LspManifest::parse(&text).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {MANIFEST_RELATIVE_PATH}: {e}")),
    }
}

/// True when `argv[0]` resolves on PATH. Servers that are not installed
/// are skipped with a warning, never fatal (LSP is an accelerator).
pub fn binary_available(argv: &[String]) -> bool {
    let Some(program) = argv.first() else {
        return false;
    };
    if program.contains('/') {
        return Path::new(program).exists();
    }
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    path.split(':').any(|dir| {
        let candidate = Path::new(dir).join(program);
        candidate.is_file()
    })
}

/// A resolved candidate after table + manifest merge.
#[derive(Debug, Clone)]
pub struct ServerCandidate {
    /// Table entry when this came from KNOWN_SERVERS, else None.
    pub known: Option<&'static KnownServer>,
    /// Final name (manifest key or table name).
    pub name: String,
    /// Final argv (manifest command or table default).
    pub command: Vec<String>,
    /// Environment overrides.
    pub env: BTreeMap<String, String>,
    /// Network grant.
    pub network: bool,
}

/// Merges the KNOWN_SERVERS table with the manifest and the workspace's
/// language markers into the attach candidate list.
///
/// Rules (docs/lsp-design.md §1): manifest entries always attach when
/// their binary exists (explicit operator intent); table entries
/// auto-attach only when the binary is on PATH and a marker file is
/// present. A table entry overridden by the manifest is replaced.
pub fn candidates_for(root: &Path, manifest: &LspManifest) -> Vec<ServerCandidate> {
    let has_marker =
        |server: &KnownServer| -> bool { server.markers.iter().any(|m| root.join(m).exists()) };
    let mut out: Vec<ServerCandidate> = Vec::new();
    // Manifest declarations win over the table.
    for (name, decl) in &manifest.servers {
        if decl.command.is_empty() {
            tracing::warn!(server = %name, "lsp manifest entry has empty command; skipping");
            continue;
        }
        out.push(ServerCandidate {
            known: KNOWN_SERVERS.iter().find(|s| s.name == *name),
            name: name.clone(),
            command: decl.command.clone(),
            env: decl.env.clone(),
            network: decl.network,
        });
    }
    for server in KNOWN_SERVERS {
        if out.iter().any(|c| c.name == server.name) {
            continue; // overridden by manifest
        }
        if !has_marker(server) {
            continue;
        }
        let command = server
            .command
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<String>>();
        if !binary_available(&command) {
            tracing::warn!(
                server = server.name,
                "language markers present but server binary not on PATH; skipping"
            );
            continue;
        }
        out.push(ServerCandidate {
            known: Some(server),
            name: server.name.to_string(),
            command,
            env: BTreeMap::new(),
            network: false,
        });
    }
    out
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
    fn manifest_rejects_unknown_fields() {
        assert!(LspManifest::parse("[servers.x]\ncommand = [\"x\"]\nbogus = 1\n").is_err());
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
