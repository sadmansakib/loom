# Chapter 2 — The source alphabet and comments

A program is UTF-8 text. Exactly eight characters are instructions:

- `+` increment the current cell
- `-` decrement the current cell
- `>` move the pointer right
- `<` move the pointer left
- `,` read one byte of input into the current cell
- `.` write the current cell to the output
- `[` open a loop
- `]` close a loop

Every other character — letters, digits, punctuation, whitespace — is
ignored by the tokenizer. There is one special case worth naming: `#`
starts a comment. A `#` and everything after it on the same line is
ignored, which is exactly how the tokenizer already treats unknown
characters, so comments need no dedicated machinery. The acceptance test
`comment_lines_are_ignored` interleaves `#` comments with instructions
and expects the program to behave as if only the instructions existed.

Nested brackets form the only structure; there are no procedures, no
labels, and no second data type. When writing programs, prefer `#`
comments to explain the tape layout — the interpreter never sees them.
