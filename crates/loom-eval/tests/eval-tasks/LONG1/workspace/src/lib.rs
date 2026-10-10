//! loomfuck: a Brainfuck-flavoured interpreter built to the spec
//! chapters under docs/. See docs/spec-01-overview.md first.

pub mod eval;
pub mod parser;
pub mod tape;
pub mod tokenizer;

pub use eval::{RunError, run_program};

/// Runs a program end to end: lex, parse, execute.
pub fn run(source: &str, input: &str) -> Result<String, RunError> {
    let tokens = tokenizer::tokenize(source);
    let program = parser::parse(&tokens)
        .map_err(|_| RunError::UnbalancedBracket)?;
    run_program(&program, input)
}
