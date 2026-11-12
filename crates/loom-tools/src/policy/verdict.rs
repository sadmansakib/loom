use serde::{Deserialize, Serialize};

/// Why a command policy could not be loaded.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    /// The manifest could not be read.
    #[error("cannot read {path}: {source}")]
    Io {
        /// The manifest path as supplied.
        path: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// The manifest is not valid TOML or does not match the schema.
    #[error("invalid manifest {path}: {message}")]
    Invalid {
        /// The manifest path as supplied.
        path: String,
        /// What is wrong, in manifest terms.
        message: String,
    },
}

/// What the workspace policy says about executing a program that has
/// no escalation rule: run it inside the boxed sandbox default
/// ([`PolicyVerdict::Allow`]), refuse it outright
/// ([`PolicyVerdict::Deny`]), or pause for operator approval
/// ([`PolicyVerdict::Ask`] — requires an interactive frontend; without
/// one, execution fails closed).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyVerdict {
    /// Run inside the sandboxed default (or escalate when a rule
    /// exists). The shipped behavior for every bare program.
    #[default]
    Allow,
    /// Refuse to execute. The model sees a refusal naming the program.
    Deny,
    /// Requires operator approval before execution. Without an
    /// attached ask resolver (interactive frontend), this fails closed
    /// as a denial that names the missing frontend.
    Ask,
}

/// What the operator is asked to approve, in full (review F-13): the
/// program *and* its arguments, plus the escalation state this call
/// will run under. A title rendered from the program alone let `sh`
/// hide `-c 'curl … | sh'` behind a one-word approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskRequest {
    /// Bare executable name.
    pub program: String,
    /// The argv that will run, verbatim.
    pub args: Vec<String>,
    /// Whether this call runs inside the OS sandbox (`false` only
    /// when a rule escalates the program out of it).
    pub sandboxed: bool,
    /// Whether this call holds the network grant.
    pub network: bool,
}

impl AskRequest {
    /// The command line the operator is approving. Arguments that
    /// contain whitespace or quotes are wrapped in double quotes so
    /// the rendered line cannot hide an argument boundary.
    #[must_use]
    pub fn render(&self) -> String {
        let mut line = self.program.clone();
        for arg in &self.args {
            line.push(' ');
            if arg.is_empty()
                || arg.chars().any(char::is_whitespace)
                || arg.contains('\'')
                || arg.contains('"')
            {
                line.push_str(&format!("{arg:?}"));
            } else {
                line.push_str(arg);
            }
        }
        line
    }
}

/// Operator-approval hook for [`PolicyVerdict::Ask`]: returns `true`
/// when execution is approved. Interactive frontends supply one; the
/// engine default is none, so `ask` fails closed.
pub type AskResolver =
    std::sync::Arc<dyn Fn(AskRequest) -> futures::future::BoxFuture<'static, bool> + Send + Sync>;
