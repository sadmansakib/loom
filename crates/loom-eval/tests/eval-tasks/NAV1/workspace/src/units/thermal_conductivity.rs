//! thermal_conductivity: conversions to base units.

/// How many base units one milliwatts_per_watt_per_meter_kelvin holds.
pub const MILLIWATTS_PER_WATT_PER_METER_KELVIN: f64 = 1000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * MILLIWATTS_PER_WATT_PER_METER_KELVIN
}
