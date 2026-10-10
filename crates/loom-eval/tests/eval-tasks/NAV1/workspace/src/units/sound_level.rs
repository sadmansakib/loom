//! sound_level: conversions to base units.

/// How many base units one decibels_per_bel holds.
pub const DECIBELS_PER_BEL: f64 = 10.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * DECIBELS_PER_BEL
}
