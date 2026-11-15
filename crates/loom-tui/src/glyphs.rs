//! Semantic glyphs used by prototypes, with an ASCII fallback table.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlyphSet {
    Unicode,
    Ascii,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Glyph {
    Brand,
    Bullet,
    Check,
    Cross,
    Cancel,
    Retry,
    Spinner,
    Pending,
    Arrow,
    DownArrow,
    Branch,
    Vertical,
    Horizontal,
    Warning,
    Needs,
    Expand,
    Collapse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlyphPair {
    pub glyph: Glyph,
    pub unicode: &'static str,
    pub ascii: &'static str,
}

pub const TABLE: [GlyphPair; 17] = [
    GlyphPair {
        glyph: Glyph::Brand,
        unicode: "◆",
        ascii: "*",
    },
    GlyphPair {
        glyph: Glyph::Bullet,
        unicode: "●",
        ascii: "*",
    },
    GlyphPair {
        glyph: Glyph::Check,
        unicode: "✓",
        ascii: "+",
    },
    GlyphPair {
        glyph: Glyph::Cross,
        unicode: "✗",
        ascii: "x",
    },
    GlyphPair {
        glyph: Glyph::Cancel,
        unicode: "⊘",
        ascii: "/",
    },
    GlyphPair {
        glyph: Glyph::Retry,
        unicode: "↻",
        ascii: "~",
    },
    GlyphPair {
        glyph: Glyph::Spinner,
        unicode: "⠇",
        ascii: "o",
    },
    GlyphPair {
        glyph: Glyph::Pending,
        unicode: "○",
        ascii: "o",
    },
    GlyphPair {
        glyph: Glyph::Arrow,
        unicode: "›",
        ascii: ">",
    },
    GlyphPair {
        glyph: Glyph::DownArrow,
        unicode: "↓",
        ascii: "v",
    },
    GlyphPair {
        glyph: Glyph::Branch,
        unicode: "├",
        ascii: "+",
    },
    GlyphPair {
        glyph: Glyph::Vertical,
        unicode: "│",
        ascii: "|",
    },
    GlyphPair {
        glyph: Glyph::Horizontal,
        unicode: "─",
        ascii: "-",
    },
    GlyphPair {
        glyph: Glyph::Warning,
        unicode: "⚠",
        ascii: "!",
    },
    GlyphPair {
        glyph: Glyph::Needs,
        unicode: "◇",
        ascii: "?",
    },
    GlyphPair {
        glyph: Glyph::Expand,
        unicode: "▸",
        ascii: ">",
    },
    GlyphPair {
        glyph: Glyph::Collapse,
        unicode: "▾",
        ascii: "v",
    },
];

pub fn glyph(target: Glyph, set: GlyphSet) -> &'static str {
    let mut index = 0;
    while index < TABLE.len() {
        let pair = TABLE[index];
        if pair.glyph == target {
            return match set {
                GlyphSet::Unicode => pair.unicode,
                GlyphSet::Ascii => pair.ascii,
            };
        }
        index += 1;
    }
    "?"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_glyph_has_both_representations() {
        assert_eq!(TABLE.len(), 17);
        for pair in TABLE {
            assert_ne!(pair.unicode, "");
            assert_ne!(pair.ascii, "");
            assert_eq!(glyph(pair.glyph, GlyphSet::Unicode), pair.unicode);
            assert_eq!(glyph(pair.glyph, GlyphSet::Ascii), pair.ascii);
        }
    }
}
