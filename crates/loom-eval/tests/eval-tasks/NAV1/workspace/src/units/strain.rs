//! strain: conversions to base units.

/// How many base units one microstrains_per_strain holds.
pub const MICROSTRAINS_PER_STRAIN: f64 = 1_000_000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * MICROSTRAINS_PER_STRAIN
}
