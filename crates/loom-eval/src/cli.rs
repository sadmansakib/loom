//! Argument and docker helpers shared by the eval binaries.

use std::process::{Command, Stdio};

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

/// True when the docker image `image` is already present locally.
#[must_use]
pub fn image_exists(image: &str) -> bool {
    Command::new("docker")
        .args(["image", "inspect", image])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}
