//! Extracting changed paths from `git status --porcelain` output.

/// Extracts changed paths from `git status --porcelain` output.
///
/// Handles the rename form (`R  old -> new`), taking the destination, and
/// sorts/dedupes for determinism.
pub fn parse_porcelain(output: &str) -> Vec<String> {
    let mut paths: Vec<String> = output
        .lines()
        .filter_map(|line| {
            // `XY <path>`: the status is two fixed columns then a space.
            let rest = line.get(3..)?;
            let path = rest.rsplit(" -> ").next().unwrap_or(rest);
            let path = path.trim().trim_matches('"');
            (!path.is_empty()).then(|| path.to_owned())
        })
        .collect();
    paths.sort();
    paths.dedup();
    paths
}
