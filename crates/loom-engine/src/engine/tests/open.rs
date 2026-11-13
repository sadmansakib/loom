//! Engine open: credential order, overridden providers, provider hooks.

use super::*;

#[tokio::test]
async fn open_async_resolves_the_credential_before_fetching() {
    let base = tempfile::tempdir().expect("tempdir");
    let workspace = base.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let mut config = test_config();
    config.provider = OPENROUTER_PROVIDER_NAME.to_owned();
    config.workspace = workspace;
    config.db_path = base.path().join("loom.db");

    // No cached catalog and no credential: the fetch path must reach
    // credential resolution (inside `catalog::load`) and report the
    // missing credential — never a network call in tests.
    let Err(error) = Engine::open_async(config).await else {
        panic!("open succeeded without a credential")
    };
    assert!(
        matches!(
            error,
            EngineError::Auth(auth::AuthError::NoCredential { .. })
        ),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn open_with_skips_credential_resolution_for_an_overridden_provider() {
    let base = tempfile::tempdir().expect("tempdir");
    let workspace = base.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let mut config = test_config();
    config.provider = "replay".to_owned(); // no adapter, no credential
    config.workspace = workspace;
    config.db_path = base.path().join("loom.db");

    // Only the override path can succeed here: building "replay"
    // would fail with UnknownProvider before any credential work.
    let engine = Engine::open_with(
        config,
        EngineOverrides::default().with_provider(Box::new(FakeProvider::scripted(vec![
            FakeProvider::end_turn_script("overridden"),
        ]))),
    )
    .expect("no credential is resolved when the provider is overridden");
    let mut session = engine.new_session();
    let (sink, _receiver) = loom_kernel::event::ChannelEventSink::new();
    let summary = engine
        .run_task(&mut session, "task", &sink, &crate::TaskControl::default())
        .await
        .expect("task runs on the overridden provider");
    assert_eq!(summary.turns, 1);
}

#[tokio::test]
async fn a_provider_hook_wraps_whichever_provider_results() {
    let base = tempfile::tempdir().expect("tempdir");
    let workspace = base.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let mut config = test_config();
    config.provider = "replay".to_owned();
    config.workspace = workspace;
    config.db_path = base.path().join("loom.db");

    let engine = Engine::open_with(
        config,
        EngineOverrides::default()
            .with_provider(Box::new(FakeProvider::scripted(vec![
                FakeProvider::end_turn_script("original"),
            ])))
            .with_provider_hook(Arc::new(|_built: Box<dyn ModelProvider>| {
                // The hook decides what runs: a different scripted
                // answer proves the replacement took effect.
                Box::new(FakeProvider::scripted(vec![FakeProvider::end_turn_script(
                    "hooked",
                )]))
            })),
    )
    .expect("open");
    let mut session = engine.new_session();
    let (sink, _receiver) = loom_kernel::event::ChannelEventSink::new();
    engine
        .run_task(&mut session, "task", &sink, &crate::TaskControl::default())
        .await
        .expect("task runs on the hooked provider");
    let answer = session.transcript.iter().rev().find_map(|item| match item {
        loom_kernel::transcript::Item::Assistant(message) => Some(message.content.clone()),
        loom_kernel::transcript::Item::User { .. }
        | loom_kernel::transcript::Item::ToolResult(_)
        | _ => None,
    });
    assert_eq!(answer.as_deref(), Some("hooked"));
}
