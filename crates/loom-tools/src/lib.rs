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
mod fusion;
mod list_dir;
mod read_file;
mod run_command;
mod schema;
mod search;
mod text;
mod verify;
mod window;
mod write_file;

pub use artifact_read::ArtifactReadTool;
pub use edit_file::EditFileTool;
pub use list_dir::ListDirTool;
pub use read_file::ReadFileTool;
pub use run_command::RunCommandTool;
pub use search::SearchTool;
pub use verify::VerifyTool;
pub use write_file::WriteFileTool;
