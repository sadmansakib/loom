# SWE-bench Verified N=5 effort-schedule contrast — gpt-6-luna

Date: 2026-11-07. Roadmap item 9's first funded measurement.

## Vehicle

- Agent: loom (musl linux binary, one build for both arms, digest-identical).
- Model: `aws-bedrock/openai.gpt-6-luna` via a named OpenAI-compatible
  gateway (LiteLLM, paid; name withheld), catalog-advertised
  `supports_reasoning`, `reasoning_effort` passthrough behaviorally
  verified before the sweep (54 -> 89 completion tokens, low vs high).
- Instances: the 5-instance frozen probe set (django 11239, sphinx 7748,
  scikit-learn 14141, sympy 13974, astropy 12907), full loop both arms.
- Arms differ only in `LOOM_REASONING_SCHEDULE`: `progressive`
  (high effort for opening 2 turns and the turn after a `verify` call,
  low between) vs `fixed` (byte-off — no effort field on the wire).
- N=5 per arm, official harness scoring per run.

## Resolve rates

| run | progressive | fixed |
|-----|-------------|-------|
| 1   | 4/5         | 3/5   |
| 2   | 3/5         | 4/5   |
| 3   | 4/5         | 3/5   |
| 4   | 4/5         | 3/5   |
| 5   | 3/5         | 3/5   |
| median | **4/5**  | 3/5   |
| mean | 3.6         | 3.4   |

Per-instance: django 5/5 vs 5/5, astropy 5/5 vs 5/5, scikit-learn 5/5
vs 5/5, sympy 3/5 vs 2/5, sphinx 0/5 vs 0/5. The entire difference is
the marginal instance (sympy); pooled 18/25 vs 17/25 (p ≈ 0.7, Fisher).
Mechanics: progressive 817 model requests, 14.68M prompt tokens
(95.9% cache); fixed 870 requests, 18.22M prompt tokens (95.4% cache)
— but same-config sweeps have shown ±19% prompt-token swings, so the
token totals are not evidence of a cost win.

## Verdict

Directional win on the pre-registered metric (medians 4/5 vs 3/5), not
statistically distinguishable at N=5 on a 5-instance set: sympy is the
only instance that moved, and 3/5-vs-2/5 on one instance is noise-level.
What the measurement does establish: the progressive schedule is safe on
this model — no quality loss, no cost blowup, byte-off default intact.
The schedule stays default-off; the G4/G5 weak-model-lift question moves
to a larger instance set where a ±1 median can separate from noise.
