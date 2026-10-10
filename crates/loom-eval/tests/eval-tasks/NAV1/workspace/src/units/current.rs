//! current: conversions to base units.

/// How many base units one milliamps_per_amp holds.
pub const MILLIAMPS_PER_AMP: f64 = 1000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * MILLIAMPS_PER_AMP
}
