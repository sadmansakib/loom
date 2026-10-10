//! Chapters 5-7: the evaluator. NOT YET IMPLEMENTED.

use crate::parser::Program;

/// Why a run can fail once the program parses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError {
    TapeOverflow,
    TapeUnderflow,
    UnbalancedBracket,
}

/// Executes a parsed program against the tape, reading `input` and
/// returning every written byte decoded lossily.
pub fn run_program(_program: &Program, _input: &str) -> Result<String, RunError> {
    // TODO(per spec chapters 5-7): walk the operations on the tape,
    // honor the input and output contracts, and map tape faults to
    // RunError. Until then every program "succeeds" with no output.
    Ok(String::new())
}
