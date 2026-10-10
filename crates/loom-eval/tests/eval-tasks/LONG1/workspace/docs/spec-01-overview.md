# Chapter 1 — Overview and goals

loomfuck is a Brainfuck-flavoured interpreter. It is deliberately small,
but it is specified completely: every behavior the acceptance tests check
is written down in these chapters, and nothing outside them is required.

The pipeline has four stages, one module each:

1. `tokenizer` — source text to positioned tokens (chapter 3).
2. `parser` — flat tokens to a nested program (chapter 4).
3. `tape` — the machine state (chapter 5).
4. `eval` — execution (chapters 6 and 7).

`lib.rs` wires them together through `run(source, input)`, which returns
either the program's output as a `String` or a `RunError`.

The crates's sources are complete except `src/eval.rs`, which carries a
stub: it accepts any program and returns an empty output. Everything the
stub must do instead is specified in chapters 5 through 7. The acceptance
tests under `tests/acceptance.rs` are the gate; the chapters are the
specification those tests were derived from.

Non-goals (chapter 10): optimization, arbitrary tape sizes, non-byte
cells, and anything beyond the eight instructions. Read chapter 2 next
for the alphabet.
