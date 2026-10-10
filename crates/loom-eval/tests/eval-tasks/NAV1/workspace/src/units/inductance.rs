//! inductance: conversions to base units.

/// How many base units one microhenrys_per_millihenry holds.
pub const MICROHENRYS_PER_MILLIHENRY: f64 = 1000.0;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * MICROHENRYS_PER_MILLIHENRY
}
