//! momentum: conversions to base units.

/// How many base units one newton_seconds_per_kilowton_second holds.
pub const NEWTON_SECONDS_PER_KILOWTON_SECOND: f64 = 1000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * NEWTON_SECONDS_PER_KILOWTON_SECOND
}
