//! density: conversions to base units.

/// How many base units one kilograms_per_cubic_meter_per_gram_per_liter holds.
pub const KILOGRAMS_PER_CUBIC_METER_PER_GRAM_PER_LITER: f64 = 1000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * KILOGRAMS_PER_CUBIC_METER_PER_GRAM_PER_LITER
}
