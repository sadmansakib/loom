//! Shared design palette for static scenes and the live interface.

use ratatui::style::Color;

pub const BG: Color = Color::Rgb(18, 16, 14);
pub const PANEL: Color = Color::Rgb(26, 22, 18);
pub const BORDER: Color = Color::Rgb(57, 51, 40);
pub const THREAD: Color = Color::Rgb(140, 107, 51);
pub const IVORY: Color = Color::Rgb(233, 227, 215);
pub const DIM: Color = Color::Rgb(154, 146, 133);
pub const AMBER: Color = Color::Rgb(227, 168, 87);
pub const SAGE: Color = Color::Rgb(134, 193, 159);
pub const CORAL: Color = Color::Rgb(238, 130, 114);
pub const VIOLET: Color = Color::Rgb(169, 155, 214);
pub const BLUE: Color = Color::Rgb(141, 180, 232);
pub const TEAL: Color = Color::Rgb(111, 196, 196);
pub const DOTTED: Color = Color::Rgb(90, 74, 44);

pub const SELECTED: ratatui::style::Style = ratatui::style::Style::new()
    .fg(IVORY)
    .bg(PANEL)
    .add_modifier(ratatui::style::Modifier::BOLD);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_matches_the_reference_design() {
        assert_eq!(BG, Color::Rgb(18, 16, 14));
        assert_eq!(PANEL, Color::Rgb(26, 22, 18));
        assert_eq!(BORDER, Color::Rgb(57, 51, 40));
        assert_eq!(IVORY, Color::Rgb(233, 227, 215));
        assert_eq!(DIM, Color::Rgb(154, 146, 133));
        assert_eq!(AMBER, Color::Rgb(227, 168, 87));
        assert_eq!(SAGE, Color::Rgb(134, 193, 159));
        assert_eq!(CORAL, Color::Rgb(238, 130, 114));
        assert_eq!(VIOLET, Color::Rgb(169, 155, 214));
        assert_eq!(BLUE, Color::Rgb(141, 180, 232));
        assert_eq!(THREAD, Color::Rgb(140, 107, 51));
    }
}
