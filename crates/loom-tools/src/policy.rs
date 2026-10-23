//! The command execution policy: what `run_command` may execute, and
//! under which limits (roadmap: command execution policy).
//!
//! A policy is *data*: an ordered rule set keyed by bare program name.
//! The shipped default allows exactly the Rust build driver (`cargo`)
//! with no extra limits — Iteration 1's shape, unchanged for existing
//! workspaces. A workspace extends it with a `.loom/commands.toml`
//! manifest:
//!
//! ```toml
//! # Extra programs run_command may execute, beyond cargo.
//! [[command]]
//! program = "git"
//! timeout_secs = 60
//!
//! [[command]]
//! program = "ripgrep"
//! max_output_bytes = 131072
//! env_scrub = ["AWS_SECRET_ACCESS_KEY"]
//! ```
//!
//! The verdict layer (iteration 4) sits in front of the rules and is
//! optional in every workspace — omit both sections and the behavior is
//! byte-identical with the policy that preceded it:
//!
//! ```toml
//! # Programs that never run, even inside the sandbox.
//! [[deny]]
//! program = "curl"
//!
//! [policy]
//! # Verdict for bare programs with no rule and no deny entry:
//! # "allow" (shipped default: sandboxed run), "ask" (operator
//! # approval; fails closed without an interactive frontend),
//! # or "deny" (strict: only ruled programs run).
//! unknown = "deny"
//! ```
//!
//! A program with a rule is always admitted (the rule *is* the
//! operator's explicit consent); a deny entry for a ruled program is a
//! manifest error, so neither silently shadows the other.
//!
//! Each rule carries the three per-command limits:
//!
//! * `timeout_secs` — a wall-time cap. On expiry the child's whole
//!   process group is killed (the bounded-execution drop guard) and the
//!   call fails as an execution error the model can see and react to.
//!   Absent means uncapped: the engine's per-turn timeout remains the
//!   only bound, exactly as before.
//! * `max_output_bytes` — the per-stream head/tail window kept from
//!   stdout and stderr. Absent keeps the shipped constant.
//! * `env_scrub` — extra environment variable names removed from the
//!   child, on top of the credential floor below.
//!
//! The credential floor is non-negotiable: provider API keys and other
//! Loom credentials are scrubbed from every spawned child regardless of
//! any manifest, so a workspace process can never read them. A manifest
//! may add names to the scrub list; it can never remove the floor.
//!
//! Audit: every executed command is recorded as a structured
//! [`CommandEffect`](loom_kernel::tool::CommandEffect) (program, args,
//! exit status) in the session transcript, and a policy rejection or
//! timeout is recorded as the call's error status — so the durable
//! session is the audit log, with nothing to reconcile against.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Environment variables removed from every spawned child, regardless
/// of manifest: Loom's own credentials never leak into a workspace
/// process.
pub(crate) const SCRUB_FLOOR: &[&str] = &[
    "DEEPSEEK_API_KEY",
    "OPENROUTER_API_KEY",
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
];

/// The longest wall-time cap a manifest rule may declare. A policy
/// timeout exists to bound a runaway command, not to license
/// hour-long builds; the engine's turn budget stays the outer bound.
const MAX_TIMEOUT_SECS: u64 = 3_600;

/// The largest per-stream window a manifest rule may keep. The cap
/// protects the context budget: a command whose output is worth more
/// than this belongs in the artifact spill, not the transcript.
const MAX_STREAM_WINDOW_BYTES: usize = 1024 * 1024;

/// Where the workspace's command manifest lives, relative to the
/// workspace root.
pub(crate) const MANIFEST_RELATIVE_PATH: &str = ".loom/commands.toml";

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

/// Operator-approval hook for [`PolicyVerdict::Ask`]: returns `true`
/// when execution is approved. Interactive frontends supply one; the
/// engine default is none, so `ask` fails closed.
pub type AskResolver =
    std::sync::Arc<dyn Fn(&str) -> futures::future::BoxFuture<'static, bool> + Send + Sync>;

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
    fn bare(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            timeout_secs: None,
            max_output_bytes: None,
            env_scrub: Vec::new(),
            network: false,
            sandbox: true,
            extra_writable: Vec::new(),
        }
    }
}

/// What `run_command` may execute: an ordered rule set, looked up by
/// bare program name. Data, never a branch on caller identity.
///
/// The verdict layer (iteration 4) sits in front of the rules: a
/// program named in the deny list never runs, and a program with no
/// rule and no deny entry runs only when the unknown-verdict is
/// `allow` (the shipped default — byte-identical with the policy that
/// preceded the layer).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandPolicy {
    rules: Vec<CommandRule>,
    unknown: PolicyVerdict,
    denied: Vec<String>,
}

