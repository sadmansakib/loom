# Checkpoint/rewind design note (roadmap gap 1, feeds items 15/22)

Spec for restoring code + conversation state to a chosen turn — the
Claude Code `/rewind` shape. The roadmap framed this as "deterministic
today (worktree git + session DB), a frontend+CLI feature, not engine
work". That framing is half right, and the half that is wrong changes
the rollout order, so this note starts by correcting it.

Premise check (verified against the code, 2026-11-06):

- Conversation state IS exactly derivable today: the transcript is the
  append-only source of truth (ADR-004), items are addressed by stable
  `ItemIndex`, and the store mirrors it item for item. A rewind at any
  boundary is one read.
- Code state is NOT derivable at an arbitrary turn today. `ToolEffects`
  (`tool.rs`) records *which* files a call wrote or deleted, never
  content. `capture_patch` (worktrees) diffs a whole tree once at
  rollout end. Nothing snapshots per-turn file content, so a mid-session
  boundary has no reconstructable tree. Reverse-replaying `edit` calls
  from the transcript's serialized args is not honest determinism:
  whitespace repairs, ambiguous matches, and out-of-band command
  effects (`commands_run`) break it silently.
- What IS derivable without engine work: the session-start checkpoint.
  If the workspace was a clean git checkout when `loom run` started,
  `git reset --hard <start-head>` + `git clean` restores it exactly.

So the feature ships in three tiers, each useful alone, and the tier
that needs engine work is named now so it lands without redesign.

## Design

### 1. Rewind is a fork, never a truncate

The transcript is append-only and indices are guaranteed stable for the
life of a session (ADR-004, `session.rs` docs). In-place truncation
would break that invariant and destroy the only evidence of what was
undone. Rewind therefore copies items `[0..boundary)` into a **new**
session id and leaves the original untouched and still listed:

- New `SessionStore::fork(source, boundary, new_id, provenance)` —
  SQLite does it in one transaction
  (`INSERT INTO items SELECT :new, idx, … WHERE session_id = :src AND
  idx < :boundary`), so a crash mid-fork leaves nothing half-built.
- Copied items **keep their indices** (`idx` 0..boundary): references
  held against the original remain valid in the fork.
- Task records are not copied. They are per-run metadata; the fork is
  conversational state and starts a fresh task on `--session` continue
  (which is what resume does anyway).
- Todos start empty — identical to reloading a session today.
- Artifact refs stay valid: the artifacts table is global by id, not
  per-session.
- Events are not copied. The event stream is a per-session trace; the
  transcript remains the truth (ADR-004).
- Provenance: the new session records `forked_from` + boundary (meta
  row or additive sessions column) and `loom sessions` shows the fork
  marker, so a forked lineage is never mistaken for an original.

Both `MemorySessionStore` and `SqliteStore` implement `fork`; schema
changes are additive only.

### 2. Guard: leases

Rewind refuses while a live lease exists on the source session — the
same single-writer discipline `acquire_lease` already enforces. A
rewind racing a running task would fork a transcript whose tip is about
to move. Refusal message names the lease owner.

### 3. Boundaries and the picker

