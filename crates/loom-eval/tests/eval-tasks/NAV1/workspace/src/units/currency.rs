//! currency: conversions to base units.

/// How many base units one cents_per_dollar holds.
pub const CENTS_PER_DOLLAR: f64 = 100.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * CENTS_PER_DOLLAR
}
