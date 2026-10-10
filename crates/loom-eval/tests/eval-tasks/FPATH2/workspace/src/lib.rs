pub mod args;

/// Parses a raw argument string into its ordered options.
pub fn parse_opts(raw: &str) -> Vec<String> {
    raw.split_whitespace().map(str::to_owned).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_whitespace() {
        assert_eq!(parse_opts("a  b\tc"), vec!["a".to_owned(), "b".to_owned(), "c".to_owned()]);
    }

    #[test]
    fn empty_input_yields_nothing() {
        assert!(parse_opts("").is_empty());
    }

    #[test]
    fn cmdline_joins_through_the_helper() {
        assert_eq!(args::cmdline("run  --fast"), "run --fast");
    }
}
