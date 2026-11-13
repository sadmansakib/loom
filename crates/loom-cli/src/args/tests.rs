use super::*;
use clap::Parser;
use loom_core::ReasoningEffort;
use loom_core::engine::auth::Method;
use loom_core::kernel::agent::DEFAULT_MAX_TURNS;

fn parse(args: &[&str]) -> Cli {
    Cli::try_parse_from(args).unwrap()
}

#[test]
fn run_defaults_and_collects_task_words() {
    let cli = parse(&[
        "loom",
        "run",
        "--provider",
        "nvidia",
        "--model",
        "z-ai/glm-5.3-flash",
        "find",
        "the",
        "turn",
        "loop",
    ]);
    let Command::Run(args) = cli.command else {
        panic!("expected `run`");
    };
    assert_eq!(args.task, vec!["find", "the", "turn", "loop"]);
    assert_eq!(args.flags.provider, "nvidia");
    assert_eq!(args.flags.model, "z-ai/glm-5.3-flash");
    assert_eq!(args.flags.reasoning_effort, ReasoningEffort::High);
    assert_eq!(args.flags.max_turns, DEFAULT_MAX_TURNS);
    assert!(!args.flags.json);
    assert!(!args.flags.no_save);
    assert!(!args.flags.no_global_instructions);
}

#[test]
fn run_accepts_custom_provider_urls() {
    let cli = parse(&[
        "loom",
        "run",
        "--provider",
        "custom",
        "--model",
        "gateway-model",
        "--base-url",
        "http://localhost:4000/v1",
        "--models-url",
        "http://localhost:4000/v1/models",
        "do",
        "the",
        "thing",
    ]);
    let Command::Run(args) = cli.command else {
        panic!("expected `run`");
    };
    assert_eq!(args.flags.provider, "custom");
    assert_eq!(
        args.flags.base_url.as_deref(),
        Some("http://localhost:4000/v1")
    );
    assert_eq!(
        args.flags.models_url.as_deref(),
        Some("http://localhost:4000/v1/models")
    );
}

#[test]
fn run_collects_repeatable_and_delimited_headers() {
    let cli = parse(&[
        "loom",
        "run",
        "--provider",
        "custom",
        "--model",
        "gateway-model",
        "--header",
        "X-Title=loom eval",
        "--header",
        "X-A=1,X-B=2",
        "do",
        "the",
        "thing",
    ]);
    let Command::Run(args) = cli.command else {
        panic!("expected `run`");
    };
    assert_eq!(
        args.flags.headers,
        vec![
            "X-Title=loom eval".to_owned(),
            "X-A=1".to_owned(),
            "X-B=2".to_owned()
        ]
    );
    let pairs = parse_header_pairs(&args.flags.headers).expect("valid pairs");
    assert_eq!(
        pairs,
        vec![
            ("X-Title".to_owned(), "loom eval".to_owned()),
            ("X-A".to_owned(), "1".to_owned()),
            ("X-B".to_owned(), "2".to_owned()),
        ]
    );
}

#[test]
fn header_pairs_reject_malformed_input() {
    for bad in [
        "X-Title",
        "=value",
        "X Title=value",
        "X\u{7f}=value",
        "X-Title=col\u{1}lar",
        "X-Title=line\nbreak",
        "X-Title=",
    ] {
        assert!(
            parse_header_pairs(&[bad.to_owned()]).is_err(),
            "`{bad}` should be rejected"
        );
    }
}

#[test]
fn provider_subcommands_parse() {
    let cli = parse(&[
        "loom",
        "provider",
        "add",
        "orcarouter",
        "--base-url",
        "https://api.orcarouter.ai/v1",
    ]);
    let Command::Provider(args) = cli.command else {
        panic!("expected `provider`");
    };
    match args.command {
        ProviderCommand::Add {
            name,
            base_url,
            models_url,
            headers,
        } => {
            assert_eq!(name, "orcarouter");
            assert_eq!(base_url, "https://api.orcarouter.ai/v1");
            assert_eq!(models_url, None);
            assert!(headers.is_empty());
        }
        ProviderCommand::List | ProviderCommand::Remove { .. } => panic!("expected `add`"),
    }

    assert!(matches!(
        parse(&["loom", "provider", "list"]).command,
        Command::Provider(ProviderArgs {
            command: ProviderCommand::List
        })
    ));
    assert!(matches!(
        parse(&["loom", "provider", "remove", "orcarouter"]).command,
        Command::Provider(ProviderArgs {
            command: ProviderCommand::Remove { name }
        }) if name == "orcarouter"
    ));
}

