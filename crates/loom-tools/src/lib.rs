//! Built-in deterministic tools for Loom: `read_file`, `list_dir`,
//! `search`.
//!
//! All three are read-only, workspace-confined (§9.1), and run their
//! blocking filesystem work on `spawn_blocking` (§12). `.gitignore`
//! participates only as a *discovery* filter in [`SearchTool`] — never
//! as authorization (§9.2).

mod confinement;
mod list_dir;
mod read_file;
mod search;
mod text;

pub use list_dir::ListDirTool;
pub use read_file::ReadFileTool;
pub use search::SearchTool;
