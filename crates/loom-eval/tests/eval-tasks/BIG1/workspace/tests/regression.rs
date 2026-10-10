//! The specification of correct behavior for the toolbox.
use loom_eval_big1::*;

#[test]
fn slugify_separates_words_with_hyphens() {
    assert_eq!(slugify("Hello, Slug World! 2"), "hello-slug-world-2");
}

#[test]
fn snake_case_collapses_punctuation() {
    assert_eq!(snake_case("Hello, World! Again"), "hello_world_again");
}

#[test]
fn capitalize_words_titles_each_word() {
    assert_eq!(capitalize_words("hello WORLD again"), "Hello World Again");
}

#[test]
fn wrap_words_respects_the_width() {
    assert_eq!(wrap_words("aaa bb cc dddd", 7), "aaa bb\ncc dddd");
}

#[test]
fn is_palindrome_ignores_punctuation() {
    assert!(is_palindrome("A man, a plan, a canal: Panama"));
}

#[test]
fn roman_numeral_builds_standard_forms() {
    assert_eq!(roman_numeral(1994), "MCMXCIV");
}

#[test]
fn parse_pair_reads_the_value() {
    assert_eq!(parse_pair("a=1, b=2", "b"), Some("2"));
}
