# Chapter 7 — Input and output

Input: `run(source, input)` takes the whole input as a `&str`. Reading
`,` consumes one *byte* — not one character: slice the input's bytes
and keep a cursor. When the cursor is past the end, the read leaves the
cell unchanged; in particular the cell is not zeroed and the run does
not fail. The acceptance test `eof_input_leaves_the_cell_unchanged`
runs `,.` on empty input and expects a single `\u{0}` byte out — the
write reports the untouched, zero-initialized cell.

Output: the evaluator collects written bytes into a `Vec<u8>` and
returns `String::from_utf8_lossy(&bytes)` — a valid `String` even when
the program wrote non-UTF-8 bytes. Byte 255 alone is invalid UTF-8 and
appears in the output as U+FFFD, the replacement character
(`non_ascii_bytes_emit_the_lossy_replacement`). Bytes that are valid
UTF-8 (0..=127, and any valid multi-byte sequence a program happens to
emit) pass through unchanged.

The digit adder (`digit_adder_sums_two_digits`) exercises both streams:
two bytes in ("34"), one byte out ("7"), with the loop `[<+>-]` moving
the second cell into the first before the 48 subtractions convert the
digit sum to its character code.
