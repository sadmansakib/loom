use std::path::Path;

use serde::Deserialize;

use super::rule::{CommandRule, bare_name, validate};
use super::verdict::{PolicyError, PolicyVerdict};

mod verdict_argv;

/// Where the workspace's command manifest lives, relative to the
/// workspace root.
pub(super) const MANIFEST_RELATIVE_PATH: &str = ".loom/commands.toml";

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
    /// Set when the manifest was loaded *restrictively* because the
    /// operator has not pinned it in the trust store (review
    /// F-02/F-12): every escalation field was stripped. Diagnostic
    /// surface, not a branch callers are expected to inspect.
    restricted_manifest: bool,
}

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
    env_allow: Vec<String>,
    #[serde(default)]
    network: bool,
    #[serde(default = "default_sandbox")]
    sandbox: bool,
    #[serde(default)]
    extra_writable: Vec<String>,
}

fn default_sandbox() -> bool {
    true
}

/// Validates one manifest `[[command]]` rule and appends it; refuses
/// duplicate declarations and deny/rule conflicts.
fn insert_rule(
    policy: &mut CommandPolicy,
    path: &str,
    entry: ManifestCommand,
) -> Result<(), PolicyError> {
    let rule = CommandRule {
        program: entry.program,
        timeout_secs: entry.timeout_secs,
        max_output_bytes: entry.max_output_bytes,
        env_scrub: entry.env_scrub,
        env_allow: entry.env_allow,
        network: entry.network,
        sandbox: entry.sandbox,
        extra_writable: entry.extra_writable,
    };
    validate(&rule, path)?;
    if policy.rule(&rule.program).is_some() {
        return Err(PolicyError::Invalid {
            path: path.to_owned(),
            message: format!("program \"{}\" is declared twice", rule.program),
        });
    }
    if policy.denied.contains(&rule.program) {
        return Err(PolicyError::Invalid {
            path: path.to_owned(),
            message: format!(
                "program \"{}\" is both denied and ruled; remove one",
                rule.program
            ),
        });
    }
    policy.rules.push(rule);
    Ok(())
}

impl CommandPolicy {
    /// The shipped policy: `cargo` only, shipped-default limits.
    #[must_use]
    pub fn cargo_only() -> Self {
        Self {
            rules: vec![CommandRule::bare("cargo")],
            unknown: PolicyVerdict::Allow,
            denied: Vec::new(),
            restricted_manifest: false,
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
            restricted_manifest: false,
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
    ///
    /// # Errors
    /// Fails when the TOML is malformed, contains unknown fields, or
    /// re-declares a shipped rule (`PolicyError::Invalid` carries the
    /// reason); a rule may only add programs, never shadow one.
    pub fn parse(text: &str) -> Result<Self, PolicyError> {
        let path = MANIFEST_RELATIVE_PATH.to_owned();
        let manifest: Manifest = toml::from_str(text).map_err(|error| PolicyError::Invalid {
            path: path.clone(),
            message: error.to_string(),
        })?;
        let mut policy = Self::cargo_only();
        if let Some(section) = manifest.policy {
            policy.unknown = section.unknown;
        }
        for entry in manifest.deny {
            if !bare_name(&entry.program) {
                return Err(PolicyError::Invalid {
                    path,
                    message: format!(
                        "denied program \"{}\" must be a bare executable name",
                        entry.program
                    ),
                });
            }
            if policy.rule(&entry.program).is_some() {
                // The one pre-ruled program is the shipped `cargo`
                // default — and a strict workspace must be able to
                // lock the build driver out entirely, because
                // `build.rs` and proc-macros execute arbitrary code
                // even inside the sandbox (review F-10). Denying it
                // drops the shipped rule instead of erroring; a
                // manifest that then *re-rules* cargo is still
                // rejected by the command loop below.
                if entry.program != "cargo" {
                    return Err(PolicyError::Invalid {
                        path,
                        message: format!(
                            "program \"{}\" is both denied and ruled; remove one",
                            entry.program
                        ),
                    });
                }
                policy.rules.retain(|rule| rule.program != "cargo");
            }
            if policy.denied.contains(&entry.program) {
                return Err(PolicyError::Invalid {
                    path,
                    message: format!("program \"{}\" is denied twice", entry.program),
                });
            }
            policy.denied.push(entry.program);
        }
        for entry in manifest.command {
            insert_rule(&mut policy, &path, entry)?;
        }
        Ok(policy)
    }

    /// Loads the workspace's policy: the `.loom/commands.toml` manifest
    /// when present, otherwise the shipped `cargo`-only default. A
    /// manifest that exists but is unreadable or invalid is an error —
    /// a policy that silently degrades to default would widen
    /// execution behind the operator's back.
    ///
    /// A manifest the operator has not pinned in the trust store
    /// (review F-02/F-12: a cloned repo *ships* `.loom/commands.toml`)
    /// loads *restrictively*: rules survive with their limits and the
    /// deny list, but every escalation field — `sandbox = false`,
    /// `network = true`, `extra_writable`, `env_allow` — is stripped.
    /// `loom trust` pins the reviewed manifest and restores it.
    ///
    /// # Errors
    /// Fails when the manifest cannot be read from `root` (errors
    /// other than not-found) or when [`Self::parse`] rejects it.
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
        // PolicyError is #[non_exhaustive]: the wildcard arm is required.
        #[allow(clippy::wildcard_enum_match_arm)]
        let policy = Self::parse(&text).map_err(|error| match error {
            PolicyError::Invalid { message, .. } => PolicyError::Invalid {
                path: path.display().to_string(),
                message,
            },
            other => other,
        })?;
        match crate::trust::TrustGate::open().decide(root, MANIFEST_RELATIVE_PATH) {
            crate::trust::Trust::Trusted | crate::trust::Trust::Absent => Ok(policy),
            crate::trust::Trust::Untrusted => {
                tracing::warn!(
                    path = %path.display(),
                    "commands manifest not pinned in the trust store; \
                     loading without escalation fields (run `loom trust` to review and pin)"
                );
                Ok(policy.restricted())
            }
        }
    }

    /// The escalation-stripped form of this policy for an unpinned
    /// manifest (review F-02/F-12). Keeps: the program allowlist,
    /// `timeout_secs`, `max_output_bytes`, `env_scrub`, the deny
    /// list, and the unknown-verdict (all limit, never widen).
    /// Strips: `sandbox = false`, `network = true`, `extra_writable`,
    /// `env_allow` — the fields that grant anything.
    #[must_use]
    pub(crate) fn restricted(mut self) -> Self {
        for rule in &mut self.rules {
            rule.network = false;
            rule.sandbox = true;
            rule.extra_writable.clear();
            rule.env_allow.clear();
        }
        self.restricted_manifest = true;
        self
    }

    /// Whether this policy was loaded restrictively from an unpinned
    /// manifest (diagnostics; see [`Self::from_manifest`]).
    #[must_use]
    pub fn is_restricted(&self) -> bool {
        self.restricted_manifest
    }
}
