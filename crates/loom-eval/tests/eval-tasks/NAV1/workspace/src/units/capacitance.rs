//! capacitance: conversions to base units.

/// How many base units one picofarads_per_microfarad holds.
pub const PICOFARADS_PER_MICROFARAD: f64 = 1_000_000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * PICOFARADS_PER_MICROFARAD
}
