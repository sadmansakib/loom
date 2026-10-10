//! frequency: conversions to base units.

/// How many base units one hertz_per_kilohertz holds.
pub const HERTZ_PER_KILOHERTZ: f64 = 1000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * HERTZ_PER_KILOHERTZ
}
