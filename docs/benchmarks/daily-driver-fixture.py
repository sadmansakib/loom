#!/usr/bin/env python3
"""Generates the daily-driver probe repos (docs/daily-driver.md).

Usage: python3 daily-driver-fixture.py TARGET_DIR

Writes three fresh single-crate repos under TARGET_DIR — fix/, impl/,
refactor/ — each seeded red (a failing test, a stub, or a compile error).
The probe recreates them before every run and throws them away after.
"""

import os
import sys

MANIFEST = '[package]\nname = "proberepo"\nversion = "0.1.0"\nedition = "2021"\n'

FIX = {
    "src/lib.rs": '''//! Small library under repair.

/// Returns the median of a non-empty slice.
pub fn median(values: &[i64]) -> i64 {
    let mut sorted = values.to_vec();
    sorted.sort();
    let n = sorted.len();
    sorted[n / 2]
}
''',
    "tests/median.rs": '''use proberepo::median;

#[test]
fn odd_takes_middle() {
    assert_eq!(median(&[3, 1, 2]), 2);
}

#[test]
fn even_averages_middle_pair() {
    assert_eq!(median(&[1, 2, 3, 4]), 2);
    assert_eq!(median(&[10, 20]), 15);
}
''',
}

IMPL = {
    "src/lib.rs": '''//! Small library with a missing implementation.

/// Rotates a string right by `n` characters (Unicode code points),
/// wrapping around. Examples: rotate_right("hello", 2) == "lohel",
/// rotate_right("abc", 0) == "abc".
pub fn rotate_right(s: &str, n: usize) -> String {
    todo!("implement rotate_right according to the doc comment")
}
''',
    "tests/rotate.rs": '''use proberepo::rotate_right;

#[test]
fn wraps_around() {
    assert_eq!(rotate_right("hello", 2), "lohel");
}

#[test]
fn zero_is_identity() {
    assert_eq!(rotate_right("abc", 0), "abc");
}

#[test]
fn empty_stays_empty() {
    assert_eq!(rotate_right("", 3), "");
}
''',
}

REFACTOR = {
    "src/lib.rs": '''//! Slug helpers.

/// Joins the words lowercased with the separator, collapsing repeats.
pub fn slugify(words: &[&str], sep: char) -> String {
    words
        .iter()
        .map(|w| w.to_lowercase())
        .collect::<Vec<_>>()
        .join(&sep.to_string())
}

/// Builds a bracketed tag from words.
pub fn tag(words: &[&str]) -> String {
    format!("[{}]", slugify(words, '-'))
}
''',
    "tests/slug.rs": '''use proberepo::{slugify, tag};

#[test]
fn dash_separator() {
    assert_eq!(slugify(&["Hello", "World"], "-"), "hello-world");
}

#[test]
fn underscore_separator() {
    assert_eq!(slugify(&["a", "b"], "_"), "a_b");
}

#[test]
fn tag_uses_dash() {
    assert_eq!(tag(&["x", "y"]), "[x-y]");
}
''',
}

VARIANTS = {"fix": FIX, "impl": IMPL, "refactor": REFACTOR}


def main() -> None:
    root = sys.argv[1] if len(sys.argv) > 1 else "dd-probe"
    for name, files in VARIANTS.items():
        for rel, body in files.items():
            path = os.path.join(root, name, rel)
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "w") as handle:
                handle.write(body)
        with open(os.path.join(root, name, "Cargo.toml"), "w") as handle:
            handle.write(MANIFEST)
    print(f"wrote {sorted(VARIANTS)} under {root}")


if __name__ == "__main__":
    main()
