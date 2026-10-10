//! data_rate: conversions to base units.

/// How many base units one bytes_per_megabit holds.
pub const BYTES_PER_MEGABIT: f64 = 125_000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * BYTES_PER_MEGABIT
}
