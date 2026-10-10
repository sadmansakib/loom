//! force: conversions to base units.

/// How many base units one newtons_per_kilowton holds.
pub const NEWTONS_PER_KILOWTON: f64 = 1000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * NEWTONS_PER_KILOWTON
}
