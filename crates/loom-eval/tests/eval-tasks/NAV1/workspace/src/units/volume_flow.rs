//! volume_flow: conversions to base units.

/// How many base units one liters_per_second_per_kiloliter_per_second holds.
pub const LITERS_PER_SECOND_PER_KILOLITER_PER_SECOND: f64 = 1000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * LITERS_PER_SECOND_PER_KILOLITER_PER_SECOND
}
