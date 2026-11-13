//! `loom tui`: CLI composition-root adapter for the official TUI.

use crate::args::{RunArgs, TuiArgs, TuiScene};

/// Launch the interactive picker, or render a fixed prototype scene.
pub fn run(args: &TuiArgs) -> anyhow::Result<()> {
    if let Some(scene) = args.scene {
        return loom_tui::render_scene(scene.into());
    }

    // `loom tui` intentionally has no provider setup of its own. Provider and
    // model selection belong to the command-line run flags and are passed
    // directly to the TUI when launching a task.
    loom_tui::run_with_options(None, None, None, None, None, None)
}

/// Launch a task from `loom run --tui` with the shared run configuration.
pub fn run_task(args: &RunArgs) -> anyhow::Result<()> {
    let task = args.task.join(" ");
    let (config, overrides) = crate::run::engine_config_from(&args.flags)?;
    loom_tui::run_with_config_and_overrides(Some(task), config, overrides)
}

impl From<TuiScene> for loom_tui::Scene {
    fn from(scene: TuiScene) -> Self {
        match scene {
            TuiScene::Main => Self::Main,
            TuiScene::Cancel => Self::Cancel,
            TuiScene::Retry => Self::Retry,
            TuiScene::Complete => Self::Complete,
            TuiScene::Picker => Self::Picker,
            TuiScene::Help => Self::Help,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scene_mapping_covers_all_cli_scene_variants() {
        assert_eq!(loom_tui::Scene::from(TuiScene::Main), loom_tui::Scene::Main);
        assert_eq!(
            loom_tui::Scene::from(TuiScene::Cancel),
            loom_tui::Scene::Cancel
        );
        assert_eq!(
            loom_tui::Scene::from(TuiScene::Retry),
            loom_tui::Scene::Retry
        );
        assert_eq!(
            loom_tui::Scene::from(TuiScene::Complete),
            loom_tui::Scene::Complete
        );
        assert_eq!(
            loom_tui::Scene::from(TuiScene::Picker),
            loom_tui::Scene::Picker
        );
        assert_eq!(loom_tui::Scene::from(TuiScene::Help), loom_tui::Scene::Help);
    }
}
