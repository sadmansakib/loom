//! angle: conversions to base units.

/// How many base units one radians_per_turn holds.
pub const RADIANS_PER_TURN: f64 = 6.283185307179586;

/// Converts a value measured in the prefixed unit to base units.
pub fn to_base(value: f64) -> f64 {
    value * RADIANS_PER_TURN
}
