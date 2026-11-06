# Loom

[![Status: Early release](https://img.shields.io/badge/status-early%20release-blue)]()
[![Tests: 977](https://img.shields.io/badge/tests-977-brightgreen)]()
[![SWE-bench Verified: 90/100](https://img.shields.io/badge/SWE--bench%20Verified-90%2F100-blue)]()

> **Early release.** The headless agent core is usable today: the `loom`
> binary runs tasks end to end against real providers, and `loom-core`
> exposes the embedding facade. There is no interactive TUI yet, `ask`
> policy verdicts fail closed without a frontend, and APIs version by
> iteration, not semver.

Loom is a headless, provider-agnostic **coding-agent core** written in Rust.
The deterministic loop is the controller; the model is only the reasoning
component.

Design goal: **maximum successful work per model invocation with minimum
context and turns.**

Measured: **90/100 resolved** on a frozen 100-instance SWE-bench Verified
subset (first 20 dataset rows of astropy/django/scikit-learn/sphinx/sympy;
official testbeds + official scorer, `deepseek-flash`, prompt v3, N=1).
Not comparable to full-500 published numbers — smaller, five-repo mix;
see docs/eval.md.

## Principles

1. Never spend a model invocation on work deterministic software can do.
2. Never give the model context it does not need for its next decision.
3. The model decides *what* to do next; deterministic code decides
   everything that can be decided without it — context selection, tool
   scheduling, retries, validation, termination.

Capabilities move from model to software over time, never the reverse.

## Install

Requires Rust 1.89+ (edition 2024).

```sh
cargo install --git https://github.com/sadmansakib/loom loom-cli
```

From a checkout: `cargo install --path crates/loom-cli`.

macOS uses Seatbelt and Linux uses bubblewrap for sandboxed command
execution; on Linux install `bubblewrap` with your package manager first.

## Quickstart

```sh
# store a provider key once (OS keychain; never a config file)
loom auth login deepseek

# run a task in the current directory (one-shot, headless)
loom run --provider deepseek "fix the failing test and make cargo test pass"

# any OpenAI-compatible gateway
export CUSTOM_API_KEY=...
loom run --provider custom --base-url https://gateway.example/v1 \
    --model provider/model-name "your task"

# several gateways, each under its own name (URLs remembered)
loom provider add orcarouter --base-url https://api.orcarouter.ai/v1
loom run --provider orcarouter --model deepseek/deepseek-v4.1-flash-free "your task"

# structured events for tooling (NDJSON, event schema 11)
loom run --provider deepseek --json "your task" | jq 'select(.kind.type=="tool_completed")'

# past sessions live in SQLite; list and inspect them
loom sessions
loom stats

# offline counterfactual context replay: re-compile a recorded trace
# under different prune settings — zero model calls, token/cache cost
# only (never task success)
loom run --provider deepseek --trace traces/demo "your task"
loom replay-context traces/demo/<task-id> --set prune_trigger_percent=50
```

Sandboxed commands run with network off by default; escalation (network,
extra writable paths, higher limits) is declared per program in
`.loom/commands.toml`, and unrecognized programs can be allowed, asked
about, or denied via the `[policy]` verdict layer.

## What works

| Area | State |
|---|---|
| Turn loop | completion gate, one-shot refusals, zero-progress + late-budget nudges, malformed tool-call recovery |
| Tools | read/write/edit/list/search, sandboxed `run_command`, `verify` (local, or containerized/remote via `.loom/verify.toml`; opt-in `--autofix` applies rustc's `MachineApplicable` suggestions after a failing cargo verify under a never-worse gate), `git_status`/`git_diff`/`git_log` (opt-in `--git`), symbol tools (`--symbols`), skills (`--skills`), read-only scout (`--scout`), MCP servers (opt-in `--mcp`: stdio, allowlisted, hash-pinned, deferred loading), LSP semantic tools (opt-in `--lsp`: definition/references/symbols/hover/diagnostics/rename/format/call-hierarchy via known servers) |
| Safety | Seatbelt/bubblewrap, network off default, manifest escalation, allow/ask/deny verdicts, credential scrubbing, process-group kill |
| Context | repo facts, AGENTS.md instructions, pruning + turn-age masking, per-tool output budgets (env-sweepable), offline counterfactual replay (`loom replay-context`, self-validating against the recorded census) |
| Providers | DeepSeek, OpenRouter, GLM, NVIDIA, Groq, Anthropic + any OpenAI-compatible custom gateway |
| Persistence | SQLite sessions/events, resume, steering, `loom sessions`/`loom stats` |
| Embedding | `loom-core` facade (ADR-021), full drive loop in `crates/loom-core/examples/frontend.rs` |

Not built yet: interactive TUI/CLI surface (resume/steering/ask-mode need a
frontend; the engine seams exist). Parallel best-of-N worktree rollouts
ship opt-in (`--best-of N`, roadmap item 21); delegation economics at
N>1 remain a measurement question (roadmap item 19 trigger).

## Crates

| Crate | Purpose |
|---|---|
| `loom-kernel` | Domain model and traits (session, turns, tools, providers). No workspace dependencies. |
| `loom-providers` | Provider adapters: DeepSeek, OpenRouter, GLM, NVIDIA, Groq (OpenAI-compatible wire format) and Anthropic (messages API). |
| `loom-store` | SQLite persistence for sessions and events. |
| `loom-tools` | Built-in deterministic tools: file read/write/edit, search, listing, sandboxed command execution, git porcelain, symbols, skills. |
| `loom-engine` | The deterministic turn loop: context compilation, token estimation, pruning, repo intelligence, completion gate, scout. |
| `loom-core` | Facade re-exporting the public API for embedders. |
| `loom-eval` | SWE-bench Verified sweep orchestration + official-scorer reporting. |
| `loom-cli` | `loom` binary: auth, run, sessions, stats, tracing, signals. |

Dependency direction is enforced: `loom-kernel` depends on no workspace
crate, so provider- or tool-specific behavior cannot leak into the domain.
`loom-engine` builds on kernel, store, providers, and tools; `loom-core`
and `loom-cli` sit on top.

## Development

```sh
cargo build
cargo test
cargo clippy --all-targets
cargo fmt --check
```

Workspace lints: `unsafe_code` forbidden; `missing_docs`, `unwrap_used`,
`expect_used` warned.

Documentation: [`docs/architecture.md`](docs/architecture.md) (design),
[`docs/eval.md`](docs/eval.md) (eval suite protocol + honesty rules),
[`docs/daily-driver.md`](docs/daily-driver.md) (generalization probes),
[`docs/roadmap.md`](docs/roadmap.md) (vision, iterations, evidence
backlog).

## License

[MIT](LICENSE) — © 2026 Sadman Sakib.
