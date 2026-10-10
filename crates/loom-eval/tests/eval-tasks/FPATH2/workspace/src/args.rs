/// Formats a command line from the raw argument string via the crate
/// root's parser.
pub fn cmdline(raw: &str) -> String {
    crate::parse_opts(raw).join(" ")
}
