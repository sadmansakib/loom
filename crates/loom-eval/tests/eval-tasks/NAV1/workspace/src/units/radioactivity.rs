//! radioactivity: conversions to base units.

/// How many base units one becquerels_per_kilobecquerel holds.
pub const BECQUERELS_PER_KILOBECQUEREL: f64 = 1000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * BECQUERELS_PER_KILOBECQUEREL
}
