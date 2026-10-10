//! fuel_economy: conversions to base units.

/// How many base units one liters_per_gallon holds.
pub const LITERS_PER_GALLON: f64 = 3.785411784;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * LITERS_PER_GALLON
}
