//! resistance: conversions to base units.

/// How many base units one ohms_per_kilohm holds.
pub const OHMS_PER_KILOHM: f64 = 1000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * OHMS_PER_KILOHM
}
