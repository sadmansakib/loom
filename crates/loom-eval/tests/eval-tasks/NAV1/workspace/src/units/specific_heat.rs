//! specific_heat: conversions to base units.

/// How many base units one kilojoules_per_megajoule_per_kelvin holds.
pub const KILOJOULES_PER_MEGAJOULE_PER_KELVIN: f64 = 1000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * KILOJOULES_PER_MEGAJOULE_PER_KELVIN
}