A boundary is a transcript index `b`; the fork keeps `[0..b)`. The
interactive picker lists committed turns (boundary = index just past a
turn's last tool result, or at a `User` item to drop a whole attempt):
turn number, assistant-text head, and an effects summary (files
touched) computed from the transcript alone. Non-interactive:
`--at <index>`.

### 4. Code restore, tiered by what is provable

- **Tier 0 — refuse.** No determinable tree state (dirty start,
  non-git workspace): `--restore-workspace` refuses with the reason.
  Conversation fork still works everywhere.
- **Tier 1 — session-start checkpoint (no engine work).** At `loom
  run` start, when the workspace is a git repo with empty `git status
  --porcelain`, record the start HEAD beside the session (additive
  meta). `--restore-workspace --to-start` then restores that snapshot.
  Boundaries mid-session restore conversation only; the picker says so
  at the affected rows.
- **Tier 2 — per-turn before-images (engine work, named seam).
  Shipped 2026-11-06.** The tools layer, which already records
  `files_written`/`files_deleted` at execution time (ADR-012
  invariant: recorded by the tool, never inferred), additionally
  captures the pre-image content of each path it is about to modify
  (or "absent"), content-addressed into the existing artifact store
  (sha256 identity, dedupe for free). Rewind to boundary `b` then
  restores, for every path first touched after `b`, its newest
  pre-image at-or-before `b` — or deletes it if it did not exist.
  `commands_run` effects are surfaced as unrestorable and gate the
  restore unless `--accept-unrestorable` is passed. This is the
  Claude Code file-history shape riding seams that already exist; it
  is an iteration of its own, not a redesign.
  - Captured at the tools seam: `write_file`, `edit_file`,
    `symbols` replace, and LSP-applied edits (`lsp/apply.rs`): the
    plan pass already reads every file it modifies for its undo
    snapshots, so pre-image capture rides those reads. LSP effect
    keys were normalized from absolute to workspace-relative paths in
    the same pass (ADR-012, matching every other write tool), which
    is what makes their pre-images resolvable at restore time. An
    unreadable rename source yields no entry (never a false
    "absent"), and a rolled-back batch leaves no ledger entry. A
    dropped region touching an uncaptured write (e.g. a session run
    with no artifact store) still refuses at that path ("no
    pre-image was captured") rather than guessing.
  - `run` records no pre-images by design: command effects are
    surfaced through the `commands_run` gate instead.

### 5. Safety rule for every tree mutation

Before any restore touches the workspace, write a recovery snapshot
(kept, not garbage collected with run artifacts). Refusal beats
best-effort: the restore either has proof for every affected path or
it does not run.

- Tier 1 (git reset): `git add -A && git diff --cached` into
  `<workspace>/.loom/rewind/<timestamp>.patch`.
- Tier 2 (per-path): copy every affected file into
  `<workspace>/.loom/rewind/<timestamp>.files/` with a
  `manifest.json` naming what existed — git is not required, so the
  snapshot cannot lean on it. A restore that would itself touch
  `.loom/rewind` is refused.

### 6. CLI surface

- `loom rewind <SESSION>` — picker; prints the forked session id and
  the `loom run --session <new> TASK…` continuation line. Each
  boundary row carries a RESTORE column (`yes` / `no` / `cmds(n)`)
  computed from recorded effects.
- `loom rewind <SESSION> --at <index>` — non-interactive fork.
- `--restore-workspace` — tiered: with `--to-start` it is tier 1
  (git start checkpoint); with `--at IDX` it is tier 2 (per-boundary
  pre-images, dirty starts and non-git workspaces included).
  `--accept-unrestorable` proceeds past the `commands_run` gate.
  Default is conversation-only, so the common "retry from turn N"
  never touches the tree.
- Item 22 frontends call the same `fork` + restore helpers through the
  facade; `AwaitingInput` marks the idle points where a rewind needs no
  lease guard.

## Invariants

1. Rewind never destroys: fork only; originals stay listed and
   queryable.
2. Tree mutation writes a recovery snapshot first; refusal beats
   partial restore.
3. Fork copies items in index order in one store transaction, indices
   preserved.
4. Fork provenance is recorded and surfaced.
5. No rewind of a session with a live lease.
6. Restore claims only what effects or git prove; `commands_run` is
   never silently absorbed.

## Testing

- Kernel store tests: fork copies the prefix and preserves indices;
  task records and events not copied; unknown boundary refuses; lease
  refusal; `SqliteStore` and `MemorySessionStore` agree.
- CLI tests: picker rows from a seeded transcript, `--at` fork end to
  end (`loom sessions` shows the marker), tier-0 refusal text,
  recovery snapshot written before a tier-1 restore.
- Tier 2 (shipped): pre-image capture round trip (kernel serde
  back-compat, tools capture with/without store), first-touch-wins
  restore, delete-on-absent, `commands_run` gating, GC keeps
  pre-image bodies, per-file recovery snapshot with manifest.

## Rollout order

1. `SessionStore::fork` + `loom rewind` conversation-only (pure
   store + CLI). Shipped 2026-11-06.
2. Tier-1 start checkpoint recording + `--restore-workspace
   --to-start`. Shipped 2026-11-06.
3. Tier-2 before-images at the tools seam (own iteration, updates
   this note's tier table). Shipped 2026-11-06 for `write_file`,
   `edit_file`, and `symbols`; LSP-applied edits (with absolute→
   relative effect-key normalization) shipped the same day — tier 2
   now covers every loom write path.
