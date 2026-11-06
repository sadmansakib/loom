# Machine-applicable compiler autofix after failing verify (`--autofix`)

Status: design + implementation, 2026-11-06 (roadmap item 33, opt-in). The
feature is **local and free**: it applies rustc's own `MachineApplicable`
suggestions inside the workspace, re-runs the same fixed-table verify command,
and keeps the result only under a never-worse gate.

## Problem

When `verify` fails (standalone, or fused into `edit_file`/`write_file`), the
rendered evidence already names the file:line of each diagnostic — but for a
large class of mechanical errors the compiler also ships an **exact fix**
(`suggested_replacement` with `suggestion_applicability:
"MachineApplicable"`) that `cargo_diagnostics` throws away. The model then
spends the next turn re-deriving that exact fix: one model pass, one context
re-bill, for work rustc already did.

## Evidence (measured 2026-11-06, this machine)

**Stored sessions** (`~/.local/share/loom/loom.db`, 15 sessions, 14 verify
calls): only failing verifies are `cargo test` **assertion** failures (the
median exercise) — zero compile-error loops observed. Thin, as the plan
anticipated; the fixture fallback below is the sanctioned evidence source.
(Scan scripts: tempdir throwaways, methodology recorded here.)

**Real `cargo check --message-format=json` on 24 representative
agent-caused error classes** (independent temp crates, this toolchain):

- 25 error/warning diagnostics total, **9 carried a MachineApplicable
  suggestion (36%)**.
- Restricted to **errors** (what makes verify fail): 18 diagnostics, **4 MA
  (~22%)** — including the highest-frequency mechanical classes: E0596
  (needs `mut`), E0308 stray semicolon, E0369 `String + String`, E0384
  (assign to immutable).
- MA **warnings** (unused_mut, unused_imports, unused_variables) ride along
  in most failing outputs and are fixed by the same mechanism.

Verdict: **non-negligible** — roughly one compile failure in four or five
carries a deterministic fix. Not benchmaxxing: the claim is "rustc marks
these applicable," nothing about end-to-end solve rates.

## Options considered

1. **`rustfix` crate** — mature, but its own docs warn it targets Cargo's
   test-suite and may change; rejected per the G8 dependency rule for
   something an in-house applier does in well under 200 lines.
2. **Model-side prompt** ("apply the suggestion") — costs the very turn the
   feature saves; also unverified.
3. **In-house applier, opt-in, never-worse** (chosen) — parse the spans,
   apply byte-exact replacements, re-verify, revert unless the diagnostic
   count did not increase.

## Chosen design

**Opt-in.** `EngineConfig.autofix: bool`, default `false`, set by the CLI's
`--autofix`. The frozen SWE-bench protocol and the bare-loop arm never set
it, so the registry behaves byte-identically until a frontend opts in
(`bare_registry` reuses the built instances; the flag defaults off).

**Rust only, local runs only.** The applier runs when: the flag is on, the
ecosystem is Cargo, the verify run **failed** (non-zero exit), no container
runner was declared (a remote runner's filesystem is not locally writable —
honest skip, documented), and the raw stdout/stderr contains at least one
`MachineApplicable` span (top-level message spans and `children[]` spans
both count) whose `file_name` resolves **inside** the workspace
(`confinement::resolve_within`; registry deps under `~/.cargo` are refused).

**Apply.** Per file: sort spans by `byte_start`; skip any span overlapping
an already-accepted one in that file (conservative, per spec); skip spans
whose `byte_end` exceeds the file or that split a UTF-8 char boundary. Each
touched file gets a `BeforeImage` snapshot (`pre_image`, content-addressed)
**before** the first edit — the rewind precedent.

**Never-worse gate.** Re-run the *same* fixed-table command through the
same exec path. Count diagnostics with `cargo_diagnostics` (stdout, stderr
fallback) before and after. Keep the edits only if the count did not
increase AND the re-run executed; otherwise restore every snapshot and
report the revert. A fix that trades one error for two is reverted, even if
it was "correct."

**Telling the model (bounded).** The tool result keeps the original failure
evidence, then a `--- autofix ---` section: one line per applied fix
(diagnostic code + file), the before→after diagnostic counts, and the
re-verify verdict. ToolEffects records `files_written`, `before_images`,
and a `repairs` note — the `with_repair` precedent.

**Event.** `ToolEffects` gains an additive optional `autofix` field
(`#[serde(default)]`, so persisted old transcripts deserialize); the kernel
agent loop emits an additive `EventKind::AutofixApplied { edits, files,
diagnostics_before, diagnostics_after, kept }` when a result carries it.
`EVENT_SCHEMA_VERSION` 13 → 14 (a new enum tag is a wire change even when
additive).

**Seam placement.** New `crates/loom-tools/src/autofix.rs` owns parsing,
planning, applying, the gate, and restore. `VerifyTool` (after the local
sandbox render) and `fusion::run_fused` (after the fused render) call the
same `autofix::attempt`; edit/write tools carry the flag through so the
fused path is opt-in too. Best-of-N (`worktrees.rs`) builds its own
`VerifyTool::with_policy` — measurement stays mutation-free.

## Failure modes

- Stale byte ranges (file changed mid-flight): length/boundary checks skip
  the edit; the gate catches anything else.
- Conflicting fixes: overlap rule skips them rather than guessing.
- Multipart suggestions applied only partially: the never-worse re-run is
  the safety net — count increases ⇒ revert.
- Cargo emits a fix that silences one error but hides another: count equal
  or lower ⇒ kept; that is the contract, not a bug.
- Re-run fails to execute: revert everything, report honestly.

## What is deliberately absent

- No fixes beyond `MachineApplicable` — `MaybeIncorrect` /
  `HasPlaceholders` / `Unspecified` are never guessed.
- No non-Rust ecosystems (Go/Python/npm/make parsers unchanged).
- No autofix on the container-runner path, in best-of-N selection, or in
  the frozen eval protocol.
- No new dependencies.

## Measurement plan

Offline only: the tests below run real `cargo` in tempdirs. A funded A/B
(turns-to-green with/without `--autofix`) is deferred until the balance
allows; the event stream already carries `autofix_applied` counts, so a
future eval can attribute turns saved exactly.

## Kill / revisit criteria

If the event stream shows kept-fixes land but failing-verify turn counts do
not drop in real sessions, the feature is mis-predicting value — revisit or
remove. If rustfix's API stabilizes and grows diagnostics coverage beyond
the in-house applier's, revisit the build-vs-depend decision.
