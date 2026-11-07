# SWE-bench Multilingual — Rust slice runner (design)

Feature 4. A parallel eval path that runs loom on the 43 Rust tasks of
SWE-bench Multilingual with the same isolation model as the frozen
Verified sweep, plus the runbook for a future live sweep. No live sweep
in this feature; deliverables are code, offline tests, and the runbook.

## Verified facts (all checked 2026-11-06)

| Claim | Measurement | Source |
| --- | --- | --- |
| Dataset size | 300 rows, 1 config (`default`), 1 split (`test`), 15 columns | HF datasets-server `/size`, `/splits` |
| Schema | no `language` column; per-row `image` carries the exact Docker Hub name; `log_parser` per ecosystem (19 values); `eval_type` (1 value: `pass_and_fail`) | datasets-server `/first-rows` + full 300-row fetch |
| Repos | 41 distinct (the website's "42 repositories" overcounts by one) | full-row fetch, `repo` distinct count |
| Rust subset | **43 tasks / 7 repos**: tokio-rs/tokio 9, sharkdp/bat 8, tokio-rs/axum 7, astral-sh/ruff 7, nushell/nushell 5, uutils/coreutils 5, burntsushi/ripgrep 2 | full-row fetch, repo ∈ Rust set; matches the official Appendix A table |
| Rust parser | all 43 rows `log_parser = parse_log_cargo` | full-row fetch |
| Prebuilt images | manifest 200 for one image per Rust repo under `swebench/sweb.eval.x86_64.<id-with-__→_1776_>:latest` | Docker Hub registry API, 7/7 spot-checked (the `image` column names them exactly; no derivation needed) |
| Scorer | installed `swebench` CLI: `swebench eval multilingual -p preds.jsonl --run-id X -j N` (`multilingual` → `SWE-bench/SWE-bench_Multilingual`); `swebench images check` exists for preflight | `swebench eval --help` on this machine |
| Baseline anchor | SWE-agent + Claude-3.7-Sonnet: Rust 58.14% (25/43, highest language), $2.50/task cost cap | swebench.com/multilingual.html; already in `docs/benchmarks/published-anchors.md` |

Also verified: `hints_text` empty for 23/43 Rust rows (prompt already
handles optional hints); image-name shape is uniform across all 300 rows
(`swebench/sweb.eval.x86_64.<slug>:latest`, 0 shape violations).

## Slice definition

`repo ∈ RUST_REPOS` where `RUST_REPOS` is the sorted 7-repo constant
above. Selection is deterministic (dataset order) and gated by
integrity checks that abort before any spend:

- fetched rows == 300 (dataset drift guard, mirrors the Verified
  `EXPECTED_ROWS` check),
- Rust-filtered count == 43,
- every selected row has `log_parser == "parse_log_cargo"` and an
  `image` matching the registry shape above.

Provenance records: dataset id, dataset JSONL sha256 (cache file),
selection rule (`rust-repos-v1` + the 7 repos), counts, and method —
same fields as the Verified provenance block.

## Architecture

New code only; the Verified path is untouched:

- `crates/loom-eval/src/multilingual.rs` —
  `MultilingualInstance` (classic fields + `image`, `log_parser`,
  `eval_type`, `version`; reuses the `string_or_vec` deserializer made
  `pub(crate)`), `fetch_dataset_multilingual` (datasets-server rows
  API, paged, row-shape drift checks — mirrors `fetch_dataset_verified`
  without sharing its internals), `select_rust_rows` + integrity
  checks, `RUST_CONTAINER_SCRIPT`, and slice provenance helpers.
- `crates/loom-eval/src/bin/multilingual_run.rs` — mirrors
  `swebench_run.rs`: dataset cache/fetch, selection (`--instances`/`
  --repos`/`--max`/`--per-repo` on top of the Rust slice), image =
  the row's `image` pulled directly (it already is the scorer's lookup
  name, so **no retag step**), one container per (instance, run),
  predictions JSONL per run, `multilingual-sweep-<model>.json` report
  with provenance, scoring hint `swebench eval multilingual -p …`.
- `swebench.rs` gets one internal refactor: `build_prompt` keeps its
  signature and bytes but delegates the text assembly to a
  `pub(crate)` inner fn the Multilingual path calls with its own
  fields. The byte-frozen prompt is unchanged (existing pinning tests
  stay green); no other Verified behavior moves.

### The Rust container script

`RUST_CONTAINER_SCRIPT` is the Verified `CONTAINER_SCRIPT` with four
added `[[command]]` entries — `cargo`, `rustc`, `rustup`, `cargo-fmt`
— ahead of the same `/loom/loom run` invocation and patch extraction.
It is written as an explicit frozen constant (not spliced at runtime),
introduced frozen, and pinned by sha256 in a test that also pins the
Python script, so both scripts are covered by one guard. Python/pip
entries stay: they are inert where absent and ruff's testbed uses
Python-side tooling. Isolation model is unchanged — the container is
the boundary, sandbox passthrough, manifest removed before the diff.

### Prompt

The frozen v5 prompt is language-neutral ("a real issue in a GitHub
repository", run the project's tests) and is reused **byte-identical**.
Provenance records the same `prompt_version` tag; the dataset differs,
which is the point — same contract, new language.

## Offline tests

- fixture JSONL (2 synthetic Rust rows + 1 non-Rust + 1 bad-image row):
  parse, filter, integrity gates abort on 299/42-rows and non-cargo
  parser, image-shape validation rejects the bad row;
- `RUST_CONTAINER_SCRIPT` pinned by sha256 and structurally (contains
  the cargo toolchain entries, same loom invocation tail as the Python
  script — the "pinning both" test);
- prompt reuse: `build_prompt` on a Verified fixture and the inner fn
  on equivalent Multilingual fields produce identical text;
- predictions JSONL row shape `{instance_id, model, patch}`;
- provenance block fields present on a synthetic report;
- no test touches the network or docker.

## Runbook summary (full version in docs/eval.md)

Pilot (1 instance, ~1 h wall, cents on flash pricing) before anything
wider; the pilot's measured `run.json` usage × DeepSeek list price is
the cost datum the plan requires — no invented per-instance number
(no DeepSeek sweep history exists on this machine to reuse). Why Rust
costs more than Python per task: compile-verify cycles mean more
tool-turns and longer prompts per fix; the published $2.50/task cap is
the upper anchor. Go/no-go gates and the full command sequence live in
the runbook.

## Residual risks / open checks for the pilot

- Testbed network: Rust testbeds were validated with their deps
  preinstalled, but if `cargo test` inside a container needs registry
  access and has none, the pilot surfaces it in minutes.
- x86_64-only prebuilts (same constraint as Verified; arm64 hosts pass
  `--platform linux/amd64`).
- The scorer's registry-trust caveat (`--task-repo` exists if a
  locally-built image set is ever needed).
