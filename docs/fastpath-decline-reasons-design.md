# Fast-path decline reasons — split the rename coarse reason (roadmap item 34 precondition)

Status: design + implementation. Feature 2 of the 2026-11-07
four-item batch (37, 34, 1, 12).

## Why

Roadmap item 34 (LSP-backed rename fast-path fallback) was deferred
on evidence with a recorded revisit trigger: *rename asks appear in
traces **and** the solver declines on the target-name guard.* That
trigger is unmeasurable today because `FastPathDeclined` collapses
every rename decline into one coarse string, `intent-rename-unsolved`
— collision, zero-occurrence, and the >500-occurrence cap are
indistinguishable in recorded events. Item 34's own deferral entry
names this split as the cheap first step: *"widen it additively first
so the trigger is measurable."* This ships exactly that. The fallback
itself stays deferred until the (now measurable) trigger fires.

## Classification

`apply_rename` (crates/loom-tools/src/fastpath.rs) declines at six
sites; each becomes its own reason string:

| Site | New reason |
| --- | --- |
| target name exists in a touched file | `intent-rename-collision` |
| target name exists anywhere else in the workspace | `intent-rename-collision` |
| zero occurrences of the source name | `intent-rename-zero-occurrences` |
| more than 500 occurrences | `intent-rename-too-many-occurrences` |
| rewrite written, `cargo check` failed, reverted | `intent-rename-verify-failed` |
| a write failed mid-rewrite, reverted | `intent-rename-write-failed` |

`intent-rename-unsolved` remains as the fallback for any unclassified
rename decline (none exist today; the arm stays so the grammar is
total). The other intents (constant, format, dependency) keep their
coarse reasons — item 34's trigger names rename only, and widening
without a trigger population of their own would be speculative.

## Wire compatibility

`FastPathDeclined.reason` is a free `String` on the event wire, so new
values are additive: old readers see an unknown string, nothing fails
to deserialize. Per the project rule (additive event change ⇒ schema
bump), `EVENT_SCHEMA_VERSION` goes 14 → 15 with the change recorded in
the version-history doc comment. No stored transcript is touched (the
event is not a transcript item); replay of older-schema traces follows
the existing `NOT ASSERTABLE` path unchanged.

The measurable trigger, once rename asks occur: any recorded
`fast_path_declined` with `reason == "intent-rename-collision"` —
exactly the LSP-addressable class (item 34).

## Tests (written first)

- `apply_rename` classification: collision in a touched file;
  collision only in an untouched workspace file; zero occurrences;
  >500 occurrences; pre-existing broken crate (rewrite fine, verify
  fails, workspace restored byte-identically) → `intent-rename-verify-failed`.
- A valid rename on a healthy crate still solves (guard against
  over-classifying).
- `no-intent-match` and the other intents' coarse reasons unchanged.
- Event round-trip: the new reason strings serialize and round-trip
  beside the existing `intent-rename-unsolved` case.
