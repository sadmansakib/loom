# Chapter 4 — The program tree

The parser folds the flat token list into a tree. `Program` owns a
`Vec<Op>`; `Op::Loop(Vec<Op>)` owns its body. Loop bodies nest
arbitrarily deep. The tree shape is what the evaluator walks: a loop
op re-executes its body from the top whenever the cell under the
pointer is non-zero when the closing bracket would run.

Two faults reject a program before execution:

- `UnclosedLoop` — a `[` whose matching `]` never arrives. The parser
  reports the offset of the offending `[` when it runs out of tokens,
  or of the point where the mismatch became visible otherwise.
- `UnopenedLoop` — a `]` with no matching `[`. The parser surfaces the
  leftover token to `parse`, which reports its offset.

`lib.rs` maps any parse error onto `RunError::UnbalancedBracket`, so
callers of `run` see one error variant for both bracket faults. The
acceptance test `parser_nests_loops_and_rejects_unbalanced_brackets`
exercises the tree shape for `[+[-]]` and both rejection paths.
