# Changelog

All notable changes to Loom. Format loosely follows Keep a Changelog; this
project versions by iteration, not semver.

## 2026-10-06 — OpenRouter provider (free and paid models)

Second adapter, one shared wire. Loom now talks to OpenRouter alongside
DeepSeek; free and paid models travel the same path — pricing is data
(zero vs non-zero), never a branch. Commits `727783d`–`82e01a9`.

- **Cost reporting** (`727783d`): `Usage` gains `cost_usd: Option<f64>`,
  set when a provider reports actual spend (OpenRouter does, DeepSeek
  does not); `Add` merges it (Some+Some = sum). `ModelEvent::Completed`
  stays the sole usage authority; `EVENT_SCHEMA_VERSION` bumps to 2.
- **Shared wire driver** (`26bcf0b`): the OpenAI-compatible plumbing is
  extracted behind a `WireEndpoint` trait — `http` (timeouts, bounded
  error bodies, `Retry-After`), `openai_wire` (request shape, chunk
  accumulation, mid-stream error objects), `wire_stream` (SSE state
  machine). An adapter now owns only configuration and error
  classification, so streaming behaviour cannot drift between providers.
- **OpenRouter adapter** (`26bcf0b`): capabilities, pricing, and
  `parallel_tool_calls` resolve from the `GET /models` catalog
  (`supported_parameters`; unlisted = serialize calls, the safe
  direction). HTTP 402 classifies as permanent `QuotaExhausted` (exhausted
  credits block even free models), 429 as retryable `RateLimited`.
  Catalog reads are bounded at 16 MiB and UTF-8-validated under the idle
  timeout.
- **Provider factory** (`10d26ac`): `EngineConfig.provider` selects the
  adapter; the engine hands catalog-driven adapters a
  `ProviderCatalog` enum (data, not a provider-named field). It enforces
  the 64 K pruning floor (`ModelWindowTooSmall`) and fails loudly with
  `UnknownProvider`/`CatalogMissing` rather than falling back.
- **Frontend-owned catalog** (`387a075`): the CLI fetches and caches the
  catalog beside `loom.db` (`openrouter-models.json`, 24 h TTL); a fresh
  cache skips the network, and a fetch failure falls back to a stale
  cache so a transient outage cannot block a run.
- **Review pass** (`62ea5a6`): fixed a wrong `parallel_tool_calls`
  claim (every tool-capable model got parallel), bounded the catalog
  download, neutralized `EngineConfig`'s catalog field, and collapsed
  duplicate provider-name constants and clocks.
- **Auth consts** (`82e01a9`): `loom-engine` names providers via the
  shared constants (`DEFAULT_PROVIDER = DEEPSEEK_PROVIDER_NAME`) — one
  definition site per name.
- Docs (`fba8c7a`): architecture §9/§10 and the roadmap rewritten around
  the two adapters, the factory, and the frontend-owned catalog.
- `cargo test --workspace` 466 passed / 0 failed / 13 ignored (live
  free-model tests `#[ignore]`d behind `OPENROUTER_API_KEY`, mirroring
  `DEEPSEEK_API_KEY`); clippy and fmt clean.

## 2026-10-06 — Action fusion and a measured context mandate

Two token-efficiency changes, both measured on the eval harness (N=5,
deepseek-flash) before being kept.

- **Fused verification** (`011fd5a`): `write_file` and `edit_file` accept
  an optional `verify` argument (`"check"`/`"test"`) that runs cargo in
  the same call, so a change is proved in the turn that made it instead
  of a separate verify round-trip. The fused output carries the same
  structured failure evidence as the `verify` tool; effects record both
  the file write and the command. Tool-spec guards relaxed to 4,820 B
  total (still ~10 % under the pre-compaction size).
- **Context mandate, trimmed by measurement**: the default system prompt
  teaches exactly two rules — batch independent read-only calls in one
  turn (they run in parallel; never mix a mutation into the batch), and
  set `verify` on writes that touch compilable code. The first draft
  (~885 B) was measured at +31 % input on B2 and cut to ~310 B, which
  held B2 flat and improved B3 by 26 % (16.0 K → 11.9 K input, 6.6 → 5.4
  turns). The eval harness now sources B2–B4 prompts from the engine
  default instead of a frozen copy, so measured and shipped prompts
  cannot drift apart again.
