# Literature review — 2026 harness-improvement papers (arXiv survey, 2026-11)

Scope: harness/loop-level papers from late 2025–Oct 2026 that bear on loom's
thesis (max successful work per model invocation, min context/turns,
deterministic controller). Each paper is mapped to loom: **validated** (paper
confirms shipped design; no work), **incorporate** (ranked gap), or
**decline** (with reason). Vendor-claimed numbers are hypotheses until loom's
own eval reproduces them (roadmap item 1 rule).

Papers reviewed: 2609.20804, 2610.15093, 2510.16786, 2601.22129, 2606.05922,
2601.04171, 2610.31710, 2609.08355, 2604.26102, 2608.26218, 2606.09498,
2608.01918, 2604.25850, 2610.22791, 2608.10178, 2609.26779, 2610.02163,
2610.34425, 2607.00692, 2609.34649, 2509.23586, 2608.07440, 2607.29658,
2602.08316, 2608.08950, 2608.22191, 2511.16004, 2603.15921, 2610.03984,
2610.33958, 2607.00016, 2602.20048, 2512.20957, 2405.15793. Addendum
(2026-11-02, meta-harness pass): 2408.08435, 2505.22954, 2603.19461,
2510.21614, 2608.07645, 2508.06433, 2610.26995, 2605.27366, 2602.01869,
2507.21046, 2508.07407, 2608.03392. Second addendum (2025 sweep):
2512.22087, 2610.20927, 2608.01326, 2508.21433, 2503.13657, 2410.00752,
2503.14713, 2406.12952, 2506.09289, 2501.14723, 2407.21787, 2502.01839,
2509.20837, 2503.09089, 2410.10762, 2504.15257, 2507.03616, 2502.07373.
Workflow-search line (AFlow, FlowReasoner, EvoAgentX, EvoFlow): code-space/
population workflow search, MCTS or evolutionary. Same verdict as ADAS/DGM —
declined as mechanism (code-space self-modification, multi-level MAS),
validated in spirit: execution-feedback-driven search over a bounded
operator set is exactly selfopt's propose→probe→compose; AFlow's reusable
operators argue for the policy-overlay extension already flagged. Third
addendum (Nov 2026 token-economics pass): 2610.32813, 2610.26770,
2611.02079, 2607.06906, 2610.22732, 2611.00628, 2611.01415, 2610.41291,
2607.15524, TokenPilot (no arXiv id verified).

---

## Theme A — Loop economics: turns dominate cost

**TurnCost (2610.15093).** 600 trajectories, 2 scaffolds, 2 models. Turn count
explains 84% of log-cost variance (R²=0.844); mean observation size explains
4%. Median observations are 118–286 tokens; big build logs sit at P99.
98.6% of input tokens are prefix-cache hits — flat input pricing overestimates
loop cost ~7×. Delegation to cheaper models *increased* cost in half of
settings (closed-form break-even derived). Explicitly associational: does not
prove turns removable at fixed success.
→ Loom mapping: **validated.** Byte-stable projection + cached-token census
already measure this correctly. Confirms observation-compression is a
secondary lever (loom's 64 KiB output caps + artifact spill already sit at
the right place). Feeds item 19 (delegation economics) with a concrete
break-even shape: delegation pays only well off the cache boundary.

**More with Less (2510.16786).** SWE-bench, 3 frontier models. Fixed turn
limit at P75 of baseline: −24–68% cost, minimal solve-rate impact. Dynamic
turn strategy (small initial grant, extend on demand) beats fixed: further
−12–24% at equal-or-better solve. Caveat: per-model threshold effect — Gemini
collapsed under extreme limits; limits need per-model tuning.
→ Loom mapping: **incorporate — #1.** `max_turns` is a static config today
(engine.rs). A deterministic staged budget — initial grant, extension only on
evidence of progress (files changed since last check, new green verify, new
evidence class) — is pure controller logic, exactly loom's thesis. Seam:
`EngineConfig` + the loop's turn gate; per-model P75 defaults belong in the
model-profile work (item 9). Measure via the selfopt probe protocol
(median tokens N≥5, event-count and failure gates unchanged).

