# SWE-bench N=5 full-vs-bare contrast — gpt-6-luna (2026-11-07)

First A/B at the maintenance bar (N ≥ 5 medians) for the loop-shaping
claims (roadmap items 4/5/6/8/10, G4/G5). Pays down the re-baseline
debt recorded 2026-10-23.

## Vehicle

- Provider: named OpenAI-compatible gateway (LiteLLM, paid; name withheld),
  model `aws-bedrock/openai.gpt-6-luna` ($0.22/M in, $0.82/M out, $0.02/M cache-read)
- 5 frozen instances: django-11239, sphinx-7748, scikit-learn-14141,
  sympy-13974, astropy-12907 (the 2026-10-23 nano-gateway probe set)
- N=5 runs/arm, `--jobs 2`, `--max-turns 100`, official SWE-bench
  Verified scorer per run; **both arms share one musl `loom` binary**
  (`--loom-bin` reuse: identical `loom_sha256` in both sweep reports)
- Arms: `full` (facts, pruning, evidence shaping, completion gate) vs
  `bare` (`EngineOverrides::bare_loop()`)
- Artifacts: `target/eval/n5-full/`, `target/eval/n5-bare/` (sweeps,
  predictions-1..5, -selected); scorer reports `aws-bedrock__*.loom-n5-*.json`

## Results

| Arm | Runs resolved | Median | Mean | Patched | Requests | Prompt tokens | Cache |
|-----|---------------|--------|------|---------|----------|---------------|-------|
| full | 3,3,3,3,3 | **3** | 3.0 | 25/25 | 834 | 15.33M | 95.2% |
| bare | 3,3,4,3,4 | **3** | 3.4 | 24/25 | 445 | 8.49M | 94.6% |

Per-instance (resolved runs / 5):

| Instance | full | bare |
|----------|------|------|
| django__django-11239 | 5/5 | 5/5 |
| astropy__astropy-12907 | 5/5 | 5/5 |
| scikit-learn__scikit-learn-14141 | 5/5 | 4/5 (run-4 patch failed eval; run-5 no patch) |
| sympy__sympy-13974 | 0/5 | 2/5 |
| sphinx-doc__sphinx-7748 | 0/5 | 0/5 |

Selected (best-of-5) files: full 3/5, bare 3/5.

## Verdict

- **No median lift on this vehicle**: full and bare both sit at 3/5 on
  every comparison (median, selected). Bare's higher mean (3.4) comes
  from two sympy flips — noise at this N, not a claim.
- **The full loop costs ~1.9× requests** (834 vs 445) and ~1.8× prompt
  tokens at equal cache hit rates.
- **The N=1 directional evidence (2026-10-23: full 1/5 vs bare 0/5 on a
  weak nano model) does not generalize to a strong model.** gpt-6-luna
  saturates the easy 3/5 in both arms; the harness has nothing to add.
  G4/G5 ("Loom raises a *weaker* model's success rate") remains open —
  it needs a weak/cheap funded model, not a stronger vehicle.
- **Single-run numbers are ±1 instance**: the N=1 probe scored 4/5
  (sphinx resolved once, never again in 10 further runs). This is why
  the N ≥ 5 bar exists; the probe's 80% headline should be quoted as
  60% median, 80% best-case.
- One stability observation: the full arm was perfectly deterministic
  (3,3,3,3,3; identical resolved set every run) while bare varied —
  loop shaping may reduce variance even where it does not raise the
  median. Noting as a hypothesis only; 5 runs cannot establish it.
