# Loom

[![Status: Work in Progress](https://img.shields.io/badge/status-work%20in%20progress-orange)]()

> **⚠️ Work in progress.** Loom is under active early development. Anything
> here can change or break without notice. Not usable yet — no releases,
> no stability guarantees.

Loom is a headless, provider-agnostic **coding-agent core** written in Rust.
The deterministic loop is the controller; the model is only the reasoning
component.

Design goal: **maximum successful work per model invocation with minimum
context and turns.**

## Principles

1. Never spend a model invocation on work deterministic software can do.
2. Never give the model context it does not need for its next decision.
3. The model decides *what* to do next; deterministic code decides
   everything that can be decided without it — context selection, tool
   scheduling, retries, validation, termination.

Capabilities move from model to software over time, never the reverse.

## Crates

| Crate | Purpose |
|---|---|
| `loom-kernel` | Domain model and traits (session, turns, tools, providers). No workspace dependencies. |
| `loom-providers` | Provider adapters: DeepSeek, OpenRouter, GLM, NVIDIA (OpenAI-compatible wire format) and Anthropic (messages API). |
| `loom-store` | SQLite persistence for sessions and events. |
| `loom-tools` | Built-in deterministic tools: file read/write/edit, search, listing, sandboxed command execution. |
| `loom-engine` | The deterministic turn loop: context compilation, token estimation, pruning, repo intelligence, completion gate. |
| `loom-core` | Facade re-exporting the public API for embedders. |
| `loom-cli` | `loom` binary: config, tracing, signals, output, `loom stats`. |

Dependency direction is enforced: `loom-kernel` depends on no workspace
crate, so provider- or tool-specific behavior cannot leak into the domain.
`loom-engine` builds on kernel, store, providers, and tools; `loom-core`
and `loom-cli` sit on top.

## Status

Iteration 2 — the loop runs end to end with real providers:

```
Task → Session → engine turn loop → provider (DeepSeek / OpenRouter / Anthropic / GLM / NVIDIA)
     → tool calls → deterministic execution (incl. sandboxed shell)
     → tool results → pruning/estimation → completion gate → done
```

Shipped so far: time-ordered session/task IDs, transcript types, session
aggregate with atomic turn commits, streaming adapters for five providers,
per-provider token estimators (o200k-based; Anthropic-scaled), context
pruning with turn-age masking and one-line stubs, bootstrap repo facts
(tree, toolchains, AGENTS.md), sandboxed command execution (Seatbelt on
macOS, bubblewrap on Linux), `edit_file` batch edits with region echo and
whitespace-trim repair, a completion gate that refuses premature end turns,
and `loom stats`. 669 tests.

## Development

Requires Rust 1.85+ (edition 2024).

```sh
cargo build
cargo test
cargo clippy --all-targets
cargo fmt --check
```

Workspace lints: `unsafe_code` forbidden; `missing_docs`, `unwrap_used`,
`expect_used` warned.

Documentation: [`docs/architecture.md`](docs/architecture.md) (design),
[`docs/eval.md`](docs/eval.md) (eval suite protocol),
[`docs/roadmap.md`](docs/roadmap.md) (vision, iterations, evidence
backlog).

## License

TBD — this project has not chosen a license yet.
