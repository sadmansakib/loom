//! volume: conversions to base units.

/// How many base units one liters_per_cubic_meter holds.
pub const LITERS_PER_CUBIC_METER: f64 = 1000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * LITERS_PER_CUBIC_METER
}
