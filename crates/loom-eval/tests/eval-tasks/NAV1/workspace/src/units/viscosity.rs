//! viscosity: conversions to base units.

/// How many base units one millipascal_seconds_per_pascal_second holds.
pub const MILLIPASCAL_SECONDS_PER_PASCAL_SECOND: f64 = 1000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * MILLIPASCAL_SECONDS_PER_PASCAL_SECOND
}
