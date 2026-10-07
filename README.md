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
| `loom-core` | Domain + orchestration. No HTTP, no provider/tool specifics. |
| `loom-providers` | Model provider adapters (DeepSeek via OpenAI-compatible wire format). |
| `loom-tools` | Built-in deterministic tools (`read_file`, `list_dir`, `search`). |
| `loom-cli` | `loom` binary: config, tracing, signals, output. |

Dependency direction is enforced: `loom-core` cannot depend on provider or
tool crates, so provider- or tool-specific behavior cannot leak into
orchestration.

## Status

Iteration 1 — smallest foundation that can run a task end to end:

```
Task → Session → ContextCompiler → ModelProvider (DeepSeek) → tool calls
     → deterministic tool execution → tool results → model → completion
```

Implemented so far: workspace scaffold, time-ordered session/task IDs,
transcript types, session aggregate with atomic turn commits, model
request/stream/error abstractions, tool trait + registry, task event
stream, DeepSeek streaming adapter.

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
[`docs/roadmap.md`](docs/roadmap.md) (iterations).

## License

TBD — this project has not chosen a license yet.