- Verdict kept honest: fusion adoption on deepseek-flash is low (1 of 25
  mutating calls; B3 still prefers one raw `run_command` cargo call),
  and B2's +33 % vs its pre-fusion baseline is the fixed cost of the
  mandate plus two larger specs (~0.9 KB/turn) on a three-turn task.
  B1's unchanged path moved −11 % across the same cycles, putting the
  N=5 noise floor around ±10 %.

## 2026-10-06 — Context pruning: episodic delta-reclaim gate (cache safety)

Follow-up hardening to step 2L. Pruning must be genuinely episodic — one
cache miss per checkpoint, never per turn (ADR-011). `PruningContextCompiler`
previously re-ran `decide()` and adopted a fresh checkpoint on *every* turn
the estimate stayed above the trigger, even when a frozen checkpoint already
existed, rewriting earlier projected messages and busting the prefix cache
each turn.

- `crates/loom-kernel/src/context/pruning.rs`: a new checkpoint is adopted
  only when its reclaim exceeds, by at least `min_reclaim_tokens`, what the
  frozen stubs already reclaim (new `frozen_reclaim(session, stubs)` helper).
  A checkpoint that already covers the transcript is carried verbatim, so a
  session parked above the trigger stays byte-stable.
- New test `frozen_checkpoint_is_not_recomputed_while_parked_above_trigger`
  (fails before the change, passes after).
- `cargo test --workspace` 420 passed / 0 failed / 10 ignored; clippy and fmt
  clean.

## 2026-10-06 — Context pruning: window-relative trigger (step 2L retune)

The 2L long-context benchmark (task B5: read→edit over 8 corpus files)
showed the ADR-011 flat 64 K trigger bounds context but costs ~25 % more
under prefix caching: each checkpoint re-bills the live window at
cache-miss price. The trigger is now **window-relative** — `max(80 % of
the model's input window, 64 K)` — so automatic pruning stays dormant on a
large window and only bounds context as a task approaches the wall (commit
`6c225d9`).

- `PruneConfig` (`crates/loom-kernel/src/context/pruning.rs`):
  `trigger_tokens` → `trigger_percent` (80) + `floor_tokens` (64 K), with a
  `trigger_tokens(max_input_tokens)` helper; the strategy tests pin the old
  flat threshold via a `firing()` config, and a new test covers the
  threshold math.
- Eval harness: `LOOM_EVAL_PRUNE_TRIGGER_TOKENS` forces an absolute trigger
  for measurement.
- B5 N=5 per arm (deepseek-flash, off-peak): no-prune vs flat 64 K — final
  context 127.6 K → 35.7 K (−72 %), input −47.6 %, turns equal (18.6),
  0 failed calls, but cost +24.6 %; the window-relative default arm is
  identical to no-prune (pruning never fires at a 1 M window), and the
  forced-64 K arm reproduces the flat result. Record
  `docs/benchmarks/2L-2026-10-06.md`.
- `cargo test --workspace` 419 passed / 0 failed / 10 ignored; clippy and
  fmt clean.

## 2026-10-06 — Context pruning (ADR-011, step 2L)

`PruningContextCompiler` bounds a long task's projection without mutating
the transcript; measured on a new long-context task (B5) it cuts the final
context ~72 % but **costs more** under prefix caching. Record
`docs/benchmarks/2L-2026-10-06.md`.

- New `crates/loom-kernel/src/context/pruning.rs` wraps
  `DefaultContextCompiler` and applies four deterministic zero-model-call
  strategies in fixed order — dedupe identical read-only calls, supersede
  reads of later-modified paths, drop stale errors, age out unprotected
  read-only results — each replaced by a stub that keeps `call_id`.
  Protected: the tail (≥ 20 K tokens and ≥ last 3 turns), user items,
  assistant text, reasoning, the first user item. It fires only past
  64 K estimated input and ≥ 20 K reclaimable; decisions are frozen in an
  episodic `PruneCheckpoint` returned in `CompiledContext` and passed back
  via `ContextInput`, so the projection is byte-stable between checkpoints.
