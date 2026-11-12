//! Container-run assembly: the official testbed image naming, the
//! agent arms (loom / mini-swe-agent) and their frozen container
//! scripts, and the pure-argv `docker run` builder.

use std::path::Path;

/// The official per-instance testbed image reference. The swebench
/// harness names evaluation images `sweb.eval.<arch>.<instance_id>`
/// with the id lowercased and `/`/`:` folded to `_`; on arm64 the
/// local build (`swebench images build SWE-bench/swe-bench-tasks
/// --dataset verified -i <id>`) produces exactly this tag.
pub fn testbed_image(instance_id: &str) -> String {
    format!(
        "sweb.eval.x86_64.{}:latest",
        instance_id.to_lowercase().replace(['/', ':'], "_")
    )
}

/// The name the official scorer looks the testbed image up under:
/// Docker Hub rejects dunders in image names, so the harness rewrites
/// every `__` in the key to `_1776_` and prefixes the namespace
/// (`swebench/image_builder/image_spec.py`). Locally built images carry
/// the plain tag, so scoring needs a one-time
/// `docker tag <testbed_image> <scored_image>` alias per instance —
/// otherwise the harness tries to pull from Docker Hub and 404s.
pub fn scored_image(instance_id: &str) -> String {
    format!(
        "swebench/sweb.eval.x86_64.{}:latest",
        instance_id.to_lowercase().replace("__", "_1776_")
    )
}

/// Env vars that may carry the provider's API key, mirroring the
/// engine's auth table (`loom-engine` `provider_env_names`). The first
/// one set on the host is forwarded into the container.
pub fn api_key_envs(provider: &str) -> Vec<String> {
    match provider {
        "deepseek" => vec!["DEEPSEEK_API_KEY".into()],
        "anthropic" => vec!["ANTHROPIC_API_KEY".into()],
        "openrouter" => vec!["OPENROUTER_API_KEY".into()],
        "glm" => vec!["ZAI_API_KEY".into(), "Z_AI_API_KEY".into()],
        "nvidia" => vec!["NVIDIA_API_KEY".into(), "NIM_API_KEY".into()],
        "groq" => vec!["GROQ_API_KEY".into()],
        "custom" => vec!["CUSTOM_API_KEY".into()],
        // A named provider (`loom provider add`) derives its env var
        // the same way the engine does (`registry::env_var_name`).
        other => vec![loom_core::engine::registry::env_var_name(other)],
    }
}

/// The script the container runs. Values arrive as environment
/// (`LOOM_*` + the provider key) so the script itself needs no host
/// interpolation. The patch is extracted even when loom fails or times
/// out — partial work still scores.
///
/// The container *is* the isolation boundary, so the in-container
/// command sandbox is passthrough: the testbed images ship no
/// `bubblewrap`, nested user namespaces are blocked under docker
/// anyway, and the seeded `.loom/commands.toml` admits the toolchain
/// (python/pip/shell/git) with `sandbox = false`. `LOOM_SKIP_SANDBOX_
/// PREFLIGHT` skips the bwrap probe for the same reason. The manifest
/// is removed before the diff so it never leaks into `model_patch`.
///
/// Anti-cheat (review F-05): before the agent starts, the testbed's
/// auxiliary refs are deleted while the active `refs/heads/master` base
/// is preserved, reflogs expired, unreachable objects pruned — so the fix commit
/// (which exists in upstream history up to `base_commit` in some
/// testbeds) cannot be recovered locally with `git log`/`show`/`fsck`.
/// The scorer compares patches, so a refless worktree scores
/// identically. The runner additionally scans each trajectory for
/// network-retrieval commands and flags them (`integrity_flags` in
/// the sweep row).
pub const CONTAINER_SCRIPT: &str = r#"mkdir -p /testbed/.loom && cat > /testbed/.loom/commands.toml <<'EOF'
[[command]]
program = "bash"
sandbox = false

[[command]]
program = "sh"
sandbox = false

[[command]]
program = "python"
sandbox = false

[[command]]
program = "python3"
sandbox = false

[[command]]
program = "pip"
sandbox = false

[[command]]
program = "pip3"
sandbox = false

