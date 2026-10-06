//! Pruning decisions for a projected context (eval task B11).
//!
//! Pruning must be episodic: once a session adopts a checkpoint, the
//! projected context is rewritten (a prefix-cache miss), so a new
//! checkpoint is adopted only when it reclaims meaningfully more than
//! the one already carried.

/// Projected size above which pruning is considered.
pub const TRIGGER_TOKENS: u64 = 1_000;

/// A fresh checkpoint must beat the carried one by this many tokens.
pub const MIN_RECLAIM_TOKENS: u64 = 100;

/// What a stubbed message still costs in the projection.
pub const STUB_COST_TOKENS: u64 = 2;

/// How many recent messages are never stubbed.
pub const KEEP_RECENT: usize = 2;

/// A frozen pruning decision: messages `[0, stubbed_through)` are
/// replaced by stubs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Checkpoint {
    pub stubbed_through: usize,
}

/// What the pruner wants to do this turn.
#[derive(PartialEq, Eq, Debug)]
pub enum Decision {
    /// Below the trigger: nothing to do.
    Keep,
    /// Keep the carried checkpoint verbatim: a fresh one would not
    /// reclaim enough more to pay for the prefix-cache miss.
    Carry(Checkpoint),
    /// Adopt a fresh checkpoint.
    Adopt(Checkpoint),
}

/// The projected token count under a carried checkpoint (or raw).
pub fn projected_tokens(messages: &[u64], carried: Option<&Checkpoint>) -> u64 {
    match carried {
        None => messages.iter().sum(),
        Some(c) => {
            c.stubbed_through as u64 * STUB_COST_TOKENS
                + messages[c.stubbed_through.min(messages.len())..]
                    .iter()
                    .sum::<u64>()
        }
    }
}

/// How many tokens stubbing `[0, stubbed_through)` reclaims.
pub fn stub_reclaim(messages: &[u64], stubbed_through: usize) -> u64 {
    let through = stubbed_through.min(messages.len());
    messages[..through].iter().sum::<u64>() - through as u64 * STUB_COST_TOKENS
}

/// The fullest legal checkpoint for these messages.
pub fn optimal_stub_through(messages: &[u64]) -> usize {
    messages.len().saturating_sub(KEEP_RECENT)
}

/// Decides what to do with the projected `messages`, given the
/// checkpoint the session already carries.
pub fn decide(messages: &[u64], carried: Option<&Checkpoint>) -> Decision {
    let raw: u64 = messages.iter().sum();
    if raw <= TRIGGER_TOKENS {
        return Decision::Keep;
    }
    Decision::Adopt(Checkpoint {
        stubbed_through: optimal_stub_through(messages),
    })
}
