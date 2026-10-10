# Chapter 9 — The acceptance walkthrough

The gate is `tests/acceptance.rs`. Every test names the chapter that
specifies it:

- `hello_world_program_prints_the_greeting` — chapters 6 and 7: the
  canonical program, empty input, the full greeting with trailing
  newline.
- `digit_adder_sums_two_digits` — chapter 7: ",>,[<+>-]" plus 48 `-`
  and a `.`. Input "34", output "7".
- `comment_lines_are_ignored` — chapter 2: `#` lines between the 33
  increments and the write.
- `eof_input_leaves_the_cell_unchanged` — chapter 7: `,.` on empty
  input emits the untouched zero cell as `\u{0}`.
- `cell_values_wrap_modulo_256` — chapter 5: 256 increments then a
  write; the byte is 0.
- `non_ascii_bytes_emit_the_lossy_replacement` — chapter 7: 255
  increments then a write; the byte is 0xFF and decodes to U+FFFD.

The last three tests pin the already-complete stages so the evaluator
work cannot regress them: the tokenizer's comment handling, the
parser's nesting and bracket rejection, and the tape's moves, wrapping,
and boundary errors.

When implementing `src/eval.rs`, resist inventing behavior the chapters
do not state; every acceptance check traces to a sentence above.
