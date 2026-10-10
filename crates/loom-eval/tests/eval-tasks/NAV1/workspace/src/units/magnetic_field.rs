//! magnetic_field: conversions to base units.

/// How many base units one gauss_per_tesla holds.
pub const GAUSS_PER_TESLA: f64 = 10_000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * GAUSS_PER_TESLA
}
