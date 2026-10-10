/// How many times a flaky probe may be attempted before giving up.
pub const MAX_RETRIES: u32 = 3;

/// Runs `probe` until it succeeds or the retry ceiling is reached.
///
/// Returns the number of attempts spent, so a caller can tell a
/// first-try success (1) from a lucky last try (`MAX_RETRIES`).
pub fn attempt<F>(mut probe: F) -> u32
where
    F: FnMut() -> bool,
{
    let mut tried = 0;
    while tried < MAX_RETRIES {
        tried += 1;
        if probe() {
            return tried;
        }
    }
    tried
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gives_up_at_the_ceiling() {
        let mut calls = 0;
        let used = attempt(|| {
            calls += 1;
            false
        });
        assert_eq!(used, MAX_RETRIES);
        assert_eq!(calls, MAX_RETRIES);
    }

    #[test]
    fn fifth_try_succeeds() {
        let mut calls = 0;
        let used = attempt(|| {
            calls += 1;
            calls == 5
        });
        assert_eq!(used, 5);
    }
}
