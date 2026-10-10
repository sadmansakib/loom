# Published anchors for the external slice

Numbers we cite when reporting loom's EXT-* results against existing agents.
All from the Multi-SWE-bench paper (arXiv 2504.02605, NeurIPS 2025 D&B) unless noted.

## Multi-SWE-bench Rust — resolve rate (%), 239 instances

Resolved rate by scaffold × model (Table 4 / Table 6 BF column):

| Scaffold | Model | Rust % |
| --- | --- | --- |
| MagentLess | GPT-4o | 5.86 |
| MagentLess | OpenAI-o1 | 7.11 |
| MagentLess | OpenAI-o3-mini-high | 7.95 |
| MagentLess | Claude-3.5-Sonnet | 5.02 |
| MagentLess | Claude-3.7-Sonnet | 5.44 |
| MSWE-agent | GPT-4o | 2.09 |
| MSWE-agent | DeepSeek-R1 | 2.09 |
| MSWE-agent | Claude-3.7-Sonnet | ~5.4 (BF) |
| MopenHands | Claude-3.5-Sonnet | ~13.0 (BF) — best Rust scaffold |

Spread across 9 models × 3 scaffolds: **2.09%–12.97%**. Hard-tier Rust ≈ 0% for every
model/scaffold. Paper's headline: "even the best agent only gets ~12.13% on Rust."

DeepSeek-family context (closest to our deepseek-flash arm): DeepSeek-V3 MagentLess
Rust ≈ 4.6% BF; DeepSeek-R1 MSWE-agent Rust 2.09%.

## Interpretation rules for our EXT slice

- Our 5 EXT instances (bytes×2, rayon, clap, tracing) are small-library, mostly
  easy/medium end of the Rust distribution — expect rates above the paper's overall
  Rust mean, not comparable to hard-tier.
- N=5 per instance → per-instance solve granularity is 20%; report per-instance
  pass/fail plus slice-level rate with that caveat.
- Scaffold-Effect framing: the 6× spread between scaffolds at constant model is the
  published motivation for our full-vs-bare A/B.

## Contrast anchor (different benchmark, do not mix)

SWE-bench Multilingual (Khandpur 2025, 43 Rust tasks incl. ruff/coreutils, $2.50
cost cap): Rust 58.14% with SWE-agent + Claude-3.7-Sonnet. Different, easier
distribution — cite only to show benchmark-difficulty variance.

## Sources

- Paper: https://arxiv.org/abs/2504.02605 (Tables 4, 5, 6)
- Leaderboard: https://multi-swe-bench.github.io/ (Pass@1, no hints, unassisted)
- IBM iSWE-Agent Java blog (leaderboard-method context): research.ibm.com, 2025-12-08
- SWE-bench Multilingual: https://swe-agent-bench.github.io/multilingual.html
