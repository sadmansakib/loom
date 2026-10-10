//! voltage: conversions to base units.

/// How many base units one millivolts_per_volt holds.
pub const MILLIVOLTS_PER_VOLT: f64 = 1000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * MILLIVOLTS_PER_VOLT
}
