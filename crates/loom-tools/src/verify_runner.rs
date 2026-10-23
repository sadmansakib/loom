//! Optional containerized/remote `verify` runners (roadmap item 23).
//!
//! A workspace whose toolchain cannot live on the host — the wrong OS,
//! a pinned interpreter, a build farm — can declare a runner in
//! `.loom/verify.toml`. Verification then executes *through* that
//! runner instead of locally: the ecosystem command still comes from
//! the fixed table in [`crate::verify`] (never from model input), and
//! the output flows through the same [`crate::verify::render`]
//! evidence pipeline, so the evidence contract is unchanged — only the
//! execution substrate moves.
//!
//! ```toml
//! # .loom/verify.toml
//! [runner]
//! command = "docker run --rm -v {workspace}:/workspace -w /workspace rust:1.89 {program} {args}"
//! ```
//!
//! The command is an argv template, not a shell line: it is split on
//! whitespace, and the standalone tokens `{workspace}`, `{program}`,
//! and `{args}` expand to the host workspace path, the planned
//! program, and its arguments (`{args}` spreads to one argv token per
//! argument). No shell interpolation happens, ever. A remote runner is
//! the same shape: `ssh build-host cd {workspace} && …` cannot be
//! expressed without a shell, so remote hosts use a tiny wrapper
//! script as the runner program.
//!
//! Trust and confinement, stated honestly:
//!
//! - Declaring the runner is the operator's approval of that command,
//!   the same trust position `.loom/commands.toml` holds for program
//!   escalation.
//! - The runner executes **outside the OS sandbox** — a `docker` or
//!   `ssh` client cannot do its job through Seatbelt/bubblewrap. Every
//!   such run records a `verify.runner` notice saying exactly that,
//!   and the [`loom_kernel::tool::CommandEffect`] carries the full
//!   expanded argv for the trace.
//! - Fail closed: a present-but-invalid manifest makes `verify`
//!   refuse rather than silently fall back to local execution — the
//!   operator declared isolation, and quiet un-isolated runs would
//!   betray it. A missing manifest keeps today's byte-identical local
//!   behavior.
//!
//! Both verification paths share this module: the standalone `verify`
//! tool and the fused verify inside `write_file`/`edit_file`
//! ([`crate::fusion`]) read the same manifest at call time.

use std::path::Path;

use loom_kernel::tool::{ToolContext, ToolError, ToolNotice};

/// The manifest path, relative to the workspace root.
const MANIFEST: &str = ".loom/verify.toml";

/// The notice kind stamped on every runner-mediated verification.
pub(crate) const NOTICE_KIND: &str = "verify.runner";

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    runner: ManifestRunner,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestRunner {
    /// The argv template; see the module docs for the placeholder
    /// vocabulary.
    command: String,
}

/// What one runner-mediated run produced, plus the argv that ran.
pub(crate) struct RunnerOutcome {
    /// The runner's captured streams and exit status — the ecosystem
    /// command's output, as relayed by the runner.
    pub output: std::process::Output,
    /// A runner descendant held the pipes past the grace window.
    pub pipe_hold_truncated: bool,
    /// The runner program that actually executed.
    pub runner_program: String,
    /// The full expanded argv after the program token.
    pub runner_args: Vec<String>,
}

/// Reads the workspace's runner declaration, if any.
///
/// `Ok(None)` means no manifest: local verification, byte-identical to
/// today. Any present-but-unusable manifest is an error so callers can
/// fail closed.
pub(crate) fn runner_for(root: &Path) -> Result<Option<VerifyRunner>, String> {
    let path = root.join(MANIFEST);
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("cannot read {MANIFEST}: {error}"))?;
    let manifest: Manifest =
        toml::from_str(&text).map_err(|error| format!("invalid {MANIFEST}: {error}"))?;
    let runner = VerifyRunner::new(&manifest.runner.command)
        .map_err(|error| format!("invalid {MANIFEST}: {error}"))?;
    Ok(Some(runner))
}

/// A declared verification runner.
#[derive(Debug, Clone)]
pub(crate) struct VerifyRunner {
    /// The template, pre-split on whitespace and validated.
    tokens: Vec<String>,
}

impl VerifyRunner {
    /// Splits a command template and validates its placeholders at
    /// parse time, so a half-substituted command can never reach
    /// execution.
    pub(crate) fn new(command: &str) -> Result<Self, String> {
        let tokens: Vec<String> = command.split_whitespace().map(str::to_owned).collect();
        if tokens.is_empty() {
            return Err(String::from("runner.command is empty"));
        }
        for token in &tokens {
            // Standalone placeholders are always fine; anything else
            // may embed `{workspace}` (docker mount specs need that)
            // but no other placeholder, because `{program}` and
            // `{args}` expand to whole argv entries.
            if matches!(token.as_str(), "{workspace}" | "{program}" | "{args}") {
                continue;
            }
            let residual = token.replace("{workspace}", "");
            if residual.contains('{') || residual.contains('}') {
                return Err(format!(
                    "token {token:?} carries an unknown or embedded placeholder; only \
                     {{workspace}} may embed, {{program}} and {{args}} must be standalone"
                ));
            }
        }
        Ok(Self { tokens })
    }