impl CommandPolicy {
    /// The shipped policy: `cargo` only, shipped-default limits.
    #[must_use]
    pub fn cargo_only() -> Self {
        Self {
            rules: vec![CommandRule::bare("cargo")],
            unknown: PolicyVerdict::Allow,
            denied: Vec::new(),
        }
    }

    /// A policy admitting exactly these programs with shipped-default
    /// limits.
    #[must_use]
    pub fn allowing<I, S>(programs: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            rules: programs.into_iter().map(CommandRule::bare).collect(),
            unknown: PolicyVerdict::Allow,
            denied: Vec::new(),
        }
    }

    /// The verdict for `program`: a rule admits it, the deny list
    /// refuses it, and everything else falls through to the
    /// unknown-program verdict.
    #[must_use]
    pub fn verdict(&self, program: &str) -> PolicyVerdict {
        if self.rules.iter().any(|rule| rule.program == program) {
            PolicyVerdict::Allow
        } else if self.denied.iter().any(|name| name == program) {
            PolicyVerdict::Deny
        } else {
            self.unknown
        }
    }

    /// The denied program names, in manifest order.
    #[must_use]
    pub fn denied_names(&self) -> Vec<&str> {
        self.denied.iter().map(String::as_str).collect()
    }

    /// The verdict applying to programs with neither a rule nor a deny
    /// entry.
    #[must_use]
    pub fn unknown_verdict(&self) -> PolicyVerdict {
        self.unknown
    }

    /// The rule admitting `program`, if any.
    #[must_use]
    pub fn rule(&self, program: &str) -> Option<&CommandRule> {
        self.rules.iter().find(|rule| rule.program == program)
    }

    /// The admitted program names, in rule order.
    #[must_use]
    pub fn allowed_names(&self) -> Vec<&str> {
        self.rules
            .iter()
            .map(|rule| rule.program.as_str())
            .collect()
    }

    /// Parses a manifest, extending the shipped `cargo` rule. The
    /// manifest may only add programs; re-declaring one (including
    /// `cargo`) is an error, so a rule is never silently shadowed.
    ///
    /// Exposed for tests and callers that already hold the manifest
    /// text; [`Self::from_manifest`] is the filesystem entry point.
    pub fn parse(text: &str) -> Result<Self, PolicyError> {
        let path = MANIFEST_RELATIVE_PATH.to_owned();
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Manifest {
            #[serde(default)]
            command: Vec<ManifestCommand>,
            #[serde(default)]
            deny: Vec<ManifestDeny>,
            policy: Option<ManifestPolicy>,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ManifestDeny {
            program: String,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ManifestPolicy {
            unknown: PolicyVerdict,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields, rename_all = "snake_case")]
        struct ManifestCommand {
            program: String,
            #[serde(default)]
            timeout_secs: Option<u64>,
            #[serde(default)]
            max_output_bytes: Option<usize>,
            #[serde(default)]
            env_scrub: Vec<String>,
            #[serde(default)]
            network: bool,
            #[serde(default = "default_sandbox")]
            sandbox: bool,
            #[serde(default)]
            extra_writable: Vec<String>,
        }
        let manifest: Manifest = toml::from_str(text).map_err(|error| PolicyError::Invalid {
            path: path.clone(),
            message: error.to_string(),
        })?;
        fn default_sandbox() -> bool {
            true
        }
        let mut policy = Self::cargo_only();
        if let Some(section) = manifest.policy {
            policy.unknown = section.unknown;
        }
        for entry in manifest.deny {
            if !bare_name(&entry.program) {
                return Err(PolicyError::Invalid {
                    path: path.clone(),
                    message: format!(
                        "denied program \"{}\" must be a bare executable name",
                        entry.program
                    ),
                });
            }
            if policy.rule(&entry.program).is_some() {
                return Err(PolicyError::Invalid {
                    path: path.clone(),
                    message: format!(
                        "program \"{}\" is both denied and ruled; remove one",
                        entry.program
                    ),
                });
            }
            if policy.denied.contains(&entry.program) {
                return Err(PolicyError::Invalid {
                    path: path.clone(),
                    message: format!("program \"{}\" is denied twice", entry.program),
                });
            }
            policy.denied.push(entry.program);
        }
        for entry in manifest.command {
            let rule = CommandRule {
                program: entry.program,
                timeout_secs: entry.timeout_secs,
                max_output_bytes: entry.max_output_bytes,
                env_scrub: entry.env_scrub,
                network: entry.network,
                sandbox: entry.sandbox,
                extra_writable: entry.extra_writable,
            };
            validate(&rule, &path)?;
            if policy.rule(&rule.program).is_some() {
                return Err(PolicyError::Invalid {
                    path,
                    message: format!("program \"{}\" is declared twice", rule.program),
                });
            }
            if policy.denied.contains(&rule.program) {
                return Err(PolicyError::Invalid {
                    path,
                    message: format!(
                        "program \"{}\" is both denied and ruled; remove one",
                        rule.program
                    ),
                });
            }
            policy.rules.push(rule);
        }
        Ok(policy)
    }

    /// Loads the workspace's policy: the `.loom/commands.toml` manifest
    /// when present, otherwise the shipped `cargo`-only default. A
    /// manifest that exists but is unreadable or invalid is an error —
    /// a policy that silently degrades to default would widen
    /// execution behind the operator's back.
    pub fn from_manifest(root: &Path) -> Result<Self, PolicyError> {
        let path = root.join(MANIFEST_RELATIVE_PATH);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::cargo_only());
            }
            Err(source) => {
                return Err(PolicyError::Io {
                    path: path.display().to_string(),
                    source,
                });
            }
        };
        Self::parse(&text).map_err(|error| match error {
            PolicyError::Invalid { message, .. } => PolicyError::Invalid {
                path: path.display().to_string(),
                message,
            },
            other => other,
        })
    }
}

