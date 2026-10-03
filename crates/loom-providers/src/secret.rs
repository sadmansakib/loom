//! Secret strings that never leak through `Debug`.

use std::fmt;

/// A secret string (API key, token) whose [`fmt::Debug`] output is
/// always `***`.
///
/// ADR-009: secrets use a redacting wrapper — no `Serialize`
/// (accidental persistence fails to compile), and `Debug` output can be
/// logged, panicked, or `dbg!`-ed without exposing the value. Access
/// the inner value only at the point of use via [`expose`](Self::expose).
#[derive(Clone, PartialEq, Eq)]
pub struct SecretString(String);

impl SecretString {
    /// Wraps a secret value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret value, for use at the point of authentication only.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("***")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_never_contains_the_secret() {
        let secret = SecretString::new("sk-live-abcdef");
        let rendered = format!("{secret:?}");
        assert_eq!(rendered, "***");
        assert!(!rendered.contains("sk-live-abcdef"));
        assert_eq!(secret.expose(), "sk-live-abcdef");
    }
}
