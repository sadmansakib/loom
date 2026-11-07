//! Minimal frontend over the `loom-core` facade (ADR-021).
//!
//! Everything a frontend needs comes from this crate's root: the driving
//! API (`Engine`), control (`TaskControl`), and observation
//! (`EventSink`/`EventKind`). Run with a provider credential in the
//! environment, e.g.:
//!
//! ```sh
//! DEEPSEEK_API_KEY=… cargo run -p loom-core --example frontend -- \
//!     --provider deepseek --model deepseek-chat "add a hello function"
//! ```

use std::time::Duration;

use loom_core::kernel::event::{ChannelEventSink, EventKind};
use loom_core::{Engine, EngineConfig, TaskControl};

fn main() -> anyhow::Result<()> {
    // The frontend resolves data locations identically to the CLI (ADR-008).
    let mut args = std::env::args().skip(1);
    let mut provider = String::new();
    let mut model = String::new();
    let mut task = String::new();
    while let (Some(flag), Some(value)) = (args.next(), args.next()) {
        match flag.as_str() {
            "--provider" => provider = value,
            "--model" => model = value,
            _ => task = format!("{flag} {value}"),
        }
    }
    anyhow::ensure!(
        !provider.is_empty() && !model.is_empty() && !task.is_empty(),
        "usage: frontend --provider P --model M TASK…"
    );

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let engine = runtime.block_on(Engine::open_async(EngineConfig {
        provider,
        model,
        base_url: None,
        models_url: None,
        reasoning_effort: Default::default(),
        account: None,
        workspace: std::env::current_dir()?,
        system_prompt: None,
        max_turns: 8,
        turn_budget: Default::default(),
        context_editing: None,
        db_path: loom_core::paths::db_path(),
        persist: false,
        catalog: None,
        scout: None,
        symbols: false,
        git: false,
        skills: false,
        lessons: false,
        mcp: false,
        lsp: false,
        budgets: loom_tools::ToolBudgets::default(),
        prune: None,
        autofix: false,
    }))?;

    // Observation: one unbounded channel, one printing consumer. A TUI or
    // GUI frontend swaps this thread for its render loop — same sink.
    let (sink, mut receiver) = ChannelEventSink::new();
    let printer = std::thread::spawn(move || {
        while let Some(envelope) = receiver.blocking_recv() {
            if let EventKind::TurnStarted { turn } = envelope.kind {
                println!("frontend: turn {turn}");
            }
        }
    });

    // Control: a UI thread would hold this clone for cancel/pause/steer.
    let control = TaskControl::new();
    let ui_handle = control.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(5));
        // Item 25: queued steering lands at the next turn boundary.
        ui_handle.steer("keep the diff minimal");
    });

    let mut session = engine.new_session();
    let outcome = runtime.block_on(engine.run_task(&mut session, task, &sink, &control));
    drop(sink);
    let _ = printer.join();

    println!(
        "frontend: session {} — {:?}",
        session.id,
        outcome
            .as_ref()
            .map(|summary| summary.turns)
            .map_err(|error| error.to_string())
    );
    Ok(())
}