[[command]]
program = "git"
sandbox = false
EOF
git -C /testbed for-each-ref --format='%(refname)' | grep -v '^HEAD$' | grep -v '^refs/heads/master$' | while read -r ref; do git -C /testbed update-ref -d "$ref" 2>/dev/null || true; done; git -C /testbed reflog expire --expire=now --all 2>/dev/null || true; git -C /testbed gc --prune=now --quiet 2>/dev/null || true
if [ -n "${LOOM_BASE_URL:-}" ]; then set -- --base-url "$LOOM_BASE_URL"; else set --; fi; timeout "$LOOM_TIMEOUT_SECS" /loom/loom run --json --no-save --provider "$LOOM_PROVIDER" --model "$LOOM_MODEL" "$@" --max-turns "$LOOM_MAX_TURNS" --workspace /testbed "$(cat /prompt.txt)" > /out/run.json 2> /out/run.err; status=$?; rm -rf /testbed/.loom; cd /testbed && git add -A && git diff --cached > /out/patch.diff 2> /out/patch.err; exit $status"#;

/// Which agent the container runs. `Loom` is the product under test;
/// `Mini` swaps in mini-swe-agent — the published 100-line bash-only
/// baseline — while the prompt, image, patch extraction, and scoring
/// stay byte-identical, so a loom-vs-mini contrast isolates the agent
/// loop itself rather than the harness around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentKind {
    /// The staged loom binary (`CONTAINER_SCRIPT`).
    #[default]
    Loom,
    /// mini-swe-agent from PyPI (`MINI_CONTAINER_SCRIPT`).
    Mini,
}

/// The script for the [`AgentKind::Mini`] baseline arm. Same contract
/// as [`CONTAINER_SCRIPT`]: env in, `patch.diff` + `run.err` out, the
/// patch extracted even when the agent fails or times out — partial
/// work still scores. The agent runs through litellm's OpenAI
/// provider, so the model becomes `openai/<name>` and the endpoint is
/// `OPENAI_BASE_URL`/`OPENAI_API_BASE`; the key arrives as
/// `OPENAI_API_KEY` (the runner renames the provider key to match).
/// No `/out/run.json`: mini emits no event stream, so turn/token
/// metrics read as zero for this arm and contrasts carry score deltas
/// only. Their shipped `swebench.yaml` is used verbatim — its prompts
/// and step limit ARE the baseline being measured.
/// mini-swe-agent, run headless inside the testbed we already provide.
///
/// Recipe per their v2 docs (docs/advanced/v2_migration.md, environments.md):
/// `-c swebench.yaml` loads their published SWE-bench protocol (prompts,
/// 250-step limit, tool-calling); `-c environment.environment_class=local`
/// overrides their docker backend because this container IS the testbed
/// (no nested docker). `MSWEA_CONFIGURED` skips the first-run wizard,
/// `MSWEA_COST_TRACKING=ignore_errors` tolerates gateways that return no
/// cost metadata, `-y --exit-immediately` makes the run non-interactive,
/// and `mcp` satisfies a litellm import. The `git diff --cached` below
/// mirrors their official harness's patch extraction (unstaged work would
/// be lost otherwise).
pub const MINI_CONTAINER_SCRIPT: &str = r#"python -m pip install -q mini-swe-agent mcp && cd /testbed && export OPENAI_BASE_URL="$LOOM_BASE_URL" OPENAI_API_BASE="$LOOM_BASE_URL" MSWEA_CONFIGURED=1 MSWEA_COST_TRACKING=ignore_errors && timeout "$LOOM_TIMEOUT_SECS" mini -y --exit-immediately -c swebench.yaml -c environment.environment_class=local --model "openai/$LOOM_MODEL" -t "$(cat /prompt.txt)" > /out/run.log 2> /out/run.err; status=$?; cd /testbed && git add -A && git diff --cached > /out/patch.diff 2> /out/patch.err; exit $status"#;

impl AgentKind {
    /// Parses the `--agent` flag value (`loom` / `mini`).
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "loom" => Some(Self::Loom),
            "mini" => Some(Self::Mini),
            _ => None,
        }
    }
}

