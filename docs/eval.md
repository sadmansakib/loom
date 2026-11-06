# The eval suite

Loom measures itself against the standard the field uses: **SWE-bench
Verified** (500 human-validated real GitHub issues), run inside the
official per-instance testbed containers and graded by the official
harness. The in-house corpus, outcome gates, replay proofs, editor
bench, and live benchmarks were removed (2026-10-20) — a self-mined
suite measures against Loom's own history, not against the world, and
its baselines live on in `CHANGELOG.md` as historical record.

## What runs

Each instance is one container:

```
image:   sweb.eval.x86_64.<instance_id>     (official testbed, repo @ base_commit)
inside:  /loom/loom run --json --no-save --provider … --model … \
             --max-turns … --workspace /testbed "$(cat /prompt.txt)"
patch:   cd /testbed && git add -A && git diff --cached > /out/patch.diff
```

The agent (the same `loom` binary users run, built for linux) works on
the repository checked out at the issue's `base_commit` with the
correct interpreter and dependencies already installed. The host only
orchestrates: it mounts the binary, the prompt, and an output dir into
the container and reads the results back. Nothing touches the host's
checkouts, python, or git state — a run cannot impact the machine it
runs on, and every run starts from the image's pristine `/testbed`.

The prompt is the frozen SWE-bench template (v3): repository name,
root-cause and scope-minimality rules (fix exactly the reported
defect; no new restrictions the issue didn't ask for), a
reproduce-and-verify rule (minimal repro script — the issue's own
example when it has one — failing nonzero before the fix, rerun and
checked against the issue's stated expected values after), the
working-tree contract (no commits, no touching `.git`), the verbatim
`problem_statement`, optional `hints_text`, and a request for a change
summary. The gold patch is never shown to the agent.

## Running a sweep

Prerequisites: Docker (OrbStack works; x86_64 images run under
Rosetta), the `swebench` CLI (`pip install swebench`), and an API key
for the provider being tested.

```bash
# 1. build the linux loom binary (once per loom version)
#    (static musl: testbed images ship older glibc than build hosts)
cargo run -p loom-eval --bin swebench-run -- --build-loom

# 2. sweep five instances, one run each
cargo run -p loom-eval --bin swebench-run -- \
  --provider deepseek --model deepseek-flash --max 5

#    a gateway sweep (any OpenAI-compatible endpoint, e.g. nano-llm):
#    CUSTOM_API_KEY=… cargo run -p loom-eval --bin swebench-run -- \
#      --provider custom --model <gateway-model-id> \
#      --base-url "$NANO_LLM_BASE_URL" --max 5

# 3. grade with the official harness (commands are printed by the sweep)
swebench eval verified -p target/eval/swebench/predictions-deepseek-flash-1.jsonl \
  --run-id loom-deepseek-flash-1 -j 4
```

On any host, missing testbed images are **pulled automatically** from
the swebench Docker Hub prebuilts (x86_64) under the scorer's own
lookup names and retagged to the local tags — pulled images are
scoring-ready with no extra step. Local builds are only the fallback
(offline hosts, or `--no-pull`); building every image up front is the
OOM-prone step this replaces:

```bash
swebench images build SWE-bench/swe-bench-tasks --dataset verified -i Django__Django-16379
```

The scorer looks images up under a rewritten name — Docker Hub forbids
dunders, so `astropy__astropy-12907` becomes
`swebench/sweb.eval.x86_64.astropy_1776_astropy-12907`. Images built
locally need a one-time alias before scoring (the sweep prints the
exact commands); pulled images already carry the scorer's name:

```bash
docker tag sweb.eval.x86_64.astropy__astropy-12907:latest \
  swebench/sweb.eval.x86_64.astropy_1776_astropy-12907:latest
```

Validate the scoring pipeline end-to-end before trusting any number:
the gold patches must resolve their own instances.

```bash
swebench eval verified --gold -i sympy__sympy-20590 --run-id validate-gold
```

## Knobs

| flag | default | meaning |
|---|---|---|
| `--instances a,b` | — | explicit instance ids, in order (a typo aborts) |
| `--repos org/repo,…` | — | filter the dataset by repository |
| `--max N` | 5 | cap when no ids/repos given |
| `--per-repo N` | — | instead of `--max`: first N instances of each `--repos` repo (stratified sampling) |
| `--runs N` | 1 | independent runs per instance (fresh container each) |
| `--jobs N` | 1 | instances in parallel (per-instance dirs and container names cannot collide) |
| `--no-pull` | — | never pull missing testbed images; skip the instance with the local build command printed |
| `--provider P` | `$LOOM_EVAL_PROVIDER` or `deepseek` | provider, resolved by the in-container CLI |
| `--model M` | `$LOOM_EVAL_MODEL` or `deepseek-flash` | model id |
| `--base-url URL` | `$LOOM_EVAL_BASE_URL` | chat API base URL for OpenAI-compatible gateways (`custom` provider, e.g. nano-llm); unset for vendors with default endpoints |
| `--loom-bin PATH` | `<eval-dir>/loom-linux/loom` | prebuilt linux binary |
| `--build-loom` | — | build the binary in a rust docker container |
| `--timeout-secs N` | 3600 | per-run wall clock (container is killed after) |
| `--max-turns N` | 100 | agent turn budget (v2 default; 60 starved one run into exploration-only) |
| `--eval-dir DIR` | `$LOOM_EVAL_DIR` or `target/eval` | artifacts root |

`LOOM_EVAL_SWEBENCH_INSTANCES` / `LOOM_EVAL_SWEBENCH_REPOS` are the env
forms of the id/repo filters. The API key is read from the provider's
standard env var (`DEEPSEEK_API_KEY`, `CUSTOM_API_KEY`,
`OPENROUTER_API_KEY`, `GROQ_API_KEY`, `ZAI_API_KEY`, `NVIDIA_API_KEY`,
`ANTHROPIC_API_KEY`) on the host and forwarded into the container; no
keychain lookup happens inside.

## Dataset and artifacts

The dataset is fetched once from HuggingFace
(`princeton-nlp/SWE-bench_Verified`) through the datasets-server API
and cached at `<eval-dir>/swebench/verified.jsonl`; the fetch
cross-checks the advertised row count (500) so a truncated cache can
never masquerade as complete. `LOOM_EVAL_SWEBENCH_JSONL` points at a
manual file instead. Instance selection never shrinks silently:
explicit ids must all resolve, in the order given.

Every sweep writes under `<eval-dir>/swebench/`:

- `instances/<slug>/prompt.txt` — the frozen prompt, once per instance;
- `instances/<slug>/run-<n>/run.json`, `run.err`, `patch.diff`,
  `patch.err` — per-run container output (the patch is extracted even
  when the agent fails, so partial work is scored honestly). Since
  event schema 11 the JSONL events also carry forensics: `tool_started`
  includes a 2,000-byte `arguments` head, `tool_completed` includes
  `status` plus a 1,000-byte `content_head` (covering the `$ cmd` and
  `exit status:` lines), and every loop-owned nudge (zero-progress,
  late-budget, both completion refusals) is a `loop_nudge` event —
  transcript greps no longer miss what the model actually ran;
- `predictions-<model>-<n>.jsonl` — one row per instance, the official
  format; the scorer keys by `instance_id`, so repeated runs write
  per-run files and never duplicate rows;
- `swebench-sweep-<model>.json` — per-instance runs, exit codes, patch
  sizes, token usage, the predictions-file inventory, and a
  `provenance` block: the sha256 of the staged linux `loom` binary
  (also kept alongside as `loom.sha256`), the digest of the frozen
  container script, the prompt version, and the runner's version. Each row's
  `usage` carries `input_tokens`, `cached_input_tokens`,
  `output_tokens`, `model_requests`, and `hit_rate` summed from the
  run's `model_completed` events (the deepseek adapter parses
  `prompt_cache_hit_tokens`; gateways that report no cache split leave
  `cached_input_tokens`/`hit_rate` null). The sweep also prints a
  one-line cache summary (`cache: N/M prompt tokens served from cache
  (X%)`) — provider prefix caching is the cost lever on long agentic
  sweeps, so the projection keeps history append-only to preserve it.

An empty patch row is legitimate output (the agent changed nothing);
the official scorer counts it unresolved. The `resolved` verdict comes
only from `swebench eval` — Loom never grades its own work locally, so
no local gate can be tuned to flatter the number.

## Suite audit (UTBoost-style weak-suite flags)

UTBoost (arXiv 2506.09289) amplified SWE-bench's own suites and found
345 erroneously-passing patches — 24.4% of Verified leaderboard entries
affected — so the frozen subset's official P2P/F2P verdicts are
hypotheses, not ground truth. The audit is a one-off deterministic
re-grade of the frozen 100 with amplified suites
(`docs/lit-review-2026-11-harness-papers.md` maps the paper):

```
cargo run -p loom-eval --bin suite_audit -- \
  --dataset <eval-dir>/swebench/verified.jsonl \
  --out <eval-dir>/suite-audit-v3-100 \
  --repos astropy/astropy,django/django,sympy/sympy,scikit-learn/scikit-learn,sphinx-doc/sphinx \
  --per-repo 20
```

Selection mirrors `swebench-run` (same `select_instances`), so the
audit covers exactly the frozen subset. Two passes, no model and no
network in the binary:

1. **Structural (offline)** — `suite_audit.json` rows flag instances
   whose suite shape is weak: `suite_small` (`f2p + p2p <= 2`) or
   `no_new_tests` (the test patch adds no test-looking lines); either
   firing sets the `weak_ids` hypothesis flag. The artifact's
   `provenance` block (method version, dataset sha256, selection rule,
   runner version) makes every verdict attributable, same rule as sweep
   reports.
2. **Mutant re-grade (docker, one-off)** — `mutants/mutant-<k>/`
   predictions files hold every instance's hunk-`k`-dropped gold patch;
   scoring each with the official harness (commands in the emitted
   `README.md`) and intersecting the resolved sets per instance applies
   the weak-confirm rule: an instance whose every mutant still resolves
   has a suite that cannot catch a partial fix. Single-hunk gold
   patches (`unmutable_ids`) get no verdict either way.

Re-gate rule: any headline derived from the frozen subset that flips
when weak-confirmed instances are excluded (or re-scored against
amplified suites) is re-gated — recorded as a new measurement with its
own provenance, never silently restated.

## Honesty rules

- The headline is the official `resolved` rate over the sweep — nothing
  else. Per-run cost/turn counts stay in `run.json` for context but do
  not feed the verdict.
- The prompt and container script are frozen; changing either
  invalidates every previously recorded number.
- A run killed by the wall clock still has its patch extracted and
  scored — a timeout is an honest outcome, not a hole in the sample.
- Baselines go in `CHANGELOG.md` with the sweep JSON alongside.
- One headline, one binary: a sweep's batches must run the same engine
  build, and the sweep JSON plus its official scorer report stay in the
  eval dir (or the repo) for as long as the number is quoted — a
  headline reconstructible only from prose is not a baseline. The
  harness enforces the first half mechanically: every sweep report
  records the staged binary's sha256, `swebench-run` refuses to extend
  an eval dir whose recorded sweeps ran a different build, and
  `swebench-contrast` refuses to contrast across builds.
- Subset composition travels with the number: repo list, dataset rows,
  runs-per-instance, and model are part of the claim, not footnote
  material — a 100-of-500 five-repo subset must not read as a full-set
  score.