**AgentDiet (2509.23586) / Blast Radius (2608.07440).** Trajectory dieting
(−21–36% cost) and reversible eviction of dead context (12–28% tokens).
→ Loom mapping: **validated, minor extension.** Loom's stub strategies cover
duplicate/superseded reads, stale errors, read age-out. Gap: `run_command`
outputs older than the age-out horizon are not stubbed (age-out covers reads
only). One extra stub class, evidence-gated through selfopt knob probing.

**CliffCompaction (2609.26779) / Compact-Bench (2610.31710).** Cliff: never
rephrase, never compact a compaction — truncation-only cuts cost ~50%.
Compact-Bench: LLM summaries retain ≤16% of recurring details vs 79.1% for
mechanical extraction; stable damage to 5–64% of full-history successes;
**DeepSeek-Flash is the worst of 14 summarizer backbones** (damages the most
repairs). Retaining "relevant" information alone does not ensure continuation.
→ Loom mapping: **validated — strongly.** Loom compacts by deterministic
one-line stubs, never LLM summary, never compacts a checkpoint twice. That
DeepSeek-Flash — loom's own eval model — is the worst compactor is the
sharpest possible external evidence for never adding a summarizer. Keep the
stub floor; nothing to adopt.

**Source-Judged Compaction (2610.20927).** 83% of failed runs trace to
summary entries unsupported by any environment output; a post-hoc judge that
deletes unsourced summary entries improves reliability 2.1–8.9%.
**The Complexity Trap (2508.21433).** Simple observation masking is as
efficient as LLM summarization for context management.
**Context Compaction Theory (2608.01326).** Communication-complexity
formalization; generation can need strictly less budget than selection;
Anthropic's compaction endpoint underperforms a same-size Bloom filter on
set membership. **Cat / SWE-Compressor (2512.22087).** Context management
as a callable tool, 57.6% Verified.
→ Loom mapping: **validated / decline.** SJC quantifies *why* summarization
fails — hallucinated state — and loom's stubs cannot hallucinate (they only
ever quote recorded facts: call id, path, sizes). Complexity Trap and the
theory paper independently confirm masking/verbatim-excerpt ≥ summarization.
Cat joins the declined CTXAgent family (model-managed context).

**CTXAgent (2610.34425) / Self-GC (2607.00692) / ContextEvo (2609.34649) /
AutoCompact (2610.02163).** Model-managed context via tools; trained
compaction-timing policies (+9.2 pp SWE-bench Verified for AutoCompact).
→ Loom mapping: **decline.** Model-managed context moves controller work back
into the model — inverts core principle 1/3. AutoCompact requires policy
training; loom is provider-agnostic with no training surface. The selfopt
knob probe already searches compaction timing deterministically.

## Theme B — Test-time scaling and selection

**SWE-Replay (2601.22129).** Naive best-of-N resamples from scratch.
SWE-Replay recycles archived trajectories, branching at intermediate steps
chosen by repository-exploration potential (no value model): −17.4% cost,
+3.8% performance on SWE-bench Verified; generalizes to Pro/Multilingual.
→ Loom mapping: **incorporate — #2.** `--best-of` children currently run N
independent engines from turn 0. Because sessions persist and the projection
is byte-stable, a shared exploratory prefix (facts compile + first K turns)
forked into N worktree-local sessions shares both the reasoning spend and the
provider prefix cache for those turns. Fork-point policy stays deterministic
(e.g., after first successful edit, or fixed K). Known risk: shared prefix
correlates failures (diversity loss) — SWE-Replay measured maintain-or-
improve, but loom must re-measure on its own subset per the maintenance bar.
Seam: `engine::worktrees` + kernel session fork (session DB already
worktree-local).

