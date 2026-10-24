# Subagent design (item 19 extension: multi-model collaboration)

Research-grounded design for loom's `scout` subagent and the multi-model
collaboration layered onto it in the 2026-10-25 iteration. Three constraints
dominate: delegation must pay for its tokens, the parent stays the only
decision-maker, and everything the parent cannot verify deterministically
stays visible as data rather than hidden behind a heuristic the model is
forced to trust.

Evidence base (papers + industry postmortems, collected 2025-10→2026-10):

- **Depth is a liability.** Cognition's "Don't Build Multi-Agents" (2025-06)
  frames two principles: share full context (subagents acting on summaries
  misinterpret — even sharing *all* context underperformed a single
  sequential agent on coding), and actions carry implicit decisions, so
  parallel agents without shared context make conflicting assumptions.
  Nested chains are compounding lossy compression. Their observed
  production answer: single-threaded agents plus a dedicated compression
  model — and Claude Code's subagents: one level, question-answering only.
- **One level is what the leaders ship.** Anthropic's multi-agent research
  system (2025-06) is an orchestrator-worker topology: a lead agent plus
  parallel specialized subagents, *no recursion*. Token usage alone
  explained 80% of BrowseComp performance variance — multi-agent search is
  compute scaling (agents 4× chat tokens, multi-agent 15×). The lead
  agent's output (76% of chats) dominated end-task accuracy, so their
  effort went into delegation quality: capsules with objective, output
  format, tools, and task boundaries; explicit effort-scaling rules
  (simple → 1 subagent, comparison → 2–4, complex → 10+). Parallel
  subagents cut wall-clock up to 90%. LangChain's multicounter research
  (2025-06) agrees: agents are not yet good at coordinating in real time;
  domains needing shared context are a poor fit.
- **Cascades cut cost when the gate is good.** FrugalGPT (arXiv
  2305.05176) routes queries to a cheap model first and escalates only
  when a quality gate says so — up to 98% cost reduction at matched GPT-4
  quality. De Koninck et al. 2025 unify routing and cascading and find the
  quality *estimator* is the critical factor in cascade success. Routing
  surveys (arXiv 2502.04506, 2506.06579, 2603.04445) formalize the split:
  routing selects before inference, cascading escalates after a weak
  answer; production systems compose both.
- **Multiple proposers beat one.** Mixture-of-Agents (Wang et al. 2024,
  Together AI): n proposer models feeding an aggregator outperform the
  best single proposer (61.3% vs 56.7% AlpacaEval 2 with n=6); layered
  aggregation beats LLM-rankers. Yue et al. 2023 use self-consistency as
  a confidence signal for escalation; CascadeDebate (ACL 2026 industry)
  adds deliberation at the escalation boundary for ambiguous queries.

## Design

loom's controller stays deterministic code; the parent model stays the
only aggregator and decision-maker. Everything below is bounded, labeled,
and fail-soft.

### 1. One delegation level — structurally, not by convention

Scout is the only subagent, and the child engine composes *without* scout
in its tool registry (engine.rs `compose_scout`), so recursion is
impossible by construction — no flag to misuse, no prompt to jailbreak.
This is the Cognition/Anthropic position made code: nested chains are
compounding lossy compression, and every production system that works
ships flat.

The parent passes a capsule (question + focus + output contract), the
child answers under read-only tools, and the parent — which holds the
full context — synthesizes. Cross-scout synthesis is never delegated to
another LLM: there is no judge model in the runtime.

### 2. Cascade escalation — a deterministic gate, one hop

`--scout PROVIDER --scout-model MODEL` picks the primary (cheap) model.
`--scout-escalation PROVIDER --scout-escalation-model MODEL` optionally
names a stronger model.

After the primary answers, deterministic code — not a learned scorer, not
another LLM — checks the answer against the capsule's own output
contract. Escalation fires when the primary:

- timed out, or the run failed outright;
- produced an empty answer;
- was truncated at the 1 KB answer budget;
- cites no `file:line` pointers at all (the capsule requires one per
  claim — a pointer-free answer is contract-violating by construction,
  not merely "probably bad").

On any of these, the *same capsule* runs once on the strong model under
the same turn/wall budgets, and the parent receives both answers labeled:

```
[scout]
...primary answer...

[escalated to strong-model: primary answer was missing the file:line
pointers the capsule contract requires]

...strong answer...
```

One hop, never a chain: the escalation child is composed without an
escalation of its own. This is FrugalGPT's shape with the gate swapped
for deterministic contract checks. The tradeoff is stated honestly:
De Koninck et al. find the quality estimator is the critical factor in
cascade performance, and code heuristics are a conservative estimator —
they recall clear-cut failures (timeout, empty, pointer-free) and
deliberately miss subtle wrongness. A subtle-but-wrong answer that cites
files will not escalate; the mitigation is that the parent reads the
answer with provenance and its own judgment. A learned estimator would
recall more and cost trust: it fails silently, drifts with the model
population, and cannot be audited. loom prefers the gate it can prove.

