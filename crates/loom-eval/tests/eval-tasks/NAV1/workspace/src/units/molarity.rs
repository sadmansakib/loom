//! molarity: conversions to base units.

/// How many base units one millimoles_per_mole holds.
pub const MILLIMOLES_PER_MOLE: f64 = 1000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * MILLIMOLES_PER_MOLE
}
