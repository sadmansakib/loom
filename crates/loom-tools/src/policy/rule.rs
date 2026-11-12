use super::scrub::matches_scrub;
use super::verdict::PolicyError;

/// The longest wall-time cap a manifest rule may declare. A policy
/// timeout exists to bound a runaway command, not to license
/// hour-long builds; the engine's turn budget stays the outer bound.
const MAX_TIMEOUT_SECS: u64 = 3_600;

/// The largest per-stream window a manifest rule may keep. The cap
/// protects the context budget: a command whose output is worth more
/// than this belongs in the artifact spill, not the transcript.
const MAX_STREAM_WINDOW_BYTES: usize = 1024 * 1024;

/// One policy rule: escalation grants and limits for a program.
///
/// Rules no longer gate *whether* a program runs — every bare name may
/// run inside the OS sandbox. A rule is the escalation layer: what a
/// program may do beyond the boxed default (network, extra writable
/// paths, unsandboxed execution) and the limits that always apply
/// (timeout, output window, env scrubbing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRule {
    /// The bare executable name this rule escalates.
    pub program: String,
    /// Wall-time cap in seconds; `None` leaves the engine's per-turn
    /// timeout as the only bound.
    pub timeout_secs: Option<u64>,
    /// Per-stream head/tail window in bytes; `None` keeps the shipped
    /// constant.
    pub max_output_bytes: Option<usize>,
    /// Extra environment names scrubbed from the child, beyond the
    /// credential floor.
    pub env_scrub: Vec<String>,
    /// Credential-shaped environment names this program is explicitly
    /// allowed to receive, exempt from the floor and suffix patterns.
    /// The auditable opt-in for tools that legitimately need a
    /// token — for example `gh` with `GH_TOKEN`.
    pub env_allow: Vec<String>,
    /// Grant network access inside the sandbox for this program.
    pub network: bool,
    /// Run this program outside the OS sandbox entirely — the loudest
    /// possible grant, for operators who mean it.
    pub sandbox: bool,
    /// Extra writable directories (absolute paths) beyond the
    /// workspace/temp/cargo-home set.
    pub extra_writable: Vec<String>,
}

impl CommandRule {
    /// A rule escalating `program` with shipped-default limits.
    #[must_use]
    pub(super) fn bare(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            timeout_secs: None,
            max_output_bytes: None,
            env_scrub: Vec::new(),
            env_allow: Vec::new(),
            network: false,
            sandbox: true,
            extra_writable: Vec::new(),
        }
    }
}

/// Rejects a rule that could not have come from a careful operator:
/// path-qualified or empty program names, out-of-range caps, or scrub
/// names that are not environment variable names.
pub(super) fn validate(rule: &CommandRule, path: &str) -> Result<(), PolicyError> {
    if !bare_name(&rule.program) {
        return Err(PolicyError::Invalid {
            path: path.to_owned(),
            message: format!(
                "program \"{}\" must be a bare executable name",
                rule.program
            ),
        });
    }
    if let Some(secs) = rule.timeout_secs
        && (secs == 0 || secs > MAX_TIMEOUT_SECS)
    {
        return Err(PolicyError::Invalid {
            path: path.to_owned(),
            message: format!(
                "timeout_secs {secs} for \"{}\" must be 1..={MAX_TIMEOUT_SECS}",
                rule.program
            ),
        });
    }
    if let Some(bytes) = rule.max_output_bytes
        && (bytes == 0 || bytes > MAX_STREAM_WINDOW_BYTES)
    {
        return Err(PolicyError::Invalid {
            path: path.to_owned(),
            message: format!(
                "max_output_bytes {bytes} for \"{}\" must be 1..={MAX_STREAM_WINDOW_BYTES}",
                rule.program
            ),
        });
    }
    for name in &rule.env_scrub {
        if name.is_empty() || name.contains('=') {
            return Err(PolicyError::Invalid {
                path: path.to_owned(),
                message: format!("env_scrub entry \"{name}\" is not an environment variable name"),
            });
        }
    }
    for name in &rule.env_allow {
        // An allow entry that is not itself a credential shape is a
        // no-op with a misleading smell; reject it so the manifest
        // stays honest.
        if name.is_empty() || name.contains('=') || !matches_scrub(name) {
            return Err(PolicyError::Invalid {
                path: path.to_owned(),
                message: format!(
                    "env_allow entry \"{name}\" is not a credential-shaped environment variable name"
                ),
            });
        }
    }
    for dir in &rule.extra_writable {
        let path_buf = std::path::Path::new(dir);
        if !path_buf.is_absolute() {
            return Err(PolicyError::Invalid {
                path: path.to_owned(),
                message: format!("extra_writable entry \"{dir}\" must be an absolute path"),
            });
        }
    }
    Ok(())
}

/// Whether `program` names an executable the way a careful operator
/// would: non-empty, no path separators, and not a directory reference.
/// Path-qualified names would make the allowlist depend on where the
/// binary lives, so they are refused at parse time.
pub(super) fn bare_name(program: &str) -> bool {
    !(program.is_empty()
        || program.contains('/')
        || program.contains('\\')
        || program == "."
        || program == "..")
}
