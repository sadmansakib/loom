//! pressure: conversions to base units.

/// How many base units one pascals_per_bar holds.
pub const PASCALS_PER_BAR: f64 = 10_000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * PASCALS_PER_BAR
}
