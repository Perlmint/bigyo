//! Everything the viewer needs for *one* file.
//!
//! The highlighter is bound to a language and the differ carries a per-file
//! language override, so both belong here rather than to the app: switching
//! files in directory mode replaces the whole view.
//!
//! A view deliberately does **not** own its [`MergeSession`]. The session lives
//! with the file in the workspace, so resolutions survive moving to another
//! file and back, and so the app can mutate a file's choices and re-render
//! without a borrow conflict.

use std::path::Path;

use anyhow::Result;
use ratatui::style::Color;

use crate::external::difft::{CachingDiffer, DifftCli};
use crate::merge::session::MergeSession;
use crate::render::document::{Document, build_document};
use crate::render::highlight::{Assets, Highlighter};
use crate::render::panes::{PaneDocument, SectionNames, build_panes};
use crate::render::theme::DiffTheme;

pub struct FileView<'a> {
    highlighter: Highlighter<'a>,
    // Rebuilding on every resolution keypress would otherwise spawn a `difft`
    // process per conflict per press.
    differ: CachingDiffer<DifftCli>,
    base: String,
    left: String,
    right: String,
    pub names: SectionNames,
    /// Background from the syntax theme, painted behind unchanged rows.
    pub page_bg: Option<Color>,
    pub syntax: String,
    pub doc: Document,
    pub panes: PaneDocument,
    pub gutter_digits: usize,
    pub pane_digits: usize,
}

impl<'a> FileView<'a> {
    /// Build the view for one file's three revisions.
    ///
    /// `display_path` names the file for the title bars and drives language
    /// detection; `names` is what the four section titles show.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        assets: &'a Assets,
        session: &MergeSession,
        base: String,
        left: String,
        right: String,
        display_path: &Path,
        names: SectionNames,
        language: Option<&str>,
        theme_name: &str,
        theme: &DiffTheme,
    ) -> Result<Self> {
        let highlighter = assets.highlighter(language, Some(display_path), theme_name)?;
        let extension = display_path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_owned);
        let differ = CachingDiffer::new(DifftCli::new(language.map(str::to_owned), extension));

        let mut view = Self {
            page_bg: highlighter.background,
            syntax: highlighter.syntax_name().to_owned(),
            highlighter,
            differ,
            base,
            left,
            right,
            names,
            doc: Document::default(),
            panes: PaneDocument::default(),
            gutter_digits: 3,
            pane_digits: 2,
        };
        view.rebuild(session, theme);
        Ok(view)
    }

    /// Re-render both documents from the session's current resolutions.
    pub fn rebuild(&mut self, session: &MergeSession, theme: &DiffTheme) {
        self.doc = build_document(session, &self.highlighter, &self.differ, theme);
        self.panes = build_panes(
            session,
            &self.base,
            &self.left,
            &self.right,
            &self.highlighter,
            &self.differ,
            theme,
        );
        self.gutter_digits = widest(self.doc.rows.iter().filter_map(|r| r.line_no)).max(3);
        self.pane_digits = widest(
            self.panes
                .rows
                .iter()
                .flat_map(|r| &r.cells)
                .filter_map(|c| c.line_no),
        )
        .max(2);
    }
}

/// Digits needed for the largest line number, or 1 when there are none.
fn widest(numbers: impl Iterator<Item = usize>) -> usize {
    numbers.max().map_or(1, |n| n.to_string().len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external::difft::Differ;
    use crate::merge::diff3::MergedChunk;
    use crate::merge::session::{MarkerLabels, Resolution};
    use crate::render::highlight::DEFAULT_THEME;
    use crate::render::theme::Side;

    const BASE: &str = "fn main() {\n    let a = 1;\n}\n";
    const LEFT: &str = "fn main() {\n    let a = 100;\n}\n";
    const RIGHT: &str = "fn main() {\n    let a = 999;\n}\n";

    fn session() -> MergeSession {
        let own = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        MergeSession::new(
            vec![
                MergedChunk::Resolved {
                    lines: own(&["fn main() {"]),
                },
                MergedChunk::Conflict {
                    left: own(&["    let a = 100;"]),
                    base: own(&["    let a = 1;"]),
                    right: own(&["    let a = 999;"]),
                },
                MergedChunk::Resolved { lines: own(&["}"]) },
            ],
            MarkerLabels::default(),
        )
    }

    fn view<'a>(assets: &'a Assets, session: &MergeSession, path: &str) -> FileView<'a> {
        FileView::new(
            assets,
            session,
            BASE.into(),
            LEFT.into(),
            RIGHT.into(),
            Path::new(path),
            SectionNames::new("left.rs", "base.rs", "right.rs", "merged.rs"),
            None,
            DEFAULT_THEME,
            &DiffTheme::default(),
        )
        .unwrap()
    }

    #[test]
    fn a_view_builds_both_documents() {
        let assets = Assets::new();
        let v = view(&assets, &session(), "src/main.rs");
        assert!(!v.doc.is_empty());
        assert!(!v.panes.is_empty());
        assert_eq!(v.doc.conflicts.len(), 1);
        assert_eq!(v.panes.conflicts.len(), 1);
    }

    #[test]
    fn the_language_comes_from_the_display_path() {
        let assets = Assets::new();
        assert_eq!(view(&assets, &session(), "src/main.rs").syntax, "Rust");
        assert_eq!(view(&assets, &session(), "notes.md").syntax, "Markdown");
        // an unknown extension still opens, just without highlighting
        assert_eq!(view(&assets, &session(), "data.zzz").syntax, "Plain Text");
    }

    #[test]
    fn rebuilding_reflects_a_new_resolution() {
        let assets = Assets::new();
        let mut s = session();
        let mut v = view(&assets, &s, "src/main.rs");
        let theme = DiffTheme::default();

        let region = v.panes.conflicts[0].clone();
        assert_eq!(
            v.panes.rows[region.start].cell(Side::Left).bg,
            Some(theme.conflict_bg)
        );

        s.set_resolution(0, Resolution::Left);
        v.rebuild(&s, &theme);
        assert_eq!(
            v.panes.rows[region.start].cell(Side::Left).bg,
            Some(theme.resolved_bg),
            "the chosen side is lit after a rebuild"
        );
    }

    #[test]
    fn gutter_widths_grow_with_the_line_count() {
        let assets = Assets::new();
        let v = view(&assets, &session(), "src/main.rs");
        assert!(v.gutter_digits >= 3);
        assert!(v.pane_digits >= 2);
    }

    #[test]
    fn the_differ_is_cached_across_rebuilds() {
        // Two rebuilds of the same session must not re-run the differ for the
        // same pair of texts.
        let assets = Assets::new();
        let s = session();
        let mut v = view(&assets, &s, "src/main.rs");
        let theme = DiffTheme::default();
        let before = v.differ.diff(BASE, LEFT);
        v.rebuild(&s, &theme);
        let after = v.differ.diff(BASE, LEFT);
        assert_eq!(before.aligned, after.aligned);
    }

    #[test]
    fn an_unknown_theme_is_an_error_rather_than_a_panic() {
        let assets = Assets::new();
        let s = session();
        let result = FileView::new(
            &assets,
            &s,
            BASE.into(),
            LEFT.into(),
            RIGHT.into(),
            Path::new("a.rs"),
            SectionNames::default(),
            None,
            "no such theme",
            &DiffTheme::default(),
        );
        assert!(result.is_err());
    }
}
