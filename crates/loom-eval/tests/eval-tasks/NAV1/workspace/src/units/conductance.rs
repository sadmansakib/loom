//! conductance: conversions to base units.

/// How many base units one siemens_per_millisiemens holds.
pub const SIEMENS_PER_MILLISIEMENS: f64 = 1000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * SIEMENS_PER_MILLISIEMENS
}
