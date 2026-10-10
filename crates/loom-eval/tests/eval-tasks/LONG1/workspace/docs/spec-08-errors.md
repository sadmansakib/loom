# Chapter 8 — Errors and limits

Every failure mode is one of three `RunError` variants:

- `TapeOverflow` — the pointer moved past the last cell.
- `TapeUnderflow` — the pointer moved before the first cell.
- `UnbalancedBracket` — the program failed to parse; `lib.rs` folds the
  parser's two fault kinds into this one variant.

There is no timeout, no step budget, and no memory cap in this
implementation; a program like `+[]` loops forever by design. Callers
that need bounds must impose them outside the interpreter.

Everything else succeeds. An empty program returns `Ok("")`. A program
that writes nothing returns `Ok("")`. A program that reads from an
empty input simply never changes cells on `,`. The acceptance suite
never checks `Debug` formatting or `Display` — the variants are
compared with `PartialEq`.