/// The container script for an agent kind.
pub fn container_script(agent: AgentKind) -> &'static str {
    match agent {
        AgentKind::Loom => CONTAINER_SCRIPT,
        AgentKind::Mini => MINI_CONTAINER_SCRIPT,
    }
}
/// One isolated agent run: a container per (instance, run).
pub struct ContainerSpec<'a> {
    /// Docker image (see [`testbed_image`]).
    pub image: &'a str,
    /// Host path to the linux loom binary; bind-mounted at `/loom/loom`.
    /// `None` for arms that run no loom build (mini) — no mount, and
    /// provenance records no digest for the sweep.
    pub loom_binary: Option<&'a Path>,
    /// Host path to the prompt file; bind-mounted read-only at `/prompt.txt`.
    pub prompt_file: &'a Path,
    /// Host output directory; bind-mounted at `/out` (run.json, run.err,
    /// patch.diff, patch.err land here).
    pub out_dir: &'a Path,
    /// `--provider` value for the in-container loom run.
    pub provider: &'a str,
    /// `--model` value for the in-container loom run.
    pub model: &'a str,
    /// Chat API base URL for OpenAI-compatible gateways (the `custom`
    /// provider). Forwarded as `LOOM_BASE_URL` and injected into the
    /// loom invocation as `--base-url`; `None` for the vendors whose
    /// provider adapters ship their own default endpoint.
    pub base_url: Option<&'a str>,
    /// Env var name that carries the API key (see [`api_key_envs`]).
    pub api_key_env: &'a str,
    /// The API key value forwarded via `-e`.
    pub api_key: &'a str,
    /// Which agent the container runs (see [`AgentKind`]). Controls the
    /// script `docker_run_argv` executes; the loom binary mount stays
    /// for both arms — provenance hashes it either way.
    pub agent: AgentKind,
    /// Frozen script override: when set, `docker_run_argv` runs this
    /// script instead of `container_script(agent)`. The Multilingual
    /// Rust slice uses it to inject [`crate::multilingual::RUST_CONTAINER_SCRIPT`]
    /// while reusing everything else about the loom arm; `None` keeps
    /// the Verified behavior byte-identical.
    pub script_override: Option<&'a str>,
    /// Wall-clock budget for the whole loom run, in seconds.
    pub timeout_secs: u64,
    /// `--max-turns` budget for the in-container loom run.
    pub max_turns: usize,
    /// Selects the bare-loop arm (roadmap item 1): forwards `LOOM_BARE=1`
    /// so the in-container binary strips every harness lever (facts,
    /// pruning, completion gate, failure ledger, fast paths). The
    /// container script itself stays byte-identical — the env var is
    /// consumed by the loom binary, like `LOOM_SKIP_SANDBOX_PREFLIGHT`.
    pub bare: bool,
    /// Additional `LOOM_*` environment overlay for the in-container
    /// engine (roadmap item 20.3, the self-optimization loop's probes).
    ///
    /// Each pair is forwarded as `-e K=V` after the fixed env above and
    /// before the volumes; the container script stays byte-identical —
    /// the in-container engine applies these through the same
    /// `LOOM_*` override seams (`budgets_with_env`, `prune_with_env`)
    /// that a shell user would set. Names colliding with the fixed set
    /// ([`RESERVED_CONTAINER_ENV`]) are a bug in the caller and panic
    /// here rather than silently shadowing the run's own provider or
    /// budget.
    pub extra_env: &'a [(String, String)],
    /// Extra Docker run arguments for benchmark-specific execution. Keep
    /// empty for generic SWE-bench and selfopt; the Rust pilot uses its
    /// own task-container setup for nested bubblewrap namespaces.
    pub docker_args: &'a [&'a str],
    /// Unique container name (`--name`), e.g. `loom-<slug>-run1`.
    pub container_name: &'a str,
}

/// Env names `ContainerSpec` already owns; [`ContainerSpec::extra_env`]
/// entries with these names panic in [`docker_run_argv`].
pub const RESERVED_CONTAINER_ENV: [&str; 7] = [
    "LOOM_PROVIDER",
    "LOOM_MODEL",
    "LOOM_BASE_URL",
    "LOOM_BARE",
    "LOOM_SKIP_SANDBOX_PREFLIGHT",
    "LOOM_TIMEOUT_SECS",
    "LOOM_MAX_TURNS",
];

