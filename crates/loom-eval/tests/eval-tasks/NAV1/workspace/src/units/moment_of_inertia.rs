//! moment_of_inertia: conversions to base units.

/// How many base units one kilogram_square_meters_per_tonne_square_meter holds.
pub const KILOGRAM_SQUARE_METERS_PER_TONNE_SQUARE_METER: f64 = 1000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * KILOGRAM_SQUARE_METERS_PER_TONNE_SQUARE_METER
}
