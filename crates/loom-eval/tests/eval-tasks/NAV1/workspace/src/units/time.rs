//! time: conversions to base units.

/// How many base units one seconds_per_hour holds.
pub const SECONDS_PER_HOUR: f64 = 3600.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * SECONDS_PER_HOUR
}