#[test]
fn run_parses_lessons_flag_off_by_default() {
    let cli = parse(&[
        "loom",
        "run",
        "--provider",
        "deepseek",
        "--model",
        "deepseek-flash",
        "go",
    ]);
    let Command::Run(args) = cli.command else {
        panic!("expected `run`");
    };
    assert!(!args.flags.lessons);

    let cli = parse(&[
        "loom",
        "run",
        "--provider",
        "deepseek",
        "--model",
        "deepseek-flash",
        "--lessons",
        "go",
    ]);
    let Command::Run(args) = cli.command else {
        panic!("expected `run`");
    };
    assert!(args.flags.lessons);
}

#[test]
fn lessons_subcommand_parses() {
    let cli = parse(&[
        "loom",
        "lessons",
        "--workspace",
        "/ws",
        "--dry-run",
        "--json",
    ]);
    let Command::Lessons(args) = cli.command else {
        panic!("expected `lessons`");
    };
    assert_eq!(args.workspace, std::path::PathBuf::from("/ws"));
    assert!(args.dry_run);
    assert!(args.json);
}

#[test]
fn run_requires_provider_and_model() {
    let error = Cli::try_parse_from(["loom", "run", "x"]).unwrap_err();
    let rendered = error.to_string();
    assert!(rendered.contains("--provider"), "{rendered}");
    assert!(rendered.contains("--model"), "{rendered}");

    let error = Cli::try_parse_from(["loom", "run", "--provider", "nvidia", "x"]).unwrap_err();
    assert!(error.to_string().contains("--model"));

    let error = Cli::try_parse_from(["loom", "run", "--model", "m", "x"]).unwrap_err();
    assert!(error.to_string().contains("--provider"));
}

#[test]
fn auth_use_and_remove_require_their_provider() {
    let ok = Cli::try_parse_from(["loom", "auth", "switch", "work", "--provider", "glm"]);
    assert!(ok.is_ok());
    let error = Cli::try_parse_from(["loom", "auth", "switch", "work"]).unwrap_err();
    assert!(error.to_string().contains("--provider"), "{}", error);

    let ok = Cli::try_parse_from(["loom", "auth", "logout", "work", "--provider", "glm"]);
    assert!(ok.is_ok());
    let error = Cli::try_parse_from(["loom", "auth", "logout", "work"]).unwrap_err();
    assert!(error.to_string().contains("--provider"), "{}", error);

    let error = Cli::try_parse_from(["loom", "auth", "add"]).unwrap_err();
    assert!(error.to_string().contains("PROVIDER"), "{}", error);
}

#[test]
fn run_parses_optional_flags() {
    let cli = parse(&[
        "loom",
        "run",
        "--json",
        "--provider",
        "deepseek",
        "--model",
        "deepseek-v4-pro",
        "--base-url",
        "https://example.test",
        "--reasoning-effort",
        "max",
        "--account",
        "work",
        "--max-turns",
        "3",
        "--no-save",
        "--no-global-instructions",
        "go",
    ]);
    let Command::Run(args) = cli.command else {
        panic!("expected `run`");
    };
    assert!(args.flags.json);
    assert_eq!(args.flags.base_url.as_deref(), Some("https://example.test"));
    assert_eq!(args.flags.reasoning_effort, ReasoningEffort::Max);
    assert_eq!(args.flags.account.as_deref(), Some("work"));
    assert_eq!(args.flags.max_turns, 3);
    assert!(args.flags.no_save);
    assert!(
        args.flags.no_global_instructions,
        "--no-global-instructions opts out of the user-level file"
    );
}

#[test]
fn reasoning_effort_rejects_unknown_values() {
    let error = Cli::try_parse_from(["loom", "run", "--reasoning-effort", "bogus", "x"]);
    assert!(error.is_err());
}

#[test]
fn chat_takes_an_optional_task_and_shares_the_run_flags() {
    // No task: the prompt is the entry point.
    let cli = parse(&[
        "loom",
        "chat",
        "--provider",
        "glm",
        "--model",
        "glm-5",
        "--json",
    ]);
    let Command::Chat(args) = cli.command else {
        panic!("expected `chat`");
    };
    assert!(args.task.is_empty());
    assert_eq!(args.flags.provider, "glm");
    assert!(args.flags.json);

    // An opening task runs immediately, then the prompt takes over.
    let cli = parse(&[
        "loom",
        "chat",
        "--provider",
        "glm",
        "--model",
        "glm-5",
        "explain",
        "this",
        "repo",
    ]);
    let Command::Chat(args) = cli.command else {
        panic!("expected `chat`");
    };
    assert_eq!(args.task, ["explain", "this", "repo"]);
}