- Wired on by default in `loom-engine`; `LOOM_EVAL_PRUNE` toggles it in the
  eval harness. New heavy task **B5** (read → edit over 8 corpus files,
  `ToolSet::ReadEdit`, `LONG_CONTEXT_SYSTEM_PROMPT`); offline replay now
  mirrors the live DeepSeek capability table (the stale 128 K window it
  assumed could not replay B5's ~127 K transcript).
- Benchmark (N=5 per arm, deepseek-flash, off-peak): final context
  127.6 K → **35.7 K (−72 %)**, input −47.6 %, turns 18.6 = 18.6, 0 failed
  calls — but cost **+24.6 %** and `cache_miss` +38.7 %, because each
  checkpoint rewrites the prefix and re-bills the live window at cache-miss
  price. Automatic pruning is a window-safety mechanism, not a cost win;
  default disposition pending.
- `cargo test --workspace` 418 passed / 0 failed / 10 ignored; clippy and
  fmt clean.

## 2026-10-06 — Tool-spec compaction (step 2K)

The serialized tool specs are the dominant fixed cost of a short task's
context; they are now compacted without changing what any tool accepts
(commit `2c6dfee`).

- `schema::parameters` renders the parser's own struct schema minus the
  bytes the model does not need: `$schema`/`title` dropped,
  `definitions`/`$ref` inlined (single-element `allOf` collapsed),
  `default` and integer `format:"uint"` removed, an integral `minimum`
  normalised from `1.0` to `1`, and a `oneOf` of single-value string
  enums collapsed to one `enum` list. Descriptions and field docs were
  tightened across all eight tools.
- Schema↔parser agreement is unchanged (per-struct `agreement::check`
  still passes). Two new guards pin the result: `tests/measure_specs.rs`
  (≤ 720 B per spec, ≤ 4,400 B total) and a noise-free assertion in
  `tests/spec_schema.rs`.
- Serialized specs 5,339 → **4,222 B** (−20.9 %); the B2 seven-tool set
  4,535 → 3,568 B, so B2's final-turn `tool_specs` census drops 1,511 →
  **1,189 est tokens** (−21 %). B2 N=5: turns 3.4 → 3.0, input −22 %,
  cost −24 %, 0 failed calls; B1 is unchanged within sample variance.
- `cargo test --workspace` 410 passed / 0 failed / 10 ignored; clippy and
  fmt clean. Record `docs/benchmarks/2K-2026-10-06.md`.

## 2026-10-06 — Frontend-neutral engine boundary shipped (ADR-021, step 2I)

Three commits land the boundary designed below:

- **`loom-kernel`** — the former `loom-core`, renamed; the neutral loop/domain
  crate stays free of provider/tool/HTTP/SQLite dependencies (ADR-001).
- **`loom-engine`** (new) — the composition root and control plane every
  frontend shares: `EngineConfig` + credential resolution (`auth`, `paths`),
  one concrete provider, the built-in tool registry, session lifecycle +
  persistence, and the driving API (`Engine::{open,new_session,run_task,
  save_session}`, `EngineError`). No stdout/stderr and no terminal I/O.
- **`loom-core`** is now the **umbrella facade** — the single crate a frontend
  imports — re-exporting the kernel, the engine, the providers, and the tools,
  plus the driving API at its root. `loom-cli` depends on `loom-core` alone
  among workspace crates and is reduced to args, tracing, signals, rendering,
  and exit codes.

No behavior change. `cargo test --workspace` 288 passed / 0 failed; `cargo
clippy --workspace --all-targets` clean.

## 2026-10-06 — Frontend-neutral capability surface design (ADR-021)

Design decision. Loom must not be bound to the CLI: TUI, GUI, and server
frontends share one engine, reached in-process, instead of duplicating
credential, provider, tool, and persistence wiring.

- **ADR-021 accepted (revised the same day)** — **`loom-core` becomes the
  umbrella facade**: the single crate every frontend imports, re-exporting a
  renamed neutral kernel (`loom-kernel`, today's `loom-core`), a new
  `loom-engine` composition root (config + credentials, provider/tool
  composition, workspace defaults, session lifecycle/persistence, driving API
  + command channel), `loom-providers`, and `loom-tools`.
- **Bindings are in-process (zero latency):** a frontend links `loom-core` and
  calls the engine directly; events arrive through the in-process `EventSink`.
  The versioned serializable `EventEnvelope` is kept only for an optional
  out-of-process `loom-server`.
- `loom-kernel` preserves ADR-001 (no provider/tool/HTTP/SQLite). The earlier
  draft — a *separate* `loom-engine` boundary with frontends importing several
  crates — is recorded as a rejected alternative.
- Planned as plan-iteration-2 step **2I** (validation row V26); it is the
  **first** I2 step, so the harness (2A) composes provider/tools through the
  engine and the SQLite store (2C) is wired once inside `loom-engine`.
- architecture.md §1/§7/§12 updated with pointers.

## 2026-10-06 — Tool review follow-ups

Post-review hardening of the mutating tools and the failure policy
(commit `8da1431`); no intended behaviour change for the passing B2 path.

- `run_command` now captures stdout/stderr with a per-stream cap (64 KiB) and
  discards the remainder instead of buffering the whole child output.
- `edit_file` refuses files larger than 8 MiB before reading them.
- The failure `Hint:` is clamped to keep tool content within
  `MAX_TOOL_OUTPUT_BYTES`.
- `ToolStarted` / `ToolCompleted` are emitted only for calls that run; a
  refused repeat emits no tool events.
- Documented: a timeout kills only the direct child, not its process group
  (deferred to the sandbox/policy iteration); `ledger_key` depends on
  `preserve_order` staying off.
- Verification: loom-core lib 96 / agent_loop 16 / loom-tools 72 tests pass;
  clippy clean; B2 live 5/5 success, 0 tool-call failures.

## 2026-10-06 — Loop-owned tool-failure policy

**Zero tool-call failures required** for the DeepSeek benchmarks. The first B2
run recorded one failure that was not a real fault: listing a not-yet-existing
directory is a legitimate existence probe, and nothing stopped a failed call
from being repeated blindly.

- **ADR-020 accepted** — two layers. (1) `read_file` / `list_dir` / `search`
  return a *successful* informational result when the target does not exist;
  real faults (invalid input, permissions, invalid UTF-8, non-regular file,
  timeout) stay errors. (2) the agent loop keeps a per-task `FailureLedger`
  keyed by `(tool name, canonical arguments)`: an identical call that already
  failed is refused as the new `ToolFailure::RepeatedCall` without executing,
  and any **success** clears the ledger. Real failures get a deterministic
  next-step `Hint:`.
- Engine (`agent/tools.rs`) unchanged; no tool call is automatically retried.
- Guard tests added in `crates/loom-core/tests/agent_loop.rs`; `cargo clippy
  --workspace --all-targets` clean.

## 2026-10-06 — Iteration 1 completed (mutating tools)

**B2 passed end to end, 5/5 runs** — the second and last Iteration 1 success
criterion. `docs/benchmarks/B2-2026-10-06.md`.

- **ADR-019 accepted** — `write_file`, `edit_file`, `run_command` added to
  `loom-tools`; registered in the CLI; the system prompt now permits writes
  and commands. No engine change (mutating calls already serialize the batch).
  `ToolOutput`/effects deliberately unchanged — deferred to iteration 2 step
  2B/2G.
- `run_command` runs bare allow-listed programs only (no shell); a non-zero
  exit is a successful, model-visible result.
- The eval harness gains a generated fixture crate and `LOOM_EVAL_TASKS`
  selection; B2–B4 are now runnable (B1 keeps its 2026-10-06 baseline).
- `crates/loom-tools` unit tests: 70 passing. `cargo clippy --workspace
  --all-targets` clean.

## 2026-10-06 — Iteration 1 freeze lifted

**B1 passed end to end.** Six live runs (5× deepseek-flash, 1×
deepseek-v4-pro), all successful, zero retries, zero dropped events. Full
per-run records and aggregates: `docs/benchmarks/B1-2026-10-06.md`.

- **ADR-004 addendum** — implementation contract changes recorded under the
  §17 freeze rule (commit `cb6119d`): `TaskError::Internal` split from
  `Protocol` (internal defects are never retried); `events_dropped` is
  sampled after the terminal event (final-flush race fixed).
- **ADR-006 addendum** — `ContextInput` gains required `model: &'a str`
  (commit `8f1447c`): per-model budget policy requires the model id in the
  compiler's scope.
- **ADR-012 accepted** — Iteration 2 scope merged into a single definition
  (`docs/iteration-2.md`; architecture §18 now defers to it). Long-term
  memory and `session_search` move to Iteration 3. Three seams settled
  ahead of write tools: tool effects recorded at execution time,
  `ArtifactRef` out-of-context result storage, and **persist-then-commit**
  turn ordering (durable before in-memory, superseding the former §18.2
  wording).
- `docs/plan-iteration-2.md` completed: ordered steps 2A–2H; the evaluation
  harness (B1–B4, N ≥ 5 runs, record/replay provider, context census) is
  step 2A and gates all optimization work.
- `docs/iteration-2.md` rewritten as the merged single definition, updated
  with B1 evidence (input tokens 50–90× output; prefix-cache economics;
  run variance).

Iteration 1 contracts in `docs/architecture.md` §1–17 remain the baseline
that Iteration 2 extends.
