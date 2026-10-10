//! acceleration: conversions to base units.

/// How many base units one meters_per_second_squared_per_g holds.
pub const METERS_PER_SECOND_SQUARED_PER_G: f64 = 9.80665;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * METERS_PER_SECOND_SQUARED_PER_G
}
