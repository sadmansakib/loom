//! charge: conversions to base units.

/// How many base units one coulombs_per_milliampere_hour holds.
pub const COULOMBS_PER_MILLIAMPERE_HOUR: f64 = 3.6;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * COULOMBS_PER_MILLIAMPERE_HOUR
}
