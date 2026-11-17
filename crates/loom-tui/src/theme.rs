//! Shared design palette for static scenes and the live interface.
//!
//! Every colour in the TUI comes from here: screen modules ask for a token,
//! never an RGB literal, so a palette change is one edit.

use std::env;

use ratatui::style::Color;

pub const BG: Color = Color::Rgb(18, 16, 14);
/// Header bar: one shade above the ground, no border needed.
pub const HEADER: Color = Color::Rgb(22, 18, 16);
pub const PANEL: Color = Color::Rgb(26, 22, 18);
pub const BORDER: Color = Color::Rgb(42, 36, 28);
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

/// The palette as shown by the `--scene spec` reference: name, hex, token.
pub const PALETTE: [(&str, &str, Color); 13] = [
    ("ground", "#12100E", BG),
    ("header", "#161210", HEADER),
    ("border", "#2A241C", BORDER),
    ("ivory", "#E9E3D7", IVORY),
    ("dim", "#9A9285", DIM),
    ("amber", "#E3A857", AMBER),
    ("sage", "#86C19F", SAGE),
    ("coral", "#EE8272", CORAL),
    ("violet", "#A99BD6", VIOLET),
    ("blue", "#8DB4E8", BLUE),
    ("teal", "#6FC4C4", TEAL),
    ("thread", "#8C6B33", THREAD),
    ("dotted", "#5A4A2C", DOTTED),
];

/// Terminal colour capability, in the order the escape codes are chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    TrueColor,
    Ansi256,
    Ansi16,
    NoColor,
}

impl Depth {
    /// Detect the depth from the environment, once per draw.
    pub(crate) fn detect() -> Self {
        let no_color = env::var_os("NO_COLOR").is_some();
        let colorterm = env::var("COLORTERM").ok();
        let term = env::var("TERM").ok();
        Self::from_env(no_color, colorterm.as_deref(), term.as_deref())
    }

    fn from_env(no_color: bool, colorterm: Option<&str>, term: Option<&str>) -> Self {
        if no_color {
            return Self::NoColor;
        }
        if matches!(colorterm, Some("truecolor" | "24bit")) {
            return Self::TrueColor;
        }
        if term.is_some_and(|term| term.contains("256color")) {
            return Self::Ansi256;
        }
        // A terminal with no colour claim is safer at 16 than at RGB.
        Self::Ansi16
    }
}

/// Adapt a design token to the terminal's colour depth.
pub(crate) fn adapt(color: Color, depth: Depth) -> Color {
    match depth {
        Depth::TrueColor => color,
        Depth::NoColor => Color::Reset,
        Depth::Ansi256 => {
            if let Color::Rgb(r, g, b) = color {
                Color::Indexed(nearest_cube(r, g, b))
            } else {
                color
            }
        }
        Depth::Ansi16 => {
            if matches!(color, Color::Rgb(..)) {
                named16(color)
            } else {
                color
            }
        }
    }
}

/// xterm 6×6×6 cube: the nearest level per channel, index 16..=231.
fn nearest_cube(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let nearest = |value: u8| {
        let mut best = 0;
        let mut distance = u16::MAX;
        for (index, level) in LEVELS.iter().enumerate() {
            let delta = u16::from(value.abs_diff(*level));
            if delta < distance {
                best = index;
                distance = delta;
            }
        }
        u8::try_from(best).unwrap_or(0)
    };
    16 + 36 * nearest(r) + 6 * nearest(g) + nearest(b)
}

/// The 16-colour roles. Amber/yellow and violet/magenta are the ones the
/// design names; the rest fall back to the nearest named ANSI colour.
fn named16(color: Color) -> Color {
    if color == AMBER || color == THREAD {
        Color::Yellow
    } else if color == VIOLET {
        Color::Magenta
    } else if color == DIM || color == BORDER || color == DOTTED {
        Color::DarkGray
    } else if color == IVORY {
        Color::White
    } else if color == SAGE {
        Color::Green
    } else if color == CORAL {
        Color::Red
    } else if color == BLUE {
        Color::Blue
    } else if color == TEAL {
        Color::Cyan
    } else {
        // ground, header and panel are dark neutrals
        Color::Black
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_matches_the_reference_design() {
        assert_eq!(BG, Color::Rgb(18, 16, 14));
        assert_eq!(HEADER, Color::Rgb(22, 18, 16));
        assert_eq!(BORDER, Color::Rgb(42, 36, 28));
        assert_eq!(IVORY, Color::Rgb(233, 227, 215));
        assert_eq!(DIM, Color::Rgb(154, 146, 133));
        assert_eq!(AMBER, Color::Rgb(227, 168, 87));
        assert_eq!(SAGE, Color::Rgb(134, 193, 159));
        assert_eq!(CORAL, Color::Rgb(238, 130, 114));
        assert_eq!(VIOLET, Color::Rgb(169, 155, 214));
        assert_eq!(BLUE, Color::Rgb(141, 180, 232));
        assert_eq!(THREAD, Color::Rgb(140, 107, 51));
    }

    #[test]
    fn no_color_drops_every_token_to_the_terminal_default() {
        assert_eq!(adapt(AMBER, Depth::NoColor), Color::Reset);
        assert_eq!(adapt(BG, Depth::NoColor), Color::Reset);
    }

    #[test]
    fn sixteen_colour_keeps_the_role_mapping() {
        assert_eq!(adapt(AMBER, Depth::Ansi16), Color::Yellow);
        assert_eq!(adapt(VIOLET, Depth::Ansi16), Color::Magenta);
        assert_eq!(adapt(DIM, Depth::Ansi16), Color::DarkGray);
    }

    #[test]
    fn ansi256_picks_the_nearest_cube_entry() {
        assert_eq!(adapt(BG, Depth::Ansi256), Color::Indexed(16));
        assert_eq!(adapt(IVORY, Depth::Ansi256), Color::Indexed(188));
        assert_eq!(adapt(AMBER, Depth::Ansi256), Color::Indexed(179));
    }

    #[test]
    fn depth_claim_order_prefers_truecolor_then_256_then_16() {
        assert_eq!(
            Depth::from_env(false, Some("truecolor"), Some("xterm-256color")),
            Depth::TrueColor
        );
        assert_eq!(
            Depth::from_env(false, None, Some("xterm-256color")),
            Depth::Ansi256
        );
        assert_eq!(Depth::from_env(false, None, Some("xterm")), Depth::Ansi16);
        assert_eq!(
            Depth::from_env(true, Some("truecolor"), Some("xterm-256color")),
            Depth::NoColor
        );
    }

    #[test]
    fn colour_literals_live_only_in_the_theme_module() {
        fn collect(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).expect("read src") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    collect(&path, out);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    out.push(path);
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        collect(&root, &mut files);
        for path in files {
            if path.file_name().is_some_and(|name| name == "theme.rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("read source");
            for line in source.lines() {
                let literal = line
                    .split_once("Color::Rgb(")
                    .and_then(|(_, rest)| rest.chars().next());
                assert!(
                    !literal.is_some_and(|character| character.is_ascii_digit()),
                    "{}: {}",
                    path.display(),
                    line.trim()
                );
            }
        }
    }
}
