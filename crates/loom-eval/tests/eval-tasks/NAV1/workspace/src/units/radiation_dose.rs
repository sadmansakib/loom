//! radiation_dose: conversions to base units.

/// How many base units one grays_per_kilogray holds.
pub const GRAYS_PER_KILOGRAY: f64 = 1000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * GRAYS_PER_KILOGRAY
}