/// Encodes header pairs as the `LOOM_HEADERS` value: comma-separated
/// `K=V` pairs, re-parsed by the in-container CLI's `--header` (which
/// splits on the same delimiter). Pair values must therefore be
/// comma-free — the eval runner's are (instance ids, attempt numbers).
pub fn headers_env_value(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Assembles the `docker run` argv for one container run. Pure argv —
/// no host shell — so paths and values survive verbatim.
pub fn docker_run_argv(spec: &ContainerSpec<'_>) -> Vec<String> {
    for (name, _) in spec.extra_env {
        assert!(
            !RESERVED_CONTAINER_ENV.contains(&name.as_str()),
            "extra_env collides with a fixed container env var: {name}"
        );
        assert!(
            name != spec.api_key_env,
            "extra_env collides with the API key env var: {name}"
        );
    }
    let prompt_mount = format!("{}:/prompt.txt:ro", spec.prompt_file.display());
    let out_mount = format!("{}:/out", spec.out_dir.display());
    let timeout = spec.timeout_secs.to_string();
    let max_turns = spec.max_turns.to_string();
    let mut argv = vec![
        "run".into(),
        "--rm".into(),
        "--name".into(),
        spec.container_name.into(),
        "-e".into(),
        format!("LOOM_PROVIDER={}", spec.provider),
        "-e".into(),
        format!("LOOM_MODEL={}", spec.model),
    ];
    // Only loom-running arms carry the binary; mini needs no mount.
    let loom_mount = spec
        .loom_binary
        .map(|loom_binary| format!("{}:/loom/loom:ro", loom_binary.display()));
    if let Some(base_url) = spec.base_url {
        argv.extend(["-e".into(), format!("LOOM_BASE_URL={base_url}")]);
    }
    if spec.bare {
        argv.extend(["-e".into(), "LOOM_BARE=1".into()]);
    }
    for (name, value) in spec.extra_env {
        argv.extend(["-e".into(), format!("{name}={value}")]);
    }
    argv.extend(spec.docker_args.iter().map(|arg| (*arg).to_owned()));
    argv.extend([
        "-e".into(),
        "LOOM_SKIP_SANDBOX_PREFLIGHT=1".into(),
        "-e".into(),
        format!("LOOM_TIMEOUT_SECS={timeout}"),
        "-e".into(),
        format!("LOOM_MAX_TURNS={max_turns}"),
        "-e".into(),
        format!("{}={}", spec.api_key_env, spec.api_key),
        "-v".into(),
        prompt_mount,
        "-v".into(),
        out_mount,
    ]);
    if let Some(loom_mount) = loom_mount {
        argv.extend(["-v".into(), loom_mount]);
    }
    argv.extend([
        spec.image.into(),
        "sh".into(),
        "-c".into(),
        spec.script_override
            .unwrap_or(container_script(spec.agent))
            .into(),
    ]);
    argv
}

// ------------------------------------------------------------ tests ---

#[test]
fn headers_env_value_encodes_comma_pairs() {
    // The container path: `LOOM_HEADERS` is re-parsed by the CLI's
    // `--header` (comma-delimited), so the encoding stays the exact
    // inverse of that split.
    let value = headers_env_value(&[
        ("X-Title".to_owned(), "loom eval".to_owned()),
        (
            "X-Loom-Run-Id".to_owned(),
            "django__django-11239".to_owned(),
        ),
        ("X-Loom-Attempt".to_owned(), "2".to_owned()),
    ]);
    assert_eq!(
        value,
        "X-Title=loom eval,X-Loom-Run-Id=django__django-11239,X-Loom-Attempt=2"
    );
    assert_eq!(headers_env_value(&[]), "");
}

#[test]
fn testbed_image_folds_instance_ids_like_the_harness() {
    // Classic Verified ids are already `org__repo-num`; fold anyway so
    // variants with `/` or `:` still map to the harness's tag rule.
    assert_eq!(
        testbed_image("sympy__sympy-20590"),
        "sweb.eval.x86_64.sympy__sympy-20590:latest"
    );
    assert_eq!(
        testbed_image("Django/Django:16379"),
        "sweb.eval.x86_64.django_django_16379:latest"
    );
}

#[test]
fn scored_image_applies_the_harness_dunder_rewrite() {
    assert_eq!(
        scored_image("astropy__astropy-12907"),
        "swebench/sweb.eval.x86_64.astropy_1776_astropy-12907:latest"
    );
    // Multi-dunder repo names rewrite every `__`.
    assert_eq!(
        scored_image("scikit-learn__scikit-learn-13240"),
        "swebench/sweb.eval.x86_64.scikit-learn_1776_scikit-learn-13240:latest"
    );
}

#[test]
fn api_key_envs_mirror_the_engine_auth_table() {
    assert_eq!(api_key_envs("deepseek"), ["DEEPSEEK_API_KEY"]);
    assert_eq!(api_key_envs("custom"), ["CUSTOM_API_KEY"]);
    assert_eq!(api_key_envs("glm"), ["ZAI_API_KEY", "Z_AI_API_KEY"]);
    // Named providers derive their key var the engine's way.
    assert_eq!(api_key_envs("orcarouter"), ["ORCAROUTER_API_KEY"]);
}

#[test]
fn docker_argv_mounts_and_env_without_a_shell() {
    let spec = ContainerSpec {
        agent: AgentKind::Loom,
        script_override: None,
        image: "sweb.eval.x86_64.org__one-1:latest",
        loom_binary: Some(Path::new("/eval/loom-linux/loom")),
        prompt_file: Path::new("/eval/swebench/instances/org-one-1/prompt-run1.txt"),
        out_dir: Path::new("/eval/swebench/instances/org-one-1/run-1"),
        provider: "deepseek",
        model: "deepseek-flash",
        base_url: None,
        api_key_env: "DEEPSEEK_API_KEY",
        api_key: "sk-test",
        timeout_secs: 3600,
        max_turns: 60,
        bare: false,
        extra_env: &[],
        docker_args: &[],
        container_name: "loom-org-one-1-run1",
    };
    let argv = docker_run_argv(&spec);
    let joined = argv.join("\n");
    assert_eq!(&argv[0], "run");
    assert!(joined.contains("--rm\n"));
    assert!(joined.contains("--name\nloom-org-one-1-run1\n"));
    assert!(joined.contains("-e\nLOOM_PROVIDER=deepseek\n-e\nLOOM_MODEL=deepseek-flash"));
    // Full arm: no LOOM_BARE is forwarded.
    assert!(!joined.contains("LOOM_BARE"));
    // Generic SWE-bench runs do not receive extra container privileges.
    assert!(joined.contains("-e\nLOOM_SKIP_SANDBOX_PREFLIGHT=1"));
    assert!(!joined.contains("--cap-add"));
    assert!(joined.contains("-e\nLOOM_TIMEOUT_SECS=3600\n"));
    assert!(joined.contains("-e\nLOOM_MAX_TURNS=60\n"));
    assert!(joined.contains("-e\nDEEPSEEK_API_KEY=sk-test\n"));
    assert!(joined.contains("-v\n/eval/loom-linux/loom:/loom/loom:ro\n"));
    assert!(
        joined.contains("-v\n/eval/swebench/instances/org-one-1/prompt-run1.txt:/prompt.txt:ro\n")
    );
    assert!(joined.contains("-v\n/eval/swebench/instances/org-one-1/run-1:/out\n"));
    // The script is a single argv element passed to sh -c.
    let script_pos = argv.iter().position(|arg| arg == "-c").unwrap();
    assert_eq!(argv[script_pos + 1], CONTAINER_SCRIPT);
    assert!(argv.contains(&"sweb.eval.x86_64.org__one-1:latest".to_owned()));
    // The script reads its knobs from env, extracts the patch even on
    // failure, and preserves loom's exit status.
    assert!(CONTAINER_SCRIPT.contains("timeout \"$LOOM_TIMEOUT_SECS\""));
    assert!(CONTAINER_SCRIPT.contains("--no-save"));
    assert!(CONTAINER_SCRIPT.contains("git diff --cached > /out/patch.diff"));
    assert!(CONTAINER_SCRIPT.ends_with("exit $status"));
    // It seeds a passthrough commands manifest (the container is the
    // sandbox) and removes it again so the patch stays clean.
    assert!(CONTAINER_SCRIPT.contains("/testbed/.loom/commands.toml"));
    assert!(CONTAINER_SCRIPT.contains("program = \"python3\""));
    assert!(CONTAINER_SCRIPT.contains("sandbox = false"));
    assert!(CONTAINER_SCRIPT.contains("rm -rf /testbed/.loom;"));
    // No gateway base URL → no LOOM_BASE_URL env assignment (the
    // script's own guard still references the variable, which stays
    // unset); a gateway spec below asserts the assignment appears.
    assert!(!joined.contains("-e\nLOOM_BASE_URL="));
    assert!(CONTAINER_SCRIPT.contains("set -- --base-url \"$LOOM_BASE_URL\""));

    let gateway = ContainerSpec {
        agent: AgentKind::Loom,
        script_override: None,
        base_url: Some("https://free.empexaura.com/v1"),
        provider: "custom",
        model: "nano-model",
        api_key_env: "CUSTOM_API_KEY",
        api_key: "nano-test",
        docker_args: &["--cap-add=SYS_ADMIN"],
        ..spec
    };
    let argv = docker_run_argv(&gateway);
    let joined = argv.join("\n");
    assert!(joined.contains("-e\nLOOM_BASE_URL=https://free.empexaura.com/v1"));
    assert!(joined.contains("-e\nCUSTOM_API_KEY=nano-test"));
    assert!(joined.contains("--cap-add=SYS_ADMIN"));

    // The bare-loop arm (roadmap item 1): one env var, and the frozen
    // container script stays byte-identical — the binary consumes the
    // knob, not the script.
    let bare_arm = ContainerSpec {
        bare: true,
        ..gateway
    };
    let argv = docker_run_argv(&bare_arm);
    let joined = argv.join("\n");
    assert!(joined.contains("-e\nLOOM_BARE=1\n"));
    assert!(!CONTAINER_SCRIPT.contains("LOOM_BARE"));

    // The self-optimization overlay (roadmap item 20.3): forwarded after
    // the fixed env, and the frozen script still stays byte-identical —
    // the in-container engine consumes the knobs.
    let overlay = vec![
        ("LOOM_PRUNE_TRIGGER_PERCENT".to_owned(), "70".to_owned()),
        ("LOOM_READ_HEAD_BYTES".to_owned(), "32768".to_owned()),
    ];
    let overlaid = ContainerSpec {
        agent: AgentKind::Loom,
        script_override: None,
        extra_env: &overlay,
        docker_args: &[],
        container_name: "loom-org-one-1-selfopt-p1",
        ..bare_arm
    };
    let argv = docker_run_argv(&overlaid);
    let script_pos = argv.iter().position(|arg| arg == "-c").unwrap();
    assert_eq!(argv[script_pos + 1], CONTAINER_SCRIPT);
    let joined = argv.join("\n");
    let overlay_pos = joined
        .find("-e\nLOOM_PRUNE_TRIGGER_PERCENT=70\n-e\nLOOM_READ_HEAD_BYTES=32768")
        .expect("overlay forwarded in order");
    let fixed_pos = joined
        .find("-e\nLOOM_TIMEOUT_SECS=3600")
        .expect("fixed env present");
    assert!(
        overlay_pos > joined.find("-e\nLOOM_BARE=1").unwrap(),
        "overlay comes after the fixed env"
    );
    assert!(
        overlay_pos < joined.find("-v\n/eval/loom-linux/loom").unwrap(),
        "overlay comes before the volumes"
    );
    let _ = fixed_pos;

    // Reserved names are caller bugs: shadowing the run's own provider,
    // budget, or key would corrupt the probe silently.
    for reserved in RESERVED_CONTAINER_ENV {
        let collision = ContainerSpec {
            extra_env: &[(reserved.to_owned(), "1".to_owned())],
            docker_args: &[],
            ..overlaid
        };
        let result = std::panic::catch_unwind(|| docker_run_argv(&collision));
        assert!(result.is_err(), "reserved name must panic: {reserved}");
    }
    let key_collision = ContainerSpec {
        extra_env: &[("CUSTOM_API_KEY".to_owned(), "x".to_owned())],
        docker_args: &[],
        ..overlaid
    };
    assert!(std::panic::catch_unwind(|| docker_run_argv(&key_collision)).is_err());
}

#[test]
fn container_script_strips_the_testbed_history_before_the_agent_runs() {
    // F-05's other half: auxiliary refs deleted, reflogs expired, objects
    // pruned — all before /loom/loom starts, so no in-run git command
    // can reach the fix commit. Preserve the active master ref so git's
    // checkout retains the intended base tree while the history is stripped.
    let strip = CONTAINER_SCRIPT
        .split("if [ -n")
        .next()
        .expect("script keeps the loom invocation last");
    assert!(strip.contains("for-each-ref --format='%(refname)'"));
    assert!(strip.contains("update-ref -d"));
    assert!(strip.contains("reflog expire --expire=now --all"));
    assert!(strip.contains("grep -v '^refs/heads/master$'"));
    assert!(strip.contains("gc --prune=now"));
    assert!(
        strip.contains("git -C /testbed for-each-ref"),
        "strip runs against the testbed"
    );
    let loom_pos = CONTAINER_SCRIPT.find("/loom/loom run").expect("loom runs");
    let strip_pos = CONTAINER_SCRIPT.find("for-each-ref").expect("strip");
    assert!(
        strip_pos < loom_pos,
        "history is destroyed before the agent starts"
    );
}

#[test]
fn mini_agent_arm_selects_its_script_and_opens_the_openai_dialect() {
    // Same spec shape as the loom arm, one field flipped: the argv must
    // run MINI_CONTAINER_SCRIPT and the key must ride as OPENAI_API_KEY
    // (litellm reads nothing else).
    let spec = ContainerSpec {
        agent: AgentKind::Mini,
        script_override: None,
        image: "sweb.eval.x86_64.org__one-1:latest",
        loom_binary: None,
        prompt_file: Path::new("/eval/swebench/instances/org-one-1/prompt-run1.txt"),
        out_dir: Path::new("/eval/swebench/instances/org-one-1/run-1"),
        provider: "kilo",
        model: "nano-model",
        base_url: Some("https://free.empexaura.com/v1"),
        api_key_env: "OPENAI_API_KEY",
        api_key: "nano-test",
        timeout_secs: 3600,
        max_turns: 60,
        bare: false,
        extra_env: &[],
        docker_args: &[],
        container_name: "mini-org-one-1-run1",
    };
    let argv = docker_run_argv(&spec);
    let script_pos = argv.iter().position(|arg| arg == "-c").unwrap();
    assert_eq!(argv[script_pos + 1], MINI_CONTAINER_SCRIPT);
    let joined = argv.join("\n");
    assert!(joined.contains("-e\nOPENAI_API_KEY=nano-test"));
    assert!(joined.contains("-e\nLOOM_BASE_URL=https://free.empexaura.com/v1"));
    // No loom build runs in this arm: no mount, nothing to shadow.
    assert!(!joined.contains("/loom/loom"));
}

#[test]
fn mini_script_keeps_the_shared_extraction_contract() {
    // The baseline arm must score the same way the loom arm does: the
    // patch is extracted even after a failure (`status=$?` then diff,
    // not `set -e` semantics), and the model is routed through
    // litellm's OpenAI provider against the forwarded base URL.
    assert!(
        MINI_CONTAINER_SCRIPT.contains(
            "status=$?; cd /testbed && git add -A && git diff --cached > /out/patch.diff"
        )
    );
    assert!(MINI_CONTAINER_SCRIPT.contains("--model \"openai/$LOOM_MODEL\""));
    assert!(MINI_CONTAINER_SCRIPT.contains("OPENAI_BASE_URL=\"$LOOM_BASE_URL\""));
    // Their shipped swebench config is the baseline being measured —
    // never a hand-rolled prompt.
    assert!(MINI_CONTAINER_SCRIPT.contains("swebench.yaml"));
}