/// Rejects a rule that could not have come from a careful operator:
/// path-qualified or empty program names, out-of-range caps, or scrub
/// names that are not environment variable names.
fn validate(rule: &CommandRule, path: &str) -> Result<(), PolicyError> {
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
fn bare_name(program: &str) -> bool {
    !(program.is_empty()
        || program.contains('/')
        || program.contains('\\')
        || program == "."
        || program == "..")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_policy_is_cargo_only_with_default_limits() {
        let policy = CommandPolicy::cargo_only();
        assert_eq!(policy.allowed_names(), vec!["cargo"]);
        let rule = policy.rule("cargo").expect("cargo admitted");
        assert_eq!(rule.timeout_secs, None);
        assert_eq!(rule.max_output_bytes, None);
        assert!(rule.env_scrub.is_empty());
        assert!(policy.rule("rm").is_none());
    }

    #[test]
    fn manifest_extends_cargo_and_applies_limits() {
        let policy = CommandPolicy::parse(
            "[[command]]\nprogram = \"git\"\ntimeout_secs = 60\n\n\
             [[command]]\nprogram = \"ripgrep\"\nmax_output_bytes = 131072\n\
             env_scrub = [\"AWS_SECRET_ACCESS_KEY\"]\n",
        )
        .expect("valid manifest");
        assert_eq!(policy.allowed_names(), vec!["cargo", "git", "ripgrep"]);
        let git = policy.rule("git").expect("git admitted");
        assert_eq!(git.timeout_secs, Some(60));
        let ripgrep = policy.rule("ripgrep").expect("ripgrep admitted");
        assert_eq!(ripgrep.max_output_bytes, Some(131072));
        assert_eq!(ripgrep.env_scrub, vec!["AWS_SECRET_ACCESS_KEY".to_owned()]);
    }

    #[test]
    fn redeclaring_a_program_is_an_error() {
        let error = CommandPolicy::parse("[[command]]\nprogram = \"cargo\"\n")
            .expect_err("cargo redeclared");
        assert!(error.to_string().contains("declared twice"), "{error}");
    }

    #[test]
    fn path_qualified_programs_are_rejected_at_parse_time() {
        let error = CommandPolicy::parse("[[command]]\nprogram = \"/bin/sh\"\n")
            .expect_err("path-qualified");
        assert!(error.to_string().contains("bare executable"), "{error}");
        let error = CommandPolicy::parse("[[command]]\nprogram = \"\"\n").expect_err("empty");
        assert!(error.to_string().contains("bare executable"), "{error}");
    }

    #[test]
    fn out_of_range_limits_are_rejected() {
        let error = CommandPolicy::parse("[[command]]\nprogram = \"git\"\ntimeout_secs = 0\n")
            .expect_err("zero timeout");
        assert!(error.to_string().contains("timeout_secs"), "{error}");
        let error = CommandPolicy::parse("[[command]]\nprogram = \"git\"\ntimeout_secs = 99999\n")
            .expect_err("timeout over max");
        assert!(error.to_string().contains("timeout_secs"), "{error}");
        let error =
            CommandPolicy::parse("[[command]]\nprogram = \"git\"\nmax_output_bytes = 9999999999\n")
                .expect_err("window over max");
        assert!(error.to_string().contains("max_output_bytes"), "{error}");
    }

    #[test]
    fn unknown_manifest_fields_are_rejected() {
        let error = CommandPolicy::parse("[[command]]\nprogram = \"git\"\nallow_network = true\n")
            .expect_err("unknown field");
        assert!(error.to_string().contains("unknown field"), "{error}");
    }

    #[test]
    fn a_missing_manifest_is_the_default_policy() {
        let dir = tempfile::tempdir().expect("tempdir");
        let policy = CommandPolicy::from_manifest(dir.path()).expect("default");
        assert_eq!(policy, CommandPolicy::cargo_only());
    }

    #[test]
    fn a_present_manifest_loads_from_the_workspace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest_dir = dir.path().join(".loom");
        std::fs::create_dir_all(&manifest_dir).expect("mkdir");
        std::fs::write(
            manifest_dir.join("commands.toml"),
            "[[command]]\nprogram = \"git\"\n",
        )
        .expect("write manifest");
        let policy = CommandPolicy::from_manifest(dir.path()).expect("loaded");
        assert_eq!(policy.allowed_names(), vec!["cargo", "git"]);
    }

    #[test]
    fn an_invalid_manifest_fails_loudly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manifest_dir = dir.path().join(".loom");
        std::fs::create_dir_all(&manifest_dir).expect("mkdir");
        std::fs::write(manifest_dir.join("commands.toml"), "not a manifest").expect("write");
        let error = CommandPolicy::from_manifest(dir.path()).expect_err("invalid");
        assert!(error.to_string().contains("invalid manifest"), "{error}");
    }

    #[test]
    fn the_scrub_floor_covers_provider_credentials() {
        assert!(SCRUB_FLOOR.contains(&"DEEPSEEK_API_KEY"));
        assert!(SCRUB_FLOOR.contains(&"OPENROUTER_API_KEY"));
    }

    #[test]
    fn the_shipped_policy_stays_allow_unknown() {
        let policy = CommandPolicy::cargo_only();
        assert_eq!(policy.unknown_verdict(), PolicyVerdict::Allow);
        assert!(policy.denied_names().is_empty());
        assert_eq!(policy.verdict("rm"), PolicyVerdict::Allow);
        assert_eq!(policy.verdict("cargo"), PolicyVerdict::Allow);
    }

    #[test]
    fn the_verdict_layer_parses_and_resolves() {
        let policy = CommandPolicy::parse(
            "[policy]\nunknown = \"deny\"\n[[deny]]\nprogram = \"curl\"\n\
             [[command]]\nprogram = \"git\"\n",
        )
        .expect("policy");
        assert_eq!(policy.verdict("git"), PolicyVerdict::Allow, "rules admit");
        assert_eq!(policy.verdict("curl"), PolicyVerdict::Deny, "deny list");
        assert_eq!(
            policy.verdict("python"),
            PolicyVerdict::Deny,
            "strict unknown"
        );
        assert_eq!(policy.denied_names(), vec!["curl"]);
    }

    #[test]
    fn unknown_ask_parses() {
        let policy = CommandPolicy::parse("[policy]\nunknown = \"ask\"\n").expect("policy");
        assert_eq!(policy.verdict("echo"), PolicyVerdict::Ask);
        assert_eq!(
            policy.verdict("cargo"),
            PolicyVerdict::Allow,
            "cargo stays ruled"
        );
    }

    #[test]
    fn deny_and_rule_conflicts_are_errors_in_both_orders() {
        // Deny first, rule after.
        let error =
            CommandPolicy::parse("[[deny]]\nprogram = \"git\"\n[[command]]\nprogram = \"git\"\n")
                .expect_err("conflict");
        assert!(
            error.to_string().contains("both denied and ruled"),
            "{error}"
        );
        // Rule first (cargo), deny after — same refusal.
        let error = CommandPolicy::parse("[[deny]]\nprogram = \"cargo\"\n")
            .expect_err("cargo is always ruled");
        assert!(
            error.to_string().contains("both denied and ruled"),
            "{error}"
        );
    }

    #[test]
    fn duplicate_deny_entries_are_errors() {
        let error =
            CommandPolicy::parse("[[deny]]\nprogram = \"curl\"\n[[deny]]\nprogram = \"curl\"\n")
                .expect_err("duplicate");
        assert!(error.to_string().contains("denied twice"), "{error}");
    }

    #[test]
    fn an_invalid_unknown_verdict_is_rejected() {
        let error =
            CommandPolicy::parse("[policy]\nunknown = \"maybe\"\n").expect_err("not a verdict");
        assert!(error.to_string().contains("unknown"), "{error}");
    }
}