#[test]
fn route_flags_parse_models_and_default_off() {
    let cli = parse(&[
        "loom",
        "run",
        "--provider",
        "glm",
        "--model",
        "glm-5",
        "--route-easy",
        "glm-5-flash",
        "work",
    ]);
    let Command::Run(args) = cli.command else {
        panic!("expected `run`");
    };
    assert_eq!(args.flags.route_easy.as_deref(), Some("glm-5-flash"));
    assert_eq!(args.flags.route_hard, None);

    // Absent both flags, routing stays unwired (the byte-off default).
    let cli = parse(&[
        "loom",
        "run",
        "--provider",
        "glm",
        "--model",
        "glm-5",
        "work",
    ]);
    let Command::Run(args) = cli.command else {
        panic!("expected `run`");
    };
    assert_eq!(args.flags.route_easy, None);
    assert_eq!(args.flags.route_hard, None);
}

#[test]
fn stall_escalation_model_is_opt_in() {
    let cli = parse(&[
        "loom",
        "run",
        "--provider",
        "glm",
        "--model",
        "glm-5",
        "--stall-escalation-model",
        "glm-5-max",
        "work",
    ]);
    let Command::Run(args) = cli.command else {
        panic!("expected `run`");
    };
    assert_eq!(
        args.flags.stall_escalation_model.as_deref(),
        Some("glm-5-max")
    );

    // Absent, escalation stays unwired (the byte-off default).
    let cli = parse(&[
        "loom",
        "run",
        "--provider",
        "glm",
        "--model",
        "glm-5",
        "work",
    ]);
    let Command::Run(args) = cli.command else {
        panic!("expected `run`");
    };
    assert_eq!(args.flags.stall_escalation_model, None);
}

#[test]
fn bg_commands_is_opt_in_and_shared_by_run_and_chat() {
    // The flag registers the detached-command tools; the byte-off
    // default keeps headless runs (eval harnesses included) free
    // of the poll protocol.
    let cli = parse(&[
        "loom",
        "chat",
        "--provider",
        "glm",
        "--model",
        "glm-5",
        "--bg-commands",
    ]);
    let Command::Chat(args) = cli.command else {
        panic!("expected `chat`");
    };
    assert!(args.flags.bg_commands);

    let cli = parse(&[
        "loom",
        "run",
        "--provider",
        "glm",
        "--model",
        "glm-5",
        "work",
    ]);
    let Command::Run(args) = cli.command else {
        panic!("expected `run`");
    };
    assert!(!args.flags.bg_commands);
}

#[test]
fn auth_aliases_map_to_the_canonical_commands() {
    let cli = parse(&["loom", "auth", "add", "glm"]);
    let Command::Auth(auth) = cli.command else {
        panic!("expected `auth`");
    };
    let AuthCommand::Login(args) = auth.command else {
        panic!("expected `login`");
    };
    assert_eq!(args.provider, "glm");
    assert_eq!(args.method, Method::Key);

    let cli = parse(&["loom", "auth", "switch", "work", "--provider", "glm"]);
    let Command::Auth(auth) = cli.command else {
        panic!("expected `auth`");
    };
    let AuthCommand::Use(args) = auth.command else {
        panic!("expected `use`");
    };
    assert_eq!(args.target, "work");
    assert_eq!(args.provider, "glm");

    let cli = parse(&["loom", "auth", "logout", "work", "--provider", "glm"]);
    let Command::Auth(auth) = cli.command else {
        panic!("expected `auth`");
    };
    let AuthCommand::Remove(args) = auth.command else {
        panic!("expected `remove`");
    };
    assert_eq!(args.target, "work");
    assert_eq!(args.provider, "glm");
}

#[test]
fn rewind_parses_session_and_boundary() {
    let cli = parse(&["loom", "rewind", "0192e4c1-0000-7000-8000-000000000000"]);
    let Command::Rewind(args) = cli.command else {
        panic!("expected rewind");
    };
    assert_eq!(args.session, "0192e4c1-0000-7000-8000-000000000000");
    assert!(args.at.is_none());

    let cli = parse(&["loom", "rewind", "abc", "--at", "3"]);
    let Command::Rewind(args) = cli.command else {
        panic!("expected rewind");
    };
    assert_eq!(args.at, Some(3));
}

#[test]
fn stats_parses_the_trace_source() {
    let cli = parse(&["loom", "stats", "--trace", "traces/demo"]);
    let Command::Stats(args) = cli.command else {
        panic!("expected `stats`");
    };
    assert_eq!(
        args.trace.as_deref(),
        Some(std::path::Path::new("traces/demo"))
    );
    assert!(args.session.is_none());

    let cli = parse(&["loom", "stats"]);
    let Command::Stats(args) = cli.command else {
        panic!("expected `stats`");
    };
    assert!(args.trace.is_none());
}
