//! Entrypoint for the official Loom terminal interface.

fn main() -> anyhow::Result<()> {
    loom_tui::run()
}