    /// Expands the template against one planned command. Validation
    /// happened at parse time ([`Self::new`]); this stays defensive.
    pub(crate) fn expand(
        &self,
        workspace: &Path,
        program: &str,
        args: &[String],
    ) -> Result<Vec<String>, String> {
        let workspace = workspace.display().to_string();
        let mut argv = Vec::with_capacity(self.tokens.len() + args.len());
        for token in &self.tokens {
            match token.as_str() {
                "{workspace}" => argv.push(workspace.clone()),
                "{program}" => argv.push(program.to_owned()),
                "{args}" => argv.extend(args.iter().cloned()),
                _ => argv.push(token.replace("{workspace}", &workspace)),
            }
        }
        Ok(argv)
    }

    /// Executes the planned command through the runner, unconfined.
    ///
    /// The caller renders evidence and records the effect; this only
    /// runs and reports. The `verify.runner` notice text is returned
    /// ready to attach.
    pub(crate) async fn run(
        &self,
        ctx: &ToolContext,
        program: &str,
        args: &[String],
    ) -> Result<(RunnerOutcome, ToolNotice), ToolError> {
        let argv = self
            .expand(&ctx.workspace_root, program, args)
            .map_err(ToolError::Execution)?;
        let Some((runner_program, runner_args)) = argv.split_first() else {
            return Err(ToolError::Execution(format!(
                "invalid {MANIFEST}: expanded to an empty command"
            )));
        };
        let runner_program = runner_program.clone();
        let runner_args = runner_args.to_vec();
        // Deliberately unconfined: docker/ssh clients need the daemon
        // socket or the network, which the OS sandbox forbids. The
        // declaration is the operator's approval; the notice and the
        // recorded CommandEffect keep the trace honest.
        let sandbox = crate::sandbox::Sandbox::resolve(
            &ctx.workspace_root,
            crate::sandbox::SandboxGrants {
                enabled: false,
                network: false,
                extra_writable: Vec::new(),
            },
        );
        let outcome = crate::exec::run_bounded(
            &runner_program,
            &runner_args,
            ctx,
            &crate::exec::ExecLimits::default(),
            &sandbox,
        )
        .await?;
        // The caller records the CommandEffect from the returned argv,
        // alongside whatever effects the host call already carries.
        let notice = ToolNotice::new(
            NOTICE_KIND,
            format!(
                "verification ran through the declared runner \
                 (\"{runner_program}\" from {MANIFEST}), outside OS confinement"
            ),
        );
        Ok((
            RunnerOutcome {
                output: outcome.output,
                pipe_hold_truncated: outcome.pipe_hold_truncated,
                runner_program,
                runner_args,
            },
            notice,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runner(command: &str) -> Result<VerifyRunner, String> {
        VerifyRunner::new(command)
    }

    #[test]
    fn missing_manifest_means_local() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(runner_for(dir.path()).expect("no manifest reads").is_none());
    }

    #[test]
    fn manifest_requires_a_nonempty_runner_command() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".loom")).expect("mkdir");
        std::fs::write(dir.path().join(MANIFEST), "[runner]\ncommand = ''\n").expect("write");
        let error = runner_for(dir.path()).expect_err("empty command rejected");
        assert!(error.contains("empty"), "{error}");

        std::fs::write(dir.path().join(MANIFEST), "not a manifest\n").expect("write");
        let error = runner_for(dir.path()).expect_err("garbage rejected");
        assert!(error.contains("invalid"), "{error}");

        std::fs::write(
            dir.path().join(MANIFEST),
            "[runner]\ncommand = 'docker run {img} {program} {args}'\n",
        )
        .expect("write");
        let error = runner_for(dir.path()).expect_err("unknown placeholder rejected");
        assert!(error.contains("placeholder"), "{error}");
    }

    #[test]
    fn manifest_parses_into_tokens() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".loom")).expect("mkdir");
        std::fs::write(
            dir.path().join(MANIFEST),
            "[runner]\ncommand = 'docker run --rm {program} {args}'\n",
        )
        .expect("write");
        let runner = runner_for(dir.path())
            .expect("valid manifest")
            .expect("runner");
        assert_eq!(runner.tokens.len(), 5);
    }

    #[test]
    fn expand_spreads_args_and_embeds_workspace() {
        let runner = runner("docker run -v {workspace}:/ws img {program} {args}")
            .expect("docker mount template validates");
        let argv = runner
            .expand(
                Path::new("/tmp/repo"),
                "cargo",
                &["test".to_owned(), "--message-format=json".to_owned()],
            )
            .expect("expands");
        assert_eq!(
            argv,
            vec![
                "docker".to_owned(),
                "run".to_owned(),
                "-v".to_owned(),
                "/tmp/repo:/ws".to_owned(),
                "img".to_owned(),
                "cargo".to_owned(),
                "test".to_owned(),
                "--message-format=json".to_owned(),
            ]
        );
    }

    #[test]
    fn embedded_program_placeholder_is_rejected_at_parse() {
        let error = runner("pre{program}post").expect_err("embedded program rejected");
        assert!(error.contains("standalone"), "{error}");
        let error = runner("x {args}:1").expect_err("embedded args rejected");
        assert!(error.contains("standalone"), "{error}");
    }

    #[test]
    fn expand_allows_braceless_and_programless_templates() {
        let runner = runner("remote-verify --workspace {workspace}").expect("valid");
        let argv = runner
            .expand(Path::new("/w"), "cargo", &["test".to_owned()])
            .expect("expands");
        // The planned command need not appear; a wrapper script may
        // derive it from the workspace itself.
        assert_eq!(
            argv,
            vec![
                "remote-verify".to_owned(),
                "--workspace".to_owned(),
                "/w".to_owned()
            ]
        );
    }
}
