//! Chapter 4: flat tokens to a nested program.

use crate::tokenizer::{Kind, Token};

/// One operation; loops own their body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Inc,
    Dec,
    Next,
    Prev,
    Read,
    Write,
    Loop(Vec<Op>),
}

/// A whole parsed program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    pub ops: Vec<Op>,
}

/// Why a program could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub offset: usize,
    pub kind: ParseErrorKind,
}

/// The two bracket faults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseErrorKind {
    UnopenedLoop,
    UnclosedLoop,
}

/// Parses tokens into a program, rejecting unbalanced brackets.
pub fn parse(tokens: &[Token]) -> Result<Program, ParseError> {
    let (ops, rest) = parse_ops(tokens)?;
    if let Some(token) = rest.first() {
        return Err(ParseError {
            offset: token.offset,
            kind: ParseErrorKind::UnopenedLoop,
        });
    }
    Ok(Program { ops })
}

fn parse_ops(tokens: &[Token]) -> Result<(Vec<Op>, &[Token]), ParseError> {
    let mut ops = Vec::new();
    let mut rest = tokens;
    while let Some(token) = rest.first() {
        match token.kind {
            Kind::LoopOpen => {
                let (body, tail) = parse_ops(&rest[1..])?;
                match tail.first() {
                    Some(closing) if closing.kind == Kind::LoopClose => {
                        ops.push(Op::Loop(body));
                        rest = &tail[1..];
                    }
                    Some(next) => {
                        return Err(ParseError {
                            offset: next.offset,
                            kind: ParseErrorKind::UnclosedLoop,
                        })
                    }
                    None => {
                        return Err(ParseError {
                            offset: token.offset,
                            kind: ParseErrorKind::UnclosedLoop,
                        })
                    }
                }
            }
            Kind::LoopClose => return Ok((ops, rest)),
            kind => {
                ops.push(match kind {
                    Kind::Inc => Op::Inc,
                    Kind::Dec => Op::Dec,
                    Kind::Next => Op::Next,
                    Kind::Prev => Op::Prev,
                    Kind::Read => Op::Read,
                    Kind::Write => Op::Write,
                    _ => unreachable!("brackets handled above"),
                });
                rest = &rest[1..];
            }
        }
    }
    Ok((ops, rest))
}
