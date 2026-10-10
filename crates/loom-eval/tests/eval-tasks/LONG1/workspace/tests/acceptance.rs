//! The acceptance gate: everything here is specified by the chapters
//! under docs/. tokenizer, parser, and tape are already complete.

use loom_eval_long1::tokenizer::{Kind, tokenize};
use loom_eval_long1::tape::{Tape, TapeError};
use loom_eval_long1::{parser, run};

const HELLO: &str = "++++++++[>++++[>++>+++>+++>+<<<<-]>+>+>->>+[<]<-]>>.>---.+++++++..+++.>>.<-.<.+++.------.--------.>>+.>++.";

#[test]
fn hello_world_program_prints_the_greeting() {
    assert_eq!(run(HELLO, ""), Ok("Hello World!\n".to_owned()));
}

#[test]
fn digit_adder_sums_two_digits() {
    let program = ",>,[<+>-]<------------------------------------------------.";
    assert_eq!(run(program, "34"), Ok("7".to_owned()));
}

#[test]
fn comment_lines_are_ignored() {
    // '#' comments carry no instructions (chapter 2).
    let program = "# setup\n+++++++++++++++++++++++++++++++++# print it\n.";
    assert_eq!(run(program, ""), Ok("!".to_owned()));
}

#[test]
fn eof_input_leaves_the_cell_unchanged() {
    assert_eq!(run(",.", ""), Ok("\u{0}".to_owned()));
}

#[test]
fn cell_values_wrap_modulo_256() {
    let program = format!("{}.", "+".repeat(256));
    assert_eq!(run(&program, ""), Ok("\u{0}".to_owned()));
}

#[test]
fn non_ascii_bytes_emit_the_lossy_replacement() {
    let program = format!("{}.", "+".repeat(255));
    assert_eq!(run(&program, ""), Ok("\u{FFFD}".to_owned()));
}

#[test]
fn tokenizer_ignores_unknown_characters() {
    let kinds: Vec<_> = tokenize("a+b#z\n-")
        .into_iter()
        .map(|token| token.kind)
        .collect();
    assert_eq!(kinds, vec![Kind::Inc, Kind::Dec]);
}

#[test]
fn parser_nests_loops_and_rejects_unbalanced_brackets() {
    let nested = parser::parse(&tokenize("[+[-]]")).unwrap();
    assert_eq!(nested.ops.len(), 1);
    let unbalanced = parser::parse(&tokenize("["));
    assert!(unbalanced.is_err());
    let unopened = parser::parse(&tokenize("]"));
    assert!(unopened.is_err());
}

#[test]
fn tape_moves_and_wraps_values() {
    let mut tape = Tape::new();
    assert_eq!(tape.value(), 0);
    for _ in 0..3 {
        tape.inc();
    }
    tape.dec();
    assert_eq!(tape.value(), 2);
    assert_eq!(tape.pointer(), 0);
    assert_eq!(tape.prev(), Err(TapeError::Underflow));
    tape.next().unwrap();
    assert_eq!(tape.pointer(), 1);
    for _ in 0..256 {
        tape.inc();
    }
    assert_eq!(tape.value(), 0);
}
