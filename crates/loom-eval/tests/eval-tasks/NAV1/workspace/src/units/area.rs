//! area: conversions to base units.

/// How many base units one square_meters_per_hectare holds.
pub const SQUARE_METERS_PER_HECTARE: f64 = 10_000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * SQUARE_METERS_PER_HECTARE
}
