//! luminance: conversions to base units.

/// How many base units one candelas_per_kilocandela holds.
pub const CANDELAS_PER_KILOCANDELA: f64 = 1000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * CANDELAS_PER_KILOCANDELA
}
