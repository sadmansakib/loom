# Chapter 5 — The tape

The tape is 4096 cells of `u8`, a pointer starting at cell zero, and
nothing else. `CELLS` is the authority for the size; do not hardcode
4096 anywhere else.

Movement contract:

- `>` (`Tape::next`) moves right. The last cell has no right neighbor:
  moving past it fails with `Overflow`, mapped by the evaluator to
  `RunError::TapeOverflow`.
- `<` (`Tape::prev`) moves left. Cell zero has no left neighbor: the
  move fails with `Underflow`, mapped to `RunError::TapeUnderflow`.

Cell arithmetic wraps modulo 256 in both directions: `+` on 255 yields
0, and `-` on 0 yields 255. The wrapping is total — it can never fail.

The acceptance tests pin three behaviors: 256 increments land back on
zero (`cell_values_wrap_modulo_256`), 255 increments produce a byte
that is not valid UTF-8 on its own, and moves outside the tape are
errors, not wraps, not clamps. There is no command to resize the tape.
