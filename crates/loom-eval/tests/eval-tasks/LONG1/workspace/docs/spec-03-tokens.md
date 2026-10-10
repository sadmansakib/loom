# Chapter 3 — Tokens and positions

`tokenize` walks the source with `char_indices` so every token carries
the byte offset of its instruction character. Offsets matter for one
consumer: the parser's errors report where a bracket fault lives, and
the tests check the error *kinds*, not offsets, so precision beyond
"the offset of the offending character" is not required.

The token type is `(offset, Kind)` where `Kind` is one of the eight
instructions. Unknown characters produce no token at all — not an error,
not a marker. A program of pure comments tokenizes to an empty vector
and parses to an empty program, which the evaluator must treat as a
successful run producing no output.

Positions are byte offsets into the original source, not character
indices, not line/column pairs. Nothing downstream depends on the exact
values; if a future revision changes to line/column, only the parser
error reports change shape.