### 3. Cross-check — two proposers, the parent aggregates

With an escalation model configured, `cross_check: true` runs the capsule
on *both* models unconditionally and returns both answers plus a
deterministic agreement note computed from the `file:line` citation sets:

```
[cross-check: cheap-model vs strong-model]
...answer A... (model: cheap-model, 3 turns)
...answer B... (model: strong-model, 2 turns)
[agreement note: both cite src/a.rs:10, src/b.rs:4; only the primary
model cites src/c.rs:9; only the strong model cites src/d.rs:22]
```

This is Mixture-of-Agents' multi-proposer finding applied at loom's
scale (n=2, not 6): independent proposals plus aggregation beat a single
proposal. The aggregation itself stays with the parent model — matching
Anthropic's lead-agent-dominates-results observation — because a judge
LLM would add a 15×-token-class expense and an unauditable opinion.
Citation-set intersection is where the models' *evidence* agrees, which
is the part code can check; whether their *conclusions* agree is for the
parent to read. `cross_check` without an escalation model configured says
so plainly instead of silently degrading.

### 4. Fan-out — parallel questions, labeled and budgeted

`questions: [...]` (≤ `SCOUT_CONCURRENCY` = 3) replaces `question` for
breadth-first queries: independent capsules run concurrently, each answer
budget-truncated individually and labeled `[scout 1/3]`, `[scout 2/3]`,
… Over-cap input fails soft (an in-band answer explaining the cap), never
queues, never blocks the parent — the fail-soft semaphore semantics scout
already had. This is Anthropic's parallel-subagent wall-clock win (up to
90%) bounded to loom's existing concurrency budget; three is also the
point of diminishing multi-proposer returns per MoA's n-sweep at loom's
price point.

### 5. Provenance and spend footers

Every scout answer ends with a footer counted from the session transcript
— `(model: cheap-model, 3 turn(s))` — and escalations/cross-checks label
each model's section. Multi-agent systems spend 15× chat tokens
(Anthropic); loom's answer to that number is not to hide it but to make
every scout call's spend visible at the point of use. Footers feed the
parked delegation-economics trigger (roadmap item 19): once enough runs
accrue, the cost/benefit question of item 21 (parallel worktree agents)
becomes answerable with measurements instead of vibes.

## Budget table

| Budget | Value | Enforced by |
|---|---|---|
| Answer size | 1 KB (`ANSWER_BUDGET_BYTES`), char-boundary truncate | `truncate_answer` |
| Child turns | 8 (`max_turns`) | agent loop |
| Wall clock | 120 s per scout run (primary and escalation each) | `tokio::select!` |
| Fan-out | ≤ 3 questions per call, fail-soft over cap | schema + semaphore |
| Escalations | ≤ 1 per question, structurally (escalation child has none) | `compose_scout` |
| Concurrency | 3 permits, fail-soft "at capacity" — never queue | semaphore |
| Child tools | read-only registry only | `compose_scout` filter |

All failure modes — timeout, cancellation, cap, empty input, failed run —
return in-band answers that say what happened; scout never raises, never
blocks the parent.

## Residual risk, stated honestly

- **The deterministic gate is a conservative estimator.** It catches
  contract violations, not subtle wrongness (see §2). A wrong answer with
  plausible citations passes. The parent reads provenance labels; nothing
  pretends otherwise.
- **No per-child context partitioning.** The child sees its capsule and
  whatever its read-only tools fetch — not the parent's conversation.
  This is the Cognition "share full context" problem accepted knowingly:
  scout questions are scoped to be answerable from the workspace, and the
  parent synthesizes. Partitioning per origin is a measured follow-up if
  injection-through-results ever shows up in practice (see
  mcp-design.md §6 for the sibling discussion).
- **Synthesis quality is the parent's.** MoA's aggregator lift assumes a
  strong aggregator; loom's aggregator is the parent model with full
  context. If the parent skims, cross-check buys citation diffing, not
  truth.
- **Delegation economics remain unmeasured at scale.** Footers make spend
  visible; the parked trigger (item 19) decides whether item 21's
  parallel worktree agents are worth building.

## CLI

```
loom --scout openai --scout-model gpt-5-mini \
     --scout-escalation openai --scout-escalation-model gpt-5.3
```

`--scout-escalation` and `--scout-escalation-model` are both-or-neither
and require the `--scout`/`--scout-model` pair. Without `--scout*`, loom
runs scout-free (frozen SWE-bench protocol untouched).
