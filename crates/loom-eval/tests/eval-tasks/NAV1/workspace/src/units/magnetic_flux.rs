//! magnetic_flux: conversions to base units.

/// How many base units one microwebers_per_weber holds.
pub const MICROWEBERS_PER_WEBER: f64 = 1_000_000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * MICROWEBERS_PER_WEBER
}
