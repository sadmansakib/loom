# Argument-provenance gate — design (roadmap item 38)

Status: design + evidence gate. Feature 7 of the 8-feature plan.
Threat model first: [docs/threat-model.md](threat-model.md).

## What CaMeL proposes, and what loom adopts

CaMeL (Debenedetti et al., arXiv 2503.18813, "Defeating Prompt
Injections by Design") wraps the model in a protective layer: the
(trusted) query is compiled into an explicit program, an interpreter
mediates every dataflow, values carry security tags, and tool calls
are capabilities checked against policies at call time. Untrusted
data can then never influence control flow, and tainted values
cannot reach privileged parameters without an explicit check. On
AgentDojo it solves 77% of tasks with *provable* security vs 84%
undefended.

Loom is a single-model free-form loop, not a DSL interpreter — the
two-LLM program-extraction architecture does not transfer. What
transfers is the core idea restated for a loop:

> At tool-admission time, ask where each argument came from, and
> treat "derived from untrusted tool output" as grounds for an
> explicit operator decision.

## Mechanism

**Where.** `execute_batch` (kernel `agent`), next to the existing
refusal partition (malformed salvage, ledger repeats). The gate is a
third refusal reason — refused calls never reach the tool engine and
commit a filler result, exactly like the others.

**Taint corpus.** For the running task: the concatenated bodies of
all `Item::ToolResult` items already in the session, in transcript
order, *before* the turn being admitted. User text, manifests, and
system configuration are not in the corpus (trusted; see the threat
model).

**Taint test.** For each sensitive call, take every JSON *string*
value in `arguments` (recursively; numbers/bools cannot smuggle
instructions). Find the longest verbatim fragment shared with the
corpus. Efficient form: index the corpus as a set of K-length
rolling hashes at every offset; slide each argument string over the
same K-window; on a hash hit, verify with a real substring check
(hash collision → no match). Longest-fragment is then expandable
around the first verified hit. Cost is linear in corpus + argument
bytes per admitted sensitive call; the corpus index is built once
per turn that has sensitive calls.

**Fragment threshold.** K = 32 bytes. Shorter windows match code
identifiers and shell words everywhere (benign noise); 32 is above
typical identifier length and below one instruction clause. The
evidence run below measures actual fragment-length distributions and
can move this before anything ships.

**Sensitive tools (proposed, evidence-pending).**

- `run_command` — arguments are the executed program text.
- `write_file` / `edit_file` / `replace_symbol` when the destination
  is a *trusted path* (`.loom/**`, manifest-adjacent files) —
  scenario 2 of the threat model. Plain workspace edits are the
  agent's job; the expected benign-derived rate for ordinary edits
  (read a file, then edit it) is the reason this half needs evidence
  before it may ship.

**Verdicts.** Reuse the policy vocabulary:

- `off` (default): no check, no events, byte-identical behavior —
  the opt-in rule; frozen SWE-bench and bare-loop paths are
  untouched.
- `flag`: check and *record* (structured event with fragment head,
  source call id, tool, argument path) but admit the call — the
  measurement mode the evidence run below wants in-tree.
- `gate`: refuse with a deterministic refusal result (new
  `ToolFailure` variant, additive; wire-visible → event-schema bump)
  naming the source call id and fragment head, with a next-step hint
  telling the model it may re-issue the call with arguments it can
  justify from the trusted prompt.
- `ask`: escalate through the existing `EngineOverrides.ask` seam
  (roadmap item 37) when a resolver is present; fail closed without
  one. An explicit allow approves that one call.

Configuration rides `EngineOverrides` (engine) and a manifest key
(provenance section in `.loom/commands.toml` or a sibling), default
absent everywhere.

## Detection bound (stated plainly)

The gate detects *copied* untrusted content. It does not catch
paraphrase, re-encoding (base64, unicode homoglyphs), or an
injection that persuades the model to compose arguments from scratch.
CaMeL's provable non-interference is unreachable through a model; a
measured, honest heuristic plus the sandbox/policy/ask stack is what
a loop can actually deliver. The threat-model document carries this
same caveat.

## Evidence gate (checked before building anything)

Question: does the flow the gate targets actually occur, and would
the write/edit half be usable rather than a false-positive machine?

Corpus: every session in the live `loom.db` (loom's own recorded
runs; the `target/eval/swebench` logs are mini-swe-agent traces, not
loom's, and are excluded). For each assistant tool call of a
sensitive tool, in transcript order: longest verbatim fragment
shared between any string argument value and all prior tool-result
bodies of the same session. Report:

1. `run_command` calls with a fragment ≥ K (the target flow), plus
   the fragment-length distribution.
2. `write_file`/`edit_file`/`replace_symbol` calls with a fragment
   ≥ K split by destination (trusted-path vs not) — the
   benign-derived rate.

Decision rule, fixed in advance:

- **Defer everything** if `run_command` taint is zero across all
  sessions *and* no trusted-path write occurs: no demonstrated flow,
  no gate (record the numbers, roadmap deferral entry, stop).
- **Build `run_command` + trusted-path-write gating, `flag` mode
  first** if any tainted `run_command` flow exists. Ordinary-edit
  gating ships only if its benign-derived rate at K is **below
  10%** of ordinary-edit calls; otherwise that half is declined with
  the measured rate as the reason.

`flag` mode lands regardless of the second threshold once the first
holds — measurement before enforcement.

## Tests (written first)

- Fragment finder unit: verbatim copy hits at exactly K, K−1 misses,
  re-encoded content misses, argument-string recursion into nested
  JSON.
- Corpus index: same-session only (a prior session's results never
  taint), result-order respected (a result *after* the call cannot
  taint it).
- Gate unit: `off` admits byte-identically (no events); `flag` admits
  + records; `gate` refuses with the source call id in the result;
  `ask` without a resolver fails closed.
- Engine-level: an injected `run_command` whose argument copies ≥ K
  bytes from a read file is refused under `gate` and admitted with
  an event under `flag`; bare-loop SWE-bench profile emits nothing.
- ACP: an `ask`-verdict provenance escalation round-trips through
  `session/request_permission` (allow → executes, deny → refused).

## Evidence run (measured before building — result: deferral)

Ran against the live `loom.db` (15 sessions; 43 sensitive calls:
31 `run_command`, 12 `edit_file`, 0 `write_file`/`replace_symbol`;
120 string arguments re-verified with an independent LCS
implementation, which reported *longer* fragments everywhere the two
disagreed — the counts below are lower bounds):

| Tool | String args | ≥ 16 | ≥ 24 | ≥ 32 | ≥ 48 |
| --- | --- | --- | --- | --- | --- |
| `run_command` | 84 | 21 | 21 | 21 | 21 |
| `edit_file` | 36 | 36 | 36 | 36 | 23 |

Every single ≥ 24 fragment was inspected: all 21 `run_command`
matches are workspace-root *paths* flowing into `$.path` / `$.dir` /
`$.workdir` (the workspace path appears in command output — cargo
paths, errors — and is legitimately reused), and all 36 `edit_file`
matches are `old_string`/`new_string` anchors plus paths, i.e. the
mechanics of editing a file that was just read. **Zero
instruction-like flows into `run_command`. Zero writes to trusted
paths.**

Verdicts against the pre-registered decision rule:

- Ordinary-edit gating: benign-derived rate **36/36 = 100%** against
  a 10% ceiling — declined, by the rule.
- `run_command` gating: the adversarial flow it targets was observed
  **0 times**, and the measurement falsified the heuristic itself —
  verbatim fragment-sharing at K = 32 is the *norm* for sensitive
  arguments (a 100% benign match rate), not an anomaly an
  enforcement gate could threshold. Even `flag` mode would fire on
  essentially every command: noise, not measurement.

Deferred (roadmap item 38). The revisit condition is **not** "more
sessions" — more benign sessions cannot fix a non-discriminative
signal. Revisit when one of these exists: (a) an adversarial corpus
recorded through loom (AgentDojo-style injected tasks saved as
sessions) that changes the base rate and lets a threshold be
measured honestly; or (b) a *semantic* provenance signal (class of
content: instruction-like vs identifier/path/anchor) rather than
verbatim copying. Seam facts for that build: admission point is
`execute_batch` beside the existing refusal partition; corpus is
prior `Item::ToolResult` bodies in transcript order; the `ask`
escalation path is the `EngineOverrides.ask` seam from item 37; a
refused call needs a new additive `ToolFailure` variant and an
event-schema version bump. Artifacts:
`/private/var/folders/sf/kblwt_lj3c1gbsvjy52tt2fr0000gn/T/opencode/f7_evidence.{py,json}`
and `f7_crosscheck.py` (session temp; numbers preserved here).
