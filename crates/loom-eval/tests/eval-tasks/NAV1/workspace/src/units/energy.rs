//! energy: conversions to base units.

/// How many base units one joules_per_kilocalorie holds.
pub const JOULES_PER_KILOCALORIE: f64 = 4184.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * JOULES_PER_KILOCALORIE
}
