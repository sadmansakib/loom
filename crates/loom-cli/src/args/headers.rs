//! Shared CLI input parsing helpers.

/// Parses `--header` values into validated `(name, value)` pairs.
///
/// Validation lives at this single entry point for CLI input so the
/// engine and adapters can carry pairs verbatim: a name must be an
/// HTTP token, a value visible ASCII — so a pair can never smuggle a
/// second header (or a newline) onto the wire.
pub(crate) fn parse_header_pairs(raw: &[String]) -> Result<Vec<(String, String)>, String> {
    let mut pairs = Vec::with_capacity(raw.len());
    for item in raw {
        let Some((name, value)) = item.split_once('=') else {
            return Err(format!("`{item}`: headers are NAME=VALUE"));
        };
        let name = name.trim();
        if name.is_empty() {
            return Err(format!("`{item}`: empty header name"));
        }
        let token = |c: char| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c);
        if !name.chars().all(token) {
            return Err(format!(
                "`{item}`: header names are HTTP tokens (letters, digits, and !#$%&'*+-.^_`|~)"
            ));
        }
        let value = value.trim();
        if value.is_empty() {
            return Err(format!("`{item}`: empty header value"));
        }
        if !value.chars().all(|c| (' '..='~').contains(&c)) {
            return Err(format!("`{item}`: header values are visible ASCII"));
        }
        pairs.push((name.to_owned(), value.to_owned()));
    }
    Ok(pairs)
}
