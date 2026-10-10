//! illuminance: conversions to base units.

/// How many base units one lux_per_kilolux holds.
pub const LUX_PER_KILOLUX: f64 = 1000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * LUX_PER_KILOLUX
}
