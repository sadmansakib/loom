//! Built-in deterministic tools for Loom: `read_file`, `list_dir`,
//! `search`, `write_file`, `edit_file`, `run_command`, `artifact_read`.
//!
//! Every tool is workspace-confined (§9.1) and runs blocking work on
//! `spawn_blocking` (§12). `read_file`/`list_dir`/`search`/`artifact_read`
//! are read-only; `write_file`/`edit_file`/`run_command` mutate and so force
//! the tool engine's sequential mode (§10). `.gitignore` participates only as
//! a *discovery* filter in [`SearchTool`] — never as authorization (§9.2).

mod artifact_read;
mod budget;
mod confinement;
mod edit_file;
mod evidence;
mod exec;
pub mod fastpath;
mod fusion;
pub mod git;
mod list_dir;
pub mod lsp;
pub mod mcp;
pub mod policy;
pub mod read_file;
mod reduce;
mod run_command;
pub mod sandbox;
mod schema;
mod search;
pub mod skills;
pub mod symbols;
pub mod syntax;
mod text;
mod verify;
mod verify_runner;
mod window;
mod write_file;

pub use artifact_read::ArtifactReadTool;
pub use budget::ToolBudgets;
pub use edit_file::EditFileTool;
pub use git::{GitDiffTool, GitLogTool, GitStatusTool};
pub use list_dir::ListDirTool;
pub use lsp::{
    KNOWN_SERVERS, LspAttachment, LspManifest, LspServer, LspServerDecl, MANIFEST_RELATIVE_PATH,
};
pub use mcp::{McpError, McpManifest, McpServerDecl, McpToolPolicy};
pub use policy::{AskResolver, CommandPolicy, CommandRule, PolicyError, PolicyVerdict};
pub use read_file::ReadFileTool;
pub use run_command::RunCommandTool;
pub use search::SearchTool;
pub use skills::LoadSkillTool;
pub use symbols::{ReplaceSymbolTool, SymbolsOverviewTool};
pub use verify::VerifyTool;
pub use write_file::WriteFileTool;
