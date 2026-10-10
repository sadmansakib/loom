//! speed: conversions to base units.

/// How many base units one meters_per_second_per_knot holds.
pub const METERS_PER_SECOND_PER_KNOT: f64 = 0.5144;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * METERS_PER_SECOND_PER_KNOT
}
