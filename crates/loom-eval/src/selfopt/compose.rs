//! Composition: merging adopted overlays into one overlay, naming
//! conflicts instead of silently overwriting.

use super::knobs::Overlay;

/// Merges adopted overlays in adoption order. Conflicting values for
/// the same knob abort the merge with the conflict named — Harness²
/// composes selectively, and a silent last-write-wins would hide which
/// won. (The binary re-probes the merged overlay either way.)
pub fn merge_overlays(overlays: &[&Overlay]) -> Result<Overlay, String> {
    let mut merged = Overlay::new();
    for overlay in overlays.iter().copied() {
        for (env, value) in overlay {
            if let Some(existing) = merged.get(env) {
                if existing != value {
                    return Err(format!("{env}: {existing} vs {value}"));
                }
                continue;
            }
            merged.insert(env.clone(), value.clone());
        }
    }
    Ok(merged)
}
