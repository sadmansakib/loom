//! torque: conversions to base units.

/// How many base units one newton_meters_per_kilonewton_meter holds.
pub const NEWTON_METERS_PER_KILONEWTON_METER: f64 = 1000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * NEWTON_METERS_PER_KILONEWTON_METER
}
