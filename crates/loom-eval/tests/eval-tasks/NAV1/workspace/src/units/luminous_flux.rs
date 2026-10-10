//! luminous_flux: conversions to base units.

/// How many base units one lumens_per_kilolumen holds.
pub const LUMENS_PER_KILOLUMEN: f64 = 1000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * LUMENS_PER_KILOLUMEN
}