**Agentic Rubrics (2601.04171).** Rubric-building agent explores repo, then
scores candidates execution-free: 54.2% BEST@16 (Qwen3-Coder-30B), ≥+3.5 pp
over baselines. **RETRACE (2608.08950).** Backward reconstruction (infer the
issue from the patch, compare): +7.0/+3.6 pp Pass@1. **Risa (2608.22191).**
MoE router traces as a free steering/arbitration signal (44.9→48.2).
**InfCode (2511.16004).** Adversarial test-patch generator vs patch generator
+ selector, 79.4% claimed (single-paper claim, uncorroborated).
→ Loom mapping: **incorporate the deterministic part — #3; decline the
model-judged part.** Current selection is completion → failure markers →
turns → requests and never executes anything against candidates. Add a
per-candidate `verify` (existing fixed per-ecosystem table, no model call) in
each worktree as a hard filter before ranking — Trae's regression-filter
shape, minus the model. Cheap relative to N model rollouts, fails closed,
and directly prevents applying a "winner" that breaks the existing suite.
Model-judged selection (rubrics/RETRACE) spends invocations on judgment;
OpenHands' own finding (already in item-21 evidence) is that prompt-based
reranking adds little vs a trained critic — revisit only if the deterministic
filter measurably ties too often (count ties in the selection report).

**CodeMonkeys (2501.14723).** Scales serial (edit↔self-test↔execution
feedback) and parallel (10 trajectories) compute; 57.4% Verified, 69.8%
pre-selection coverage; selecting over external ensembles reaches 66.2%.
Serial and parallel compute are *not* interchangeable — same coverage,
different post-selection scores. **Large Language Monkeys (2407.21787,
2024 context).** Coverage scales log-linearly over 4 orders of magnitude;
selection, not coverage, is the bottleneck. **Sample, Scrutinize and Scale
(2502.01839).** Larger candidate pools improve self-verification accuracy
(implicit scaling), but frontier models verify weakly out of the box.
**Verification Limits Code LLM Training (2509.20837).** Verifier ceiling:
richer test suites buy ~+3 pass@1; soft LLM verification adds 2–4 but
depends on test strength.
→ Loom mapping: **validated + strengthens shortlist #3.** The field's
consistent finding is that N-rollout value lives or dies on the selector,
and model-judged selection is the weak link — loom's execution-based
deterministic filter is the right shape, and self-generated tests may
*only* be trusted as a soft signal after the deterministic gate (CodeMonkeys
uses them for voting, never as ground truth). LLM-Monkeys' log-linear
coverage curve also bounds expectations for `--best-of N`: gains from N>5
shrink per rollout.

**Test generation reality check: TestGenEval (2410.00752, ICLR 2025),
TestForge (2503.14713), SWT-Bench (2406.12952), UTBoost (2506.09289).**
Best models cover 35.2% lines / 18.8% mutation score on real-repo test
completion; agentic feedback loops (TestForge) fix pass rates but at $0.63/
file; repair agents beat dedicated test generators at test generation
(SWT-Bench); UTBoost found 345 erroneously-passing patches in SWE-Bench —
40.9% of Lite / 24.4% of Verified leaderboard entries affected.
→ Loom mapping: **decline model test-generation as verification** (confirms
the VibePass stance); **incorporate the eval-hygiene half — shortlist #8.**
UTBoost's finding hits loom directly: the frozen 100-instance subset's
verdicts rest on official PASS_TO_PASS/FAIL_TO_PASS suites, and a
nontrivial fraction of those suites under-discriminate. A one-off audit —
run UTBoost-style amplified suites against the frozen subset, record which
instances have weak P2P coverage in the provenance block, and re-gate any
headline that flips — is pure deterministic work that protects the one
number loom advertises.

**MAST — Why Do Multi-Agent LLM Systems Fail? (2503.13657, NeurIPS 2025
D&B).** 14 failure modes over 1,600+ annotated traces: system design 41.8%,
inter-agent misalignment 36.9%, task verification 21.3%; observed failure
rates 41–86.7% across open-source MAS; verifiers usually superficial.
→ Loom mapping: **validated decline of multi-level orchestration.** Two of
three failure categories (inter-agent misalignment, superficial verification)
are structural costs of delegation itself; loom's single-controller +
opt-in one-level design avoids the category rather than mitigating it.
Feeds item 19's prior: delegation needs to clear not just token cost but
the coordination-failure tax MAST quantifies.

