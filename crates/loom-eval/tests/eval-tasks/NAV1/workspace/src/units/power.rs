//! power: conversions to base units.

/// How many base units one watts_per_kilowatt holds.
pub const WATTS_PER_KILOWATT: f64 = 1000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * WATTS_PER_KILOWATT
}
