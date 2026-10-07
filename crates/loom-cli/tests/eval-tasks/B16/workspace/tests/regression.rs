use loom_eval_b16::{should_prune, trigger_tokens, PruneConfig};

#[test]
fn trigger_scales_with_a_large_window() {
    let config = PruneConfig {
        trigger_percent: 80,
        floor_tokens: 64_000,
    };
    assert_eq!(trigger_tokens(&config, 1_000_000), 800_000);
}

#[test]
fn an_extreme_window_saturates_instead_of_overflowing() {
    let config = PruneConfig {
        trigger_percent: 80,
        floor_tokens: 64_000,
    };
    assert_eq!(trigger_tokens(&config, u32::MAX), 42_949_672);
}

#[test]
fn should_prune_uses_the_real_trigger_not_the_floor() {
    let config = PruneConfig {
        trigger_percent: 10,
        floor_tokens: 1_000,
    };
    // trigger = 10% of 200_000 = 20_000; 15_000 is well under it.
    assert!(!should_prune(15_000, &config, 200_000));
}

#[test]
fn a_small_window_still_gets_the_floor() {
    let config = PruneConfig {
        trigger_percent: 80,
        floor_tokens: 64_000,
    };
    // 80% of 10_000 is 8_000, under the floor.
    assert_eq!(trigger_tokens(&config, 10_000), 64_000);
}

#[test]
fn zero_percent_trigger_is_the_floor() {
    let config = PruneConfig {
        trigger_percent: 0,
        floor_tokens: 5_000,
    };
    assert_eq!(trigger_tokens(&config, 100_000), 5_000);
}

#[test]
fn the_trigger_is_strict_so_equality_does_not_prune() {
    let config = PruneConfig {
        trigger_percent: 0,
        floor_tokens: 50_000,
    };
    assert!(!should_prune(50_000, &config, 100_000));
}

#[test]
fn a_tiny_total_never_triggers_regardless_of_window() {
    let config = PruneConfig {
        trigger_percent: 80,
        floor_tokens: 1_000,
    };
    assert!(!should_prune(100, &config, 100));
}
