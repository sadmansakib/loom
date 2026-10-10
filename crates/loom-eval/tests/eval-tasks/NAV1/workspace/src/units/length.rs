//! length: conversions to base units.

/// How many base units one meters_per_kilometer holds.
pub const METERS_PER_KILOMETER: f64 = 1000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * METERS_PER_KILOMETER
}
