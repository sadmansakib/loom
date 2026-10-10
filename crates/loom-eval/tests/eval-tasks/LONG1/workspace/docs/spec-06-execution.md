# Chapter 6 — The execution model

The evaluator walks the program tree with a tape and two byte streams:

- Reading `,`: pull the next byte from the input; if the input is
  exhausted, the cell is left *unchanged* (chapter 7).
- Writing `.`: append the current cell's byte to the output.

Loops: when the evaluator reaches a `Loop` op it checks the cell under
the pointer. Zero means the body is skipped. Non-zero means the body
runs, and when the body finishes the check repeats. A loop therefore
runs its body zero or more times, and a cell that the body never moves
or mutates will terminate the loop on the second check — the classic
`[-]` idiom clears the current cell by looping until it wraps to zero.

Execution is strictly sequential; there is no pre-pass, no jump
matching at runtime (the parser already validated brackets), and no
step limit. The evaluator maps `Tape::next`/`Tape::prev` failures onto
`RunError::TapeOverflow`/`TapeUnderflow` respectively and stops at the
first fault.

The `hello_world_program_prints_the_greeting` acceptance test is the
canonical end-to-end check: nested loops, arithmetic, and writes.
