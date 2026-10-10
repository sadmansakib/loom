//! substance: conversions to base units.

/// How many base units one moles_per_kilomole holds.
pub const MOLES_PER_KILOMOLE: f64 = 1000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * MOLES_PER_KILOMOLE
}
