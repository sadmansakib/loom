# Chapter 10 — Non-goals and style

Non-goals, explicitly: speed (no scan-run pre-pass, no op fusion, no
JIT), tape sizes other than 4096, cells wider than a byte, signed or
saturating arithmetic, a REPL, a debugger, and a step budget. Any of
those would change observable behavior the acceptance tests pin down.

Style for the evaluator: keep `eval.rs` plain, direct recursion over
`&Op`; the parser already guaranteed balanced brackets, so the
evaluator may assume every `Loop` body is executable without bracket
checks. Errors short-circuit: the first tape fault ends the run with
its `RunError`; partial output is discarded on failure.

The stub's signature is final — `run_program(&Program, &str) ->
Result<String, RunError>` — and the module doc comment stays. Extend
imports as needed, but do not touch the other modules: they are
complete, and the acceptance tests pin their behavior.
