//! Argument helpers shared by the eval binaries.

/// Splits a comma-separated flag value, trimming and dropping empty parts.
#[must_use]
pub fn csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|part| part.trim().to_owned())
        .filter(|part| !part.is_empty())
        .collect()
}

/// Reads an environment variable as a comma-separated list; unset means empty.
#[must_use]
pub fn env_csv(name: &str) -> Vec<String> {
    std::env::var(name).ok().map_or_default(|v| csv(&v))
}
