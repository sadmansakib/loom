//! surface_tension: conversions to base units.

/// How many base units one dynes_per_newton holds.
pub const DYNES_PER_NEWTON: f64 = 100_000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * DYNES_PER_NEWTON
}
