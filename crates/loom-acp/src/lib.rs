//! `loom-acp`: the loom engine as an [Agent Client Protocol](https://agentclientprotocol.com)
//! (ACP) server, so editors (Zed and every other ACP client) can drive
//! loom as an external agent.
//!
//! Scope (roadmap item 37):
//!
//! * One `loom-acp` process serves one editor connection over the ACP
//!   stdio transport — newline-delimited JSON-RPC 2.0 on stdin/stdout.
//!   The spec forbids anything but valid ACP messages on stdout, so
//!   this crate never prints; `main` initializes `tracing` on stderr
//!   only.
//! * One engine per ACP session: `session/new` opens an
//!   [`Engine`] on the session's `cwd` and keeps one kernel
//!   [`Session`] per ACP session id and one [`TaskControl`] per active prompt.
//! * `session/prompt` runs one task to completion, forwarding engine
//!   events as `session/update` notifications in emission order; the
//!   task outcome maps onto the ACP [`StopReason`].
//! * `session/cancel` maps to [`TaskControl::cancel`] — hard stop, the
//!   kernel session survives, and the in-flight `session/prompt`
//!   resolves with `StopReason::Cancelled` as the spec requires.
//! * A policy `Ask` verdict bridges to the client's
//!   `session/request_permission` (one `execute`-kind allow option,
//!   two rejects). Any transport failure, a client
//!   `RequestPermissionOutcome::Cancelled`, or a rejected option fails
//!   closed (deny) — which is exactly the engine's own behavior with
//!   no ask resolver attached.
//!
//! `session/load` is served: the ACP session id *is* the kernel
//! session id in string form (UUIDv7), and loading rehydrates the
//! durable transcript from the shared database — the same id resumes
//! across connections and process restarts. The client's forwarded
//! `mcp_servers` in `session/new` attach only under the explicit
//! operator flag `--allow-client-mcp` (stdio servers, eager tools,
//! no lockfile writes; without the flag they are ignored with one
//! warning). Still deliberately absent: `set_model` /
//! `set_session_mode` (the default router answers with
//! method-not-found).
//!
//! The engine is reached through `loom-core` only (ADR-021). The
//! tests drive this whole server in-process over an
//! [`agent_client_protocol::Channel`] pair with a scripted provider
//! (the documented offline seam, `EngineOverrides::provider`), so wire
//! behavior — notification order, stop reasons, the permission bridge
//! — is asserted without a network or an editor.
//!
//! [`Engine`]: loom_core::Engine
//! [`Session`]: loom_core::kernel::Session
//! [`TaskControl`]: loom_core::kernel::agent::TaskControl
//! [`TaskControl::cancel`]: loom_core::kernel::agent::TaskControl::cancel
//! [`StopReason`]: agent_client_protocol::v1::schema::StopReason

#[cfg(test)]
mod tests;

mod mcp;
mod options;
mod permission;
mod server;
mod session;
mod updates;

pub use options::ServerOptions;
pub use server::serve;
pub use session::AcpState;
