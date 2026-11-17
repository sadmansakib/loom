//! Shared modal chrome: a centered, amber-bordered box over a dimmed scene.
//! The wireframes call for 980 px at ~7.8 px per cell, which is about 125 cols.

use ratatui::{Frame, layout::Rect, style::Color};

use crate::theme;

/// Widest modal in cells: the wireframe's 980 px at 7.8 px per column.
const MAX_WIDTH: u16 = 125;

/// Centered modal rectangle inside `area`, capped at [`MAX_WIDTH`] columns.
pub(crate) fn centered(area: Rect, height: u16) -> Rect {
    let width = area.width.min(MAX_WIDTH);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// Dim every cell in `area` toward the ground color, then clear the modal rect.
pub(crate) fn dim_scene(frame: &mut Frame<'_>, area: Rect, modal: Rect) {
    let buffer = frame.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let inside =
                x >= modal.left() && x < modal.right() && y >= modal.top() && y < modal.bottom();
            if !inside {
                let cell = &mut buffer[(x, y)];
                cell.set_fg(dim(cell.fg));
            }
        }
    }
}

/// Blend a foreground color 82% toward the ground: the wireframe's overlay.
fn dim(color: Color) -> Color {
    match (color, theme::BG) {
        (Color::Rgb(r, g, b), Color::Rgb(br, bg, bb)) => {
            Color::Rgb(blend(r, br), blend(g, bg), blend(b, bb))
        }
        _ => color,
    }
}

fn blend(value: u8, ground: u8) -> u8 {
    let value = u16::from(value);
    let ground = u16::from(ground);
    u8::try_from((value * 18 + ground * 82) / 100).unwrap_or(u8::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modal_is_centered_and_capped_at_wireframe_width() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 200,
            height: 40,
        };
        let modal = centered(area, 5);
        assert_eq!(modal.width, MAX_WIDTH);
        assert_eq!(modal.x, (200 - MAX_WIDTH) / 2);
        assert_eq!(modal.height, 5);
        assert_eq!(modal.y, (40 - 5) / 2);
    }

    #[test]
    fn dim_moves_foreground_82_percent_toward_ground() {
        let dimmed = dim(theme::IVORY);
        assert_ne!(dimmed, theme::IVORY);
        if let Color::Rgb(r, g, b) = dimmed {
            assert!(r < 233 && g < 227 && b < 215);
        } else {
            assert_eq!(dimmed, theme::IVORY);
        }
    }
}