## Theme C — Harness self-optimization

**RHO (2606.05922, EMNLP-F).** Self-supervised harness optimization from past
trajectories alone: coreset of hard tasks → parallel re-solve → self-
validation/self-consistency → pairwise self-preference picks the update.
SWE-Bench Pro 59→78% in one round, no external grading.
**Self-Harness (2606.09498), HarnessCompass (2608.01918), AHE (2604.25850),
Self-HPO (2610.22791).** Weakness mining, constrained component evolution,
observability-driven evolution, alternating harness/policy optimization — all
report multi-point gains; AHE ablation found gains lived in tools/middleware/
memory, not the system prompt.
**One Recipe, Many Harnesses (2608.10178).** Gains vary by language/model
cell; null regions exist.
→ Loom mapping: **mostly validated — selfopt already implements the search
shape** (propose → probe → compose, Harness² 2610.14224, offline + adoption
gated). Two adoptable details: (a) RHO's **hard-task coreset selection** —
selfopt currently probes a fixed instance set; weighting toward prior-failure
instances concentrates signal per probe dollar; (b) AHE's finding that gains
live in tools/middleware argues the model-proposer's proposal space should
eventually span tool-menu/policy overlays, not only the 13 `LOOM_*` knobs —
but that widens the blast radius, so keep it a flagged extension, not a
default. One-Recipe caveat is already loom doctrine (per-model measurement).

## Theme D — Interface, localization, harness variance
**ADAS / Meta Agent Search (2408.08435).** Meta agent programs whole agent
designs in code space; ever-growing archive; discovered agents transfer.
**Darwin Gödel Machine (2505.22954, ICLR 2026).** Agent iteratively edits its
own codebase, empirically validates on coding benchmarks (SWE-bench 20.0→
50.0%, Polyglot 14.2→30.7%), keeps an archive for open-ended exploration.
Follow-ons (DGM-H 2603.19461, Huxley-GM 2510.21614, Mendel-GM 2608.07645)
evolve the improvement procedure itself or cross lineages.
→ Loom mapping: **decline the mechanism, validated in spirit.** Code-space
self-modification is exactly what loom's charter forbids — G10 says learning
"without uncontrolled self-modification", and the maintenance bar requires
inspectable diffs. DGM's empirical-validation loop *is* selfopt's probe gate;
the difference is the edit surface: DGM edits arbitrary code (open-ended,
many regressing variants, archive needed to survive them), selfopt edits a
clamped `LOOM_*` overlay space that is safe by construction (floors/ceilings,
reserved vars rejected, never-worse adoption gate). For a production harness
the bounded surface is the right trade; the 2026 self-evolving-agents
surveys (2507.21046, 2608.03392) independently note practical systems favor
inspectable assets and that benchmark gains rarely demonstrate *durable*
improvement — which is what the ledger's evidence-tied lessons exist to
check. Middle ground already flagged in Theme C: widen the proposal space to
declarative policy overlays (prune rules, turn-grant rules — AHE's
tools/middleware finding) rather than raw code.

