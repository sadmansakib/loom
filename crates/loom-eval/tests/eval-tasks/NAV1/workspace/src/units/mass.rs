//! mass: conversions to base units.

/// How many base units one grams_per_kilogram holds.
pub const GRAMS_PER_KILOGRAM: f64 = 1000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * GRAMS_PER_KILOGRAM
}
