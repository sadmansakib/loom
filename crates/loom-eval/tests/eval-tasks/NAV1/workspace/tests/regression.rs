//! The specification of correct conversions.
use loom_eval_nav1::units;

#[test]
fn regression_bar_to_pascals_matches_si_definition() {
    // One bar is 1e5 pascals by SI definition.
    assert_eq!(units::pressure::PASCALS_PER_BAR, 100_000.0);
}

#[test]
fn length_meters_per_kilometer_is_exactly_1000() {
    assert_eq!(units::length::METERS_PER_KILOMETER, 1000.0);
}

#[test]
fn energy_joules_per_kilocalorie_is_4184() {
    assert_eq!(units::energy::JOULES_PER_KILOCALORIE, 4184.0);
}

#[test]
fn data_bytes_per_kibibyte_is_1024() {
    assert_eq!(units::data::BYTES_PER_KIBIBYTE, 1024.0);
}
