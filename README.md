# Loom

[![Status: Early release](https://img.shields.io/badge/status-early%20release-blue)]()
[![Tests: 1659](https://img.shields.io/badge/tests-1659-brightgreen)]()
[![SWE-bench Verified: 90/100 frozen subset](https://img.shields.io/badge/SWE--bench%20Verified-90%2F100%20frozen%20subset-blue)]()

> **Early release.** The headless agent core is usable today: the `loom`
> binary runs tasks end to end against real providers, bare `loom` opens
> a live terminal UI (workflow parity is still in progress), and
> `loom-core` exposes the embedding facade. `ask` policy verdicts fail
> closed in the CLI, but ACP clients (e.g. Zed) answer them over
> `session/request_permission` via `loom-acp`. APIs version by iteration,
> not semver.

Loom is a headless, provider-agnostic **coding-agent core** written in Rust.
The deterministic loop is the controller; the model is only the reasoning
component.

Design goal: **maximum successful work per model invocation with minimum
context and turns.**

Measured: **90/100 resolved** on a frozen 100-instance SWE-bench Verified
subset (first 20 dataset rows of astropy/django/scikit-learn/sphinx/sympy;
official testbeds + official scorer, `deepseek-flash`, prompt v3, N=1).
Not comparable to full-500 published numbers — smaller, five-repo mix;
see docs/eval.md. Two caveats the badge cannot carry: the sweep spanned
two engine builds (recorded in CHANGELOG when discovered — the
one-headline-one-binary rule and its build-digest enforcement postdate
it), and the sweep JSON/predictions that would reproduce it were never
published, so treat 90/100 as a historical baseline of that build and
prompt, not a claim about head. The prompt has since moved to v5;
re-measuring head requires a fresh sweep.

## Principles

1. Never spend a model invocation on work deterministic software can do.
2. Never give the model context it does not need for its next decision.
3. The model decides *what* to do next; deterministic code decides
   everything that can be decided without it — context selection, tool
   scheduling, retries, validation, termination.

Capabilities move from model to software over time, never the reverse.

## Install

Requires Rust 1.100+ (edition 2024).

```sh
cargo install --git https://github.com/sadmansakib/loom loom-cli
```

From a checkout: `cargo install --path crates/loom-cli`.

Supported host platforms are macOS and Linux. macOS uses Seatbelt and
Linux uses bubblewrap for sandboxed command execution; on Linux install
`bubblewrap` with your package manager first. Windows support is not
currently available.

## Quickstart

```sh
# store a provider key once (OS keychain; never a config file)
loom auth login deepseek

# run a task in the current directory (one-shot, headless)
# --model is required on run/chat: loom picks no model for you
loom run --provider deepseek --model deepseek-flash \
    "fix the failing test and make cargo test pass"

# keep one session open and steer it: text while a task runs queues at
# the next turn boundary; /rewind, /stats, /new, /sessions work in it
loom chat --provider deepseek --model deepseek-flash

# terminal UI: bare `loom` opens it; /sessions /context /tools
# /account /compare /clear /clear-history work at the prompt
loom

# add --bg-commands to detach long builds: their completion notice
# steers back in at the next turn boundary (loom chat/run)
loom chat --provider deepseek --model deepseek-flash --bg-commands

# any OpenAI-compatible gateway
export CUSTOM_API_KEY=...
loom run --provider custom --base-url https://gateway.example/v1 \
    --model provider/model-name "your task"

# several gateways, each under its own name (URLs remembered)
loom provider add orcarouter --base-url https://api.orcarouter.ai/v1 \
    --header 'X-Title=loom' # attribution headers ride on every request
loom run --provider orcarouter --model deepseek/deepseek-v4.1-flash-free "your task"

# structured events for tooling (NDJSON, event schema 23)
loom run --provider deepseek --model deepseek-flash --json "your task" \
    | jq 'select(.kind.type=="tool_completed")'

# past sessions live in SQLite; list them, inspect per-tool quality
loom sessions
loom stats [--json]

# time travel: rewind a session to an earlier index and optionally
# restore the workspace to that turn's recorded state
loom rewind <session-id> --at 12 --restore-workspace

# distill recurring fixes into .loom/lessons.json, injected on later
# runs via --lessons
loom lessons

# offline counterfactual context replay: re-compile a recorded trace
# under different prune settings — zero model calls, token/cache cost
# only (never task success)
loom run --provider deepseek --model deepseek-flash --trace traces/demo "your task"
loom replay-context traces/demo/<task-id> --set prune_trigger_percent=50

# workspace manifests (.loom/commands.toml, verify.toml, mcp.toml) load
# in full only once their SHA-256 is pinned by the operator
loom trust            # review + pin
loom trust --status   # show what is pinned

# editor integration: run loom as an Agent Client Protocol server
# (Zed: add as an ACP agent; JSON-RPC on stdio, ask prompts surface
# in the editor)
loom-acp
```

Sandboxed commands run with network off by default; escalation (network,
extra writable paths, higher limits) is declared per program in
`.loom/commands.toml`, and unrecognized programs can be allowed, asked
about, or denied via the `[policy]` verdict layer.

## Features

### Deterministic turn loop

- Completion gate: a task ends only when the model's own stated
  completion criteria hold, not when it stops talking.
- Deterministic budget envelope (total → input → output → tool calls →
  wall time); the unit in flight when a ceiling is crossed still
  completes. `--max-turns` (default 50), staged turn-budget extension.
- One-shot refusal of impossible tasks; zero-progress and late-budget
  nudges that inject course corrections instead of burning turns.
- Malformed tool-call recovery and a stop-reason protocol
  (`EndTurn`/`ToolCalls`) enforced before anything is committed.
- Exact-repeat tool calls are refused (`RepeatedCall`) — "try the same
  thing again" becomes a compile error for the model.
- Persist-then-commit ordering: the SQLite store is always at least as
  far as memory, so a crash is always recoverable; a single-writer
  session lease keeps two loom processes from interleaving writes.

### Tools

- Always on: `read_file`, `write_file`, `edit_file`, `list_dir`,
  `search`, `run_command` (sandboxed), `verify`, `artifact_read`.
- `verify` runs the fixed per-ecosystem build/test command locally, or
  containerized/remote via a `.loom/verify.toml` runner; it is fused
  into write/edit so file changes carry their own verification result.
- Opt-in `--autofix`: after a failing cargo verify, apply rustc's
  `MachineApplicable` suggestions under a never-worse gate.
- Git porcelain: `git_status`/`git_diff`/`git_log` (`--git`).
- Symbol tools: `symbols_overview`, `replace_symbol` (`--symbols`):
  tree-sitter symbol-level reads and whole-definition rewrites for
  Rust, Python, and TypeScript/TSX.
- Skills (`--skills`): curated playbooks from `.agents/skills/<name>.md`
  or `<name>/SKILL.md`, loaded on demand via `load_skill`.
- Background commands (`--bg-commands`): `run_background` +
  `command_status` detach long builds; completion steers back in at the
  next turn boundary.
- MCP servers (`--mcp`): stdio servers declared in `.loom/mcp.toml` —
  allowlisted, tool-definition-pinned in `.loom/mcp.lock`, loaded
  lazily via `mcp_search_tools`/`mcp_load_tools`.
- LSP semantic tools (`--lsp`): definition, references, document and
  workspace symbols, hover, diagnostics, call hierarchy, rename, format
  — via known servers (rust-analyzer, gopls, clangd, pyright,
  typescript-language-server, zls, and more; overrides in
  `.loom/lsp.toml`).
- Run profiles (`--read-only`, or `/profile` before the first TUI task):
  filter the engine registry to read-only tools, so writes, edits, shell
  commands, and deferred MCP tools are never exposed; the header shows
  `read-only`.
- Scout subagent (`--scout` + `--scout-model`): a cheap read-only child
  agent answers focused workspace questions; optional cascade
  (`--scout-escalation`/`--scout-escalation-model`) retries on a
  stronger model and returns both answers, labeled.

### Zero-model fast paths

Rename, single-site constant replacement, format, and Cargo-only
dependency updates execute deterministically with no model call at all;
every decline is surfaced (`FastPathDeclined` events carry the reason)
so the shortcut set can be audited, not guessed at.

### Context management

- Repository facts scanned once per task; workspace `AGENTS.md` /
  `CLAUDE.md` ride that scan, global instructions come from
  `~/.config/loom/AGENTS.md` (skipped via `--no-global-instructions`).
- Pruning, turn-age masking, and phase-aware retention compile each
  request from durable state — history is never mutated, only viewed.
- Handoff compaction (default on) hands a fresh continuation the
  distilled state when the window fills.
- Per-tool output budgets (env-sweepable) keep one chatty tool from
  eating the window.
- `loom replay-context`: offline counterfactual replay re-compiles a
  recorded trace under different prune/mask settings — zero model
  calls — and self-validates against the trace's recorded census.
- Server-side context editing hint for providers that support it
  (Anthropic slice, default off).

### Model routing and delegation

- `--route-easy` / `--route-hard`: route unambiguous read-only
  questions to a cheaper model and long build-shaped tasks to a
  stronger one, on the same provider.
- `--stall-escalation-model`: a task that stalls past a nudge is
  retried on a fork of the session with a stronger model.
- `--reasoning-effort` (default `high`) is sent explicitly on every
  request.
- Best-of-N (`--best-of N`, 2–8): parallel worktree rollouts with
  prefix-fork (`--best-of-prefix-turns`), variation
  (`--best-of-vary`), and execution-based selection; opt-in while
  delegation economics at N>1 remain a measurement question.

### Safety

- Seatbelt (macOS) / bubblewrap (Linux) sandbox for every
  model-driven subprocess; network off by default.
- Per-program escalation in `.loom/commands.toml` (timeouts, output
  caps, writable paths, `network`, `env_allow`) under an
  allow/ask/deny `[policy]` verdict layer; the deny list is
  argv-aware (exec wrappers cannot smuggle a denied program).
- Manifest trust gate: `.loom/*` manifests load in full only when
  their SHA-256 is pinned in `~/.config/loom/trust.toml` via
  `loom trust`; unpinned or edited manifests load with every
  escalation stripped (or refuse to run, for the unsandboxed verify
  runner).
- Credential scrubbing (API keys never reach child environments),
  `kill_on_drop` process-group kills so grandchildren cannot outlive
  a timeout, host-git filter hardening, atomic writes that refuse to
  follow planted symlinks.

### Terminal UI

- Bare `loom` opens the live frontend: one thread per task, streaming
  turns, follow-up steering at the next turn boundary, cancel, retry
  with backoff, and a completed-run record.
- Slash-first palette: `/model`, `/effort`, `/context`, `/tools`,
  `/account`, `/compare`, `/sessions`, `/new`, `/clear`,
  `/clear-history`, `/compact`, `/retry`, `/copy`, `/export`,
  `/reasoning`, `/sidebar`, `/profile`, `/help`, `/quit`.
- Live batch progress: every tool call shows queued, active with
  elapsed time, or finished; the batch header counts active, queued,
  and done calls, and cancellation settles interrupted calls honestly.
- Sidebar telemetry: task status, turn/tool/wall budgets, context
  census, cost, retry policy, and events. Always on at 120+ columns;
  Tab opens it at 90-119.
- `F1` help publishes the full key map: task control, approval
  answers, scrolling, palette and picker keys, screen shortcuts, and
  every slash command.
- Sessions, tools, accounts, compare, and context screens open over the
  dimmed live view; `/export` writes the task record as NDJSON plus a
  manifest under `.loom/exports/`.
- Truecolor, 256-color, and 16-color fallbacks adapt to `COLORTERM` /
  `TERM` (`NO_COLOR` drops color); `LOOM_ASCII=1` switches to the ASCII
  glyph set. Mouse-wheel scroll and bracketed paste work.

### Persistence and introspection

- One SQLite database (`loom.db`; `LOOM_DB` / `LOOM_DATA_DIR` /
  XDG paths) for accounts, sessions, turns, artifacts, and forks.
- Resume any session (`loom run --session ID`); live steering in
  `loom chat`; fork lineage shown in `loom sessions`.
- `loom rewind SESSION [--at IDX] [--restore-workspace]`: fork from an
  earlier turn and restore the workspace through a tiered, integrity-
  checked pre-image replay.
- `loom stats`: per-tool quality, skill retire/watch verdicts,
  `--json`, and trace mode over exported `--trace` directories.
- `--json` NDJSON event stream (schema 23) for building your own
  frontends; `loom-core` exposes the same engine for embedding, with
  a full drive loop in `crates/loom-core/examples/frontend.rs`.

### Editor integration

`loom-acp` speaks the Agent Client Protocol over JSON-RPC on stdio
(roadmap item 37): Zed and other ACP clients drive loom as an external
agent — `session/load` resume, `session/cancel` hard stop, engine
events mapped to `session/update`, and policy `ask` verdicts bridged
to `session/request_permission`. Client-forwarded MCP servers attach
only under the explicit `--allow-client-mcp` operator gate.

### Providers

DeepSeek, OpenRouter, GLM, NVIDIA, Groq, Anthropic, and `custom` —
any OpenAI-compatible endpoint via `--base-url`. Named gateways
registered with `loom provider add` remember their URL, models URL,
and attribution headers; credentials live in the OS keychain, never a
config file.

### Evaluation harness

`loom-eval` ships the measurement machinery: SWE-bench Verified sweeps
in official per-instance testbed containers under the official scorer
(`swebench-run`), contrast runs (`swebench-contrast`), a mutant
re-grade suite audit (`suite_audit`), the 43-task Rust slice of
SWE-bench Multilingual (`multilingual-run`), and the offline
self-optimization loop over sweep artifacts (`selfopt`). Runs are
integrity-guarded: the testbed's git history is destroyed before the
agent starts and trajectories are scanned for fix-mining afterwards
(`integrity_flags` in every report). See docs/eval.md for the protocol
and its honesty rules.

## Not built yet

The argument-provenance gate (roadmap item 38) and repro-test
tie-breaker (item 39) ran their evidence gates and are deferred.
Delegation economics at best-of-N > 1 remain a measurement question
(roadmap item 19 trigger).

## Crates

| Crate | Purpose |
|---|---|
| `loom-kernel` | Domain model and traits (session, turns, tools, providers). No workspace dependencies. |
| `loom-providers` | Provider adapters: DeepSeek, OpenRouter, GLM, NVIDIA, Groq (OpenAI-compatible wire format) and Anthropic (messages API). |
| `loom-store` | SQLite persistence for sessions, artifacts, auth accounts. |
| `loom-tools` | Built-in deterministic tools: file read/write/edit, search, listing, sandboxed command execution, git porcelain, symbols, skills, MCP, LSP. |
| `loom-engine` | The deterministic turn loop: context compilation, token estimation, pruning, repo intelligence, completion gate, scout, routing. |
| `loom-core` | Facade re-exporting the public API for embedders. |
| `loom-eval` | SWE-bench Verified sweep orchestration + official-scorer reporting; SWE-bench Multilingual Rust-slice runner; selfopt. |
| `loom-acp` | Agent Client Protocol server for editors (`loom-acp`): the engine over JSON-RPC on stdio. |
| `loom-tui` | Interactive terminal frontend: live task thread, sidebar telemetry, slash palette, and modal screens. |
| `loom-cli` | `loom` binary: run, chat, TUI, auth, provider, sessions, rewind, stats, lessons, replay-context, trust, tracing, signals. |

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
[`docs/wireframes/DESIGN.md`](docs/wireframes/DESIGN.md) (TUI design
language),
[`docs/threat-model.md`](docs/threat-model.md) (trust boundaries,
attacker model, defenses),
[`docs/roadmap.md`](docs/roadmap.md) (vision, iterations, evidence
backlog).

## License

[MIT](LICENSE) — © 2026 Sadman Sakib.
