//! Built-in deterministic tools for Loom.
//!
//! Iteration 1 ships `read_file`, `list_dir`, and `search`, each implementing
//! `loom_core::tool::Tool`. Filesystem tools enforce the workspace-root
//! boundary (relative and absolute paths, `..` traversal, symlinks,
//! canonicalization failures) and treat `.gitignore` as a discovery filter
//! only, never an authorization mechanism.
