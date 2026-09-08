//! Background palette for diff state.
//!
//! Foregrounds come from the syntax theme; these are the backgrounds layered
//! underneath them. Each conflict side gets a tint, and the bytes difftastic
//! flagged as novel get a brighter version of the same tint.

use ratatui::style::Color;

#[derive(Clone, Debug)]
pub struct DiffTheme {
    pub left_bg: Color,
    pub left_emph_bg: Color,
    pub base_bg: Color,
    pub base_emph_bg: Color,
    pub right_bg: Color,
    pub right_emph_bg: Color,
    /// Rows where a file simply has no line, in the side-by-side view.
    pub gap_bg: Color,
    /// Rows belonging to a conflict region that is still undecided.
    pub conflict_bg: Color,
    /// The undecided region currently under the cursor.
    pub conflict_selected_bg: Color,
    /// A region that has been decided.
    pub resolved_bg: Color,
    /// Gutter badge naming the choice on a decided region.
    pub resolved_fg: Color,
    /// The sides not chosen, once a region is decided.
    pub dimmed_fg: Color,
    /// Ground behind those rejected sides.
    pub dimmed_bg: Color,
    /// Title of the panel that currently has focus.
    pub focus_fg: Color,
    pub gutter_fg: Color,
    pub marker_fg: Color,
    pub status_fg: Color,
    pub status_bg: Color,
}

impl Default for DiffTheme {
    fn default() -> Self {
        Self {
            left_bg: Color::Rgb(30, 58, 40),
            left_emph_bg: Color::Rgb(52, 101, 66),
            base_bg: Color::Rgb(48, 48, 48),
            base_emph_bg: Color::Rgb(84, 74, 44),
            right_bg: Color::Rgb(26, 45, 72),
            right_emph_bg: Color::Rgb(40, 74, 120),
            gap_bg: Color::Rgb(26, 26, 26),
            conflict_bg: Color::Rgb(74, 42, 42),
            conflict_selected_bg: Color::Rgb(104, 54, 54),
            resolved_bg: Color::Rgb(32, 52, 44),
            resolved_fg: Color::Rgb(126, 200, 160),
            dimmed_fg: Color::Rgb(96, 96, 96),
            dimmed_bg: Color::Rgb(22, 22, 22),
            focus_fg: Color::Rgb(240, 200, 120),
            gutter_fg: Color::Rgb(110, 110, 110),
            marker_fg: Color::Rgb(180, 160, 90),
            status_fg: Color::Rgb(220, 220, 220),
            status_bg: Color::Rgb(45, 45, 45),
        }
    }
}

/// Which side of a conflict a row belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Base,
    Right,
}

impl Side {
    /// Column order in the side-by-side view.
    pub fn index(self) -> usize {
        match self {
            Self::Left => 0,
            Self::Base => 1,
            Self::Right => 2,
        }
    }

    /// The three sides in column order.
    pub const ALL: [Self; 3] = [Self::Left, Self::Base, Self::Right];
}

impl DiffTheme {
    /// `(block background, emphasis background)` for a conflict side.
    pub fn side_colors(&self, side: Side) -> (Option<Color>, Option<Color>) {
        match side {
            Side::Left => (Some(self.left_bg), Some(self.left_emph_bg)),
            Side::Base => (Some(self.base_bg), Some(self.base_emph_bg)),
            Side::Right => (Some(self.right_bg), Some(self.right_emph_bg)),
        }
    }

    pub fn side_label(side: Side) -> &'static str {
        match side {
            Side::Left => "LEFT",
            Side::Base => "BASE",
            Side::Right => "RIGHT",
        }
    }

    pub fn side_badge(side: Side) -> char {
        match side {
            Side::Left => 'L',
            Side::Base => 'B',
            Side::Right => 'R',
        }
    }
}