**Procedural memory / skill libraries: Memp (2508.06433), SPEM (2610.26995),
MUSE-Autoskill (2605.27366), ProcMEM (2602.01869).** Distill trajectories
into fine-grained instructions + script-like abstractions with an explicit
build/retrieve/update/refine lifecycle. Notable: Memp finds memory built by a
*stronger* model transfers to weaker models (direct G4 evidence — loom could
compile skill memory once with a strong model, serve it to cheap tiers);
MUSE-Autoskill's self-created skills beat human-authored ones (85.2 vs 81.2
on its subset) via lifecycle management, not raw accumulation.
→ Loom mapping: **incorporate — feeds shortlist #7 and Iteration 5.** The
skills iteration is already roadmap-planned; these papers supply the how:
(1) two asset kinds — bounded procedural instructions and executable script
skills — not freeform notes; (2) explicit lifecycle (create/evaluate/refine,
retire on evidence) so the library cannot monotonically grow; (3) retrieval
quality is the gate (SWE-ContextBench's negative-transfer caveat stands);
(4) cross-tier transfer makes skill compilation a one-time strong-model
investment with per-tier dividends. Same adoption gate as any selfopt
proposal applies before a skill ships to the default menu.

**Same Model, Different Harness (2608.26218).** Mechanical context shortening
moves per-task F2PF 28%→49% on a tight-window cohort — harness, not weights.
→ **validated** (loom's framing theorem; already cited in the roadmap intro).

**Harness ablation study (2609.20804).** 176 matched settings, fixed loop,
varying planning / action space / context management: (1) context management
matters most when budgets are tight, mostly by preventing overflow; (2)
rule-based elision staged before any summarization is the most efficient
strategy; recoverable-elision machinery goes unused; (3) planning is an
accuracy scaffold for weak models and a cost saver for strong ones; (4)
predefined tools help bash-weak models; bash-capable models do better *and
cheaper* with bash-only interfaces.
→ Loom mapping: findings 1–2 **validated** (loom: elision-only, stubs
one-line, overflow-prevention fail-closed; the pruning trigger exists
precisely for this). Finding 3 feeds item 9/progressive-reasoning policy.
Finding 4 — **incorporate — #4 (evidence-gated):** tool menu per model tier.
Loom already has the arms (bare loop vs 8 tools vs +symbols); make the menu a
`ModelProfile` field chosen by bash-proficiency evidence, measured with the
item-1 protocol. DeepSeek-flash sits mid-table on bash proficiency — the A/B
is cheap and reversible (`EngineConfig` flags already gate tools).

**RepoNav (2609.08355, EMNLP).** "Correct file, wrong function" is 44–58% of
function-level localization misses. A file-centered post-retrieval scaffold
(structural cues + sibling symbols) narrows the gap; gains come from evidence
*organization*, not more structure exposure.
→ Loom mapping: **incorporate — #5.** `search` returns flat match lists.
Tree-sitter symbol extraction already ships (`loom-tools::symbols`); reuse it
to attach a one-line symbol map of the matched region (enclosing symbol +
sibling names) to each match group. Deterministic, no model, no new tool —
an output-shaping change to existing search. Measure exploratory-read turns
on the NAV1 scale-tier task.

**LocAgent / Loc-Bench (2503.09089).** Graph-parsed codebases (syntax +
dependency + reference edges) give the best function-level localization
(83.4 File Acc@5); beats flat-retrieval agents (Agentless, SWE-agent,
OpenHands). 560 instances, gold files/functions from real patches.
→ Loom mapping: **decline as a tool; validated as a target.** Full graph
pipeline duplicates the declined embedding/graph line, but Loc-Bench's
gold-standard task shape is a free calibration set for loom's facts
compiler + search: measure how often loom's deterministic pre-scan puts
the gold file in the facts block, before any model turn. If the scan
misses, no amount of loop polish recovers the turn budget. Cheap, offline,
no harness change.
**Code-graph study (2610.33958), CodeCompass (2602.20048), Libra (2607.00016),
RepoNavigator (2512.20957).** Graph/embedding tools roughly double localization
for 4–8B models, shrink to 4–5 pp at 32B; agents rarely chain graph
expansions; 58% of graph-enabled trials made zero graph calls (adoption is
the bottleneck, not availability).
→ Loom mapping: **validated decline** of embedding/vector retrieval (already
roadmap doctrine). The adoption finding additionally warns: adding richer
structure tools only pays if the loop *drives* them deterministically — which
is exactly how loom's facts compiler already works.

**SWE-Edit (2604.26102).** Viewer/Editor subagent split: +2.1 pp resolve,
−17.9% cost; GRPO-trained edit-format selection +12.5 pp edit success.
→ Loom mapping: **declined for now** — multi-level delegation stays out
(item 19 will decide with measured economics; one level is the roadmap
stance). Edit-format selection is a model-profile option (roadmap already
defers "hashline or patch formats as default"). Loom's unique-match
`edit_file` with ±3-line echo and whitespace-trim repair already attacks the
dominant failure mode (byte-exact reproduction).

**STair (2607.29658).** Abstracted past repair trajectories → reusable plans;
81.2% Pass@1, transfers across agents (mini-SWE-agent 75.8→81.0), fewer
steps/tokens. Raw trajectories transfer much worse. **SWE-ContextBench
(2602.08316):** unfiltered or badly retrieved experience gives limited or
*negative* benefit.
→ Loom mapping: **later — #6.** The only candidate that changes what a fresh
run *sees* (plan injection into the facts block). Fits G10 and the selfopt
ledger shape (statements tied to evidence), but retrieval quality gates
everything — defer until selfopt's ledger demonstrates reusable lesson
stability across cycles, then prototype as a per-task-class plan overlay
with the same adoption gate as any selfopt proposal.

**VibePass (2603.15921) / self-debugging studies (2501.12793, 2610.03984).**
Self-generated tests: 86% syntactically valid, only ~61% fault-triggering;
discriminative witnesses help (+6.4 pp), non-witnesses actively hurt.
→ Loom mapping: **decline test-generation verification for now** — the
acceptance oracle and completion gate already give the deterministic half,
and self-generated-test bias is a documented negative for exactly the
mid-tier models loom elevates. Revisit only behind a discriminativeness
check (witness must flip a red state to green), which is measurable.

## Theme E — token-economics line (Oct–Nov 2026)

**HarnessMM (2610.32813).** Two mechanisms: (a) *guarded decision reuse* —
once the model resolves a next action, a deterministic controller skips
later reasoning calls while tool feedback confirms the stored decision is
still valid; (b) recursive self-improvement of the guard configuration.
HumanEval (Codex): tokens/task −29.5% at flat 90.8% success; DeepSeek
harness: −87.9% tokens *and* 71.4→85.7% success; MBPP-sanitized similar.
→ Loom mapping: **incorporate — shortlist #2.** The guard is pure
deterministic-controller logic — skip a model invocation when recorded
evidence says the pending decision is unchanged. This is principle 1
("never spend a model invocation on deterministic work") extended from tool
scheduling to *decision scheduling*. Loom already has the seams: effects/
side-channel state, the wave scheduler, and the `unchanged since call <id>`
dedup. Caveat: evidence is HumanEval/MBPP — single-file, near-trivial tasks
where a stored decision survives many turns; SWE-bench-class transfer is
unproven. Gate: paired selfopt-style probe on the frozen subset before any
default-on behavior. Obvious guard primitives for loom: repeat verify after
non-mutating turn, continue an in-flight batch edit plan when the parse
guard holds, skip re-deliberation while only file mtimes/ids changed.

**HarnessPro (2610.26770).** Label-free evolution (RHO family) emitting
lite/medium/heavy harness tiers optimized for different performance–cost
priorities; heavy mode beats prior RHO (+2.0 pp avg pass, −26.1% per-task
cost across Terminal-Bench 2 / SWE-Bench Pro / ProgramBench).
→ Loom mapping: **incorporate as a selfopt output shape.** Today selfopt
adopts one winning overlay; HarnessPro's result says the Pareto set itself
is the product — a cost tier (`--tier lite|heavy`) is a user-facing choice,
not a search artifact. Cheap change: selfopt already scores every proposal
on both axes; emit the Pareto-ranked overlay set, not just the merged
winner.

**ParetoSaddler (2611.02079).** Separates intermediate promotion (Bayesian
bootstrap Pareto support on *either* axis) from final deployment selection,
so early cost bias does not prune later quality gains. ClawMark
70.9→81.4% quality at −14% cost; GAIA2 56.2→62.9% at −42% cost.
→ Loom mapping: **flagged refinement, not default.** Loom's adoption gate is
deliberately arithmetic; ParetoSaddler argues threshold gates bias the
search toward early cost wins. Adopt only if selfopt cycles show premature
convergence (ledger: repeated proposals rejected for event-count regression
that later versions would have passed). Note the bootstrap machinery costs
probe budget — the loom-cheap half is the *separation* of promotion from
deployment, not the Bayesian layer.

**The Harness Effect (2607.06906).** Orchestration-layer-only swap, same
models/tasks/judges: −38% tokens/task, −41% cost, −44% wall-clock, quality
flat-to-up. Efficiency model-invariant (33–61% across six models); *quality*
gains correlate with baseline model strength ("harness leverage").
→ Loom mapping: **validated.** Thesis confirmed externally. The leverage
nuance sets expectations for G4: orchestration buys cheap models cost cuts,
not quality lifts — quality returns need the model tier under them. Feeds
item 9 profiles (spend orchestration effort where the model can use it).

**TokenEconomizer (2610.22732).** External black-box controller: injects
micro-skills, prunes irrelevant tools per task, intervenes in failure loops.
SWE-Bench: −18.4–30.9% mean tokens, Pass@1 up in 4/5 settings.
→ Loom mapping: **incorporate two levers — shortlist #5 (merged).** Loom
*is* the external controller, so this is the closest published description
of loom's own design space. New specifics worth taking: (a) per-task
dynamic tool pruning (loom's menu is static per run — the facts scan knows
the languages/build systems, so the menu can be narrowed deterministically
before turn 1: no cargo tools in an npm workspace); (b) deterministic
failure-loop intervention — the failure ledger already detects near-repeats;
add a table-driven stall-breaker (e.g., 3 failed edits on one file → inject
its `symbols_overview` into the next turn's context, not a model call).

**AgentPRO (2611.00628).** Compiles agent workflows to graphs, merges
generation regions, removes redundant work under runtime guards;
−41.85% tokens on 115 AppWorld tasks.
→ Loom mapping: **mostly validated.** Region-merging ≈ loom's wave scheduler
+ atomic multi-edit `edits: [...]` batches; redundancy removal ≈ tool cache
+ decision dedup. Nothing new to adopt beyond shortlist #2's guards.

**Latent Harness (2611.01415).** Compiles harness behavior into a rank-8
activation shift on a frozen model — zero inference tokens, recovers most
harness gains. **Agentic Soft Tokens (2610.41291)** — in-model context
compression, ~50% fewer attention pairs.
→ Loom mapping: **decline.** Both require activation-level access to the
served model — breaks G7 (provider-agnostic) for every hosted provider.
Revisit only as a research plugin if loom ever serves open-weight models
locally; as a search space for harness ideas, Latent Harness is a
compressor, not a designer.

**TokenPilot.** Context compaction with lifecycle-aware eviction; 50–80%
cost cuts on long-horizon benches. **Recursive Harness Self-Improvement
(2607.15524).** ~60% inference-cost cuts via context/information-flow
tuning, same family as selfopt/Harness².
→ Loom mapping: **validated + minor extension.** Lifecycle-aware eviction ≈
loom's stub classes plus one idea: retention keyed to *task phase*
(exploration artifacts lose value once the patch is being written — stub
them earlier). Phase is derivable deterministically (last N turns contain
edit_file calls → writing phase). One extra stub rule, selfopt-gated.

---

## Incorporation shortlist (ranked)

| # | Change | Paper | Seam | Expected | Risk |
|---|--------|-------|------|----------|------|
| 1 | Dynamic staged turn budget (grant + evidence-based extension) | 2510.16786 | loop turn gate, `EngineConfig` | −12–24% cost over fixed limit at ≥ solve | per-model thresholds; associational evidence — selfopt probe first |
| 2 | Guarded decision reuse: skip model call when stored decision still valid under deterministic guards | HarnessMM 2610.32813 | controller decision cache, effects state, wave scheduler | −30–88% tokens (paper), thesis-core | toy-bench evidence only — paired probe on frozen subset first |
| 3 | Shared-prefix fork for `--best-of` rollouts | 2601.22129 | `engine::worktrees` + session fork | −17% rollout cost, prefix-cache reuse | correlated failures; re-measure on frozen subset |
| 4 | Execution-based candidate filter before selection | Trae/InfCode selector, CodeMonkeys, 2502.01839 | `worktrees` selection + existing `verify` table | fewer bad winners; no model cost | suite runtime per candidate; suites that don't cover the fix |
| 5 | Per-task tool-menu policy: model-profile menu + facts-scan pruning + table-driven stall-breakers | 2609.20804 f4, TokenEconomizer 2610.22732 | item 9 profiles, facts compiler, failure ledger | −18–31% tokens (paper), fewer stall loops | pruning wrong tool = task loss — prune only on certain facts (language/build detection) |
| 6 | File-centered search output (symbol cues per match) | 2609.08355 | `search` output shaping + existing tree-sitter | fewer wrong-function edits | token cost of cues — output budget marker |
| 7 | Selfopt extensions: hard-task coreset + Pareto-ranked tier output (`--tier lite/heavy`), promotion/deployment split if convergence stalls | RHO 2606.05922, HarnessPro 2610.26770, ParetoSaddler 2611.02079 | selfopt instance selection + output shape | more signal per probe; user-selectable cost tier | tier set multiplies maintenance surface |
| 8 | Skill/plan memory: procedural instructions + script skills, lifecycle-managed, compiled by strong model for weak tiers | 2607.29658, Memp 2508.06433, MUSE-Autoskill 2605.27366, caveat 2602.08316 | Iteration 5 skills + selfopt ledger | later; cross-tier (G4) | negative transfer — retrieval gate + per-skill adoption gate |
| 9 | Frozen-subset suite audit (UTBoost-style amplified P2P/F2P, weak-instance flags in provenance) | 2506.09289 | eval provenance block + one-off audit | protects the advertised 90/100 from suite noise | none — deterministic, offline |
| 10 | Phase-aware stub retention (exploration reads stub earlier once writing phase detected) | TokenPilot | stub strategies, deterministic phase flag | small extra token cut on long tasks | marginal — selfopt-gated, skip if knob probe shows noise |

Every item ships behind the maintenance bar: live-eval comparison, medians,
N ≥ 5, pass rate co-equal with cost.

Fourth addendum (2026-12, meta-harness/vendor sweep): ACon 2510.00615,
PAACE 2512.16970, GCC 2508.00031, context-folding 2510.11967,
AgentRouter 2609.22951, plus Anthropic engineering blogs (context
management, long-running harnesses), Claude Code docs (checkpointing,
skills, commands), Amp docs, tmustier harness-benchmarks.

- **Anthropic tool-result clearing (vendor, internal eval).** −84%
  tokens at +29% agentic-eval performance; +39% with the file-based
  memory tool. → **validated** — externally confirms loom's stub
  strategies as the first lever; the memory-tool delta is item 20's
  evidence shape, not a summarizer argument.
- **ACon / PAACE / GCC / context-folding.** Learned or plan-aware
  compressors; GCC adds git-style COMMIT/BRANCH/MERGE over context;
  CoF folds mid-trajectory state. → **decline as mechanism** (trained
  compressors break G7; GCC's milestone checkpoints ≈ loom's frozen
  checkpoints + item 28's session fork, both already shipped shapes).
- **AgentRouter (step-level routing).** Route each trajectory *step*
  across heterogeneous models. → **hypothesis for item 9/Iteration 8**;
  step-level routing requires relaxing the model-fixed-per-task
  invariant — Iteration-8 ADR territory, not a near-term adopt.
- **Harness benchmark spreads (tmustier; Claw-SWE-Bench).** Same model,
  12.5–27.4 pt pass@1 spread across harnesses; native CLIs led 4/6
  matched Terminal-Bench 2.1 pairs. → **validated** — consistent with
  the roadmap's framing findings; no action.
- **Claude Code checkpoint/rewind; Amp subagent roles (Oracle/
  Librarian/Finder); /skill-doctor telemetry.** → **product-surface
  notes, not papers**: rewind rides items 15/22 (see roadmap gap-check
  note); Amp's role split is item 19's specialization question;
  skill telemetry extends `loom stats` under item 20.4.
- **Post-land note (2026-12):** the execution-based selection entry
  above is now item 29 shipped — `verify_selection` filters candidates
  by the fixed-table suite before ranking; open probe tail is the
  frozen-subset re-measure of correlated-suite-failure rate and
  wall-clock cost before recommending `N > 3`.
