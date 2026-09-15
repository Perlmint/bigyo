//! Syntax highlighting, using the same syntect grammars and themes bat ships
//! (via `two-face`).
//!
//! Deliberately avoids [`syntect::easy::HighlightLines`]: a conflict's three
//! sides are three alternative continuations of the same point in the file, so
//! the parser state has to be *forked* rather than advanced once. The lower
//! level `ParseState` + `HighlightState` pair is `Clone`, so this drives those
//! directly.

use std::ops::Range;
use std::path::Path;

use anyhow::{Context, Result};
use ratatui::style::Color;
use syntect::highlighting::{
    HighlightIterator, HighlightState, Highlighter as SynHighlighter, Theme,
};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};
use two_face::theme::EmbeddedLazyThemeSet;

/// bat's default dark theme.
pub const DEFAULT_THEME: &str = "Monokai Extended";

/// Owns the syntect grammar and theme sets. Long-lived; [`Highlighter`] borrows
/// from it.
pub struct Assets {
    syntaxes: SyntaxSet,
    themes: EmbeddedLazyThemeSet,
}

impl Default for Assets {
    fn default() -> Self {
        Self::new()
    }
}

impl Assets {
    pub fn new() -> Self {
        Self {
            // `extra_newlines` expects lines to keep their trailing newline,
            // which is what `Highlighter::line` feeds it.
            syntaxes: two_face::syntax::extra_newlines(),
            themes: two_face::theme::extra(),
        }
    }

    pub fn theme_names() -> impl Iterator<Item = &'static str> {
        EmbeddedLazyThemeSet::theme_names()
            .iter()
            .map(|n| n.as_name())
    }

    /// Every syntax name, sorted and deduplicated, for the language picker.
    pub fn syntax_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self
            .syntaxes
            .syntaxes()
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }

    fn theme(&self, name: &str) -> Result<&Theme> {
        let found = EmbeddedLazyThemeSet::theme_names()
            .iter()
            .find(|n| n.as_name().eq_ignore_ascii_case(name))
            .copied()
            .with_context(|| format!("unknown theme `{name}`"))?;
        Ok(self.themes.get(found))
    }

    /// Resolve a syntax from an explicit language name, else the file
    /// extension, else fall back to plain text.
    fn syntax(&self, language: Option<&str>, path: Option<&Path>) -> &SyntaxReference {
        if let Some(lang) = language
            && let Some(s) = self
                .syntaxes
                .find_syntax_by_name(lang)
                .or_else(|| self.syntaxes.find_syntax_by_token(lang))
                .or_else(|| self.syntaxes.find_syntax_by_extension(lang))
        {
            return s;
        }
        // The whole file name first: syntaxes list names like `Makefile` and
        // `Dockerfile` among their extensions, and `CMakeLists.txt` would
        // otherwise be read as plain text on the strength of its `.txt`.
        let by = |name: Option<&std::ffi::OsStr>| {
            name.and_then(|n| n.to_str())
                .and_then(|n| self.syntaxes.find_syntax_by_extension(n))
        };
        if let Some(s) =
            by(path.and_then(|p| p.file_name())).or_else(|| by(path.and_then(|p| p.extension())))
        {
            return s;
        }
        self.syntaxes.find_syntax_plain_text()
    }

    pub fn highlighter(
        &self,
        language: Option<&str>,
        path: Option<&Path>,
        theme_name: &str,
    ) -> Result<Highlighter<'_>> {
        let theme = self.theme(theme_name)?;
        Ok(Highlighter {
            syntaxes: &self.syntaxes,
            syntax: self.syntax(language, path),
            highlighter: SynHighlighter::new(theme),
            background: theme.settings.background.map(to_ratatui_color),
        })
    }
}

/// Parser and highlighter state at one point in a document. Cloned to fork a
/// conflict's three sides off a common prefix.
#[derive(Clone)]
pub struct HlState {
    parse: ParseState,
    highlight: HighlightState,
}

pub struct Highlighter<'a> {
    syntaxes: &'a SyntaxSet,
    syntax: &'a SyntaxReference,
    highlighter: SynHighlighter<'a>,
    /// The syntax theme's own background, for the TUI to paint behind context
    /// rows so the view reads like bat.
    pub background: Option<Color>,
}

impl Highlighter<'_> {
    pub fn syntax_name(&self) -> &str {
        &self.syntax.name
    }

    pub fn new_state(&self) -> HlState {
        HlState {
            parse: ParseState::new(self.syntax),
            highlight: HighlightState::new(&self.highlighter, ScopeStack::new()),
        }
    }

    /// Highlight one line (given without its trailing newline), advancing
    /// `state` to the start of the next line.
    ///
    /// Returns `(byte range, foreground)` pairs covering `0..line.len()`.
    pub fn line(&self, state: &mut HlState, line: &str) -> Vec<(Range<usize>, Color)> {
        // The grammars in `extra_newlines` want the newline to close off
        // line-terminated contexts such as `//` comments.
        let with_newline = format!("{line}\n");
        let Ok(ops) = state.parse.parse_line(&with_newline, self.syntaxes) else {
            return Vec::new();
        };

        let mut out = Vec::new();
        let mut offset = 0usize;
        for (style, piece) in
            HighlightIterator::new(&mut state.highlight, &ops, &with_newline, &self.highlighter)
        {
            let start = offset;
            offset += piece.len();
            // Trim the synthetic newline back off.
            let end = offset.min(line.len());
            if start < end {
                out.push((start..end, to_ratatui_color(style.foreground)));
            }
        }
        out
    }

    /// Highlight a whole file from a fresh state, one entry per line.
    ///
    /// The side-by-side view needs each column highlighted in its own file's
    /// order, which is simpler than the merged view's fork-at-each-conflict.
    pub fn file(&self, lines: &[&str]) -> Vec<Vec<(Range<usize>, Color)>> {
        let mut state = self.new_state();
        lines
            .iter()
            .map(|line| self.line(&mut state, line))
            .collect()
    }
}

fn to_ratatui_color(c: syntect::highlighting::Color) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_cover_the_whole_line_without_the_newline() {
        let assets = Assets::new();
        let hl = assets
            .highlighter(Some("rust"), None, DEFAULT_THEME)
            .unwrap();
        let mut st = hl.new_state();

        let line = "fn main() {";
        let ranges = hl.line(&mut st, line);
        assert!(!ranges.is_empty());
        assert_eq!(ranges.first().unwrap().0.start, 0);
        assert_eq!(ranges.last().unwrap().0.end, line.len());
        // contiguous, non-overlapping
        for pair in ranges.windows(2) {
            assert_eq!(pair[0].0.end, pair[1].0.start);
        }
    }

    #[test]
    fn keyword_and_string_get_different_foregrounds() {
        let assets = Assets::new();
        let hl = assets
            .highlighter(Some("rust"), None, DEFAULT_THEME)
            .unwrap();
        let mut st = hl.new_state();
        let line = r#"let s = "hi";"#;
        let ranges = hl.line(&mut st, line);
        let color_at = |i: usize| ranges.iter().find(|(r, _)| r.contains(&i)).map(|(_, c)| *c);
        assert_ne!(color_at(0), color_at(9), "`let` and `hi` should differ");
    }

    #[test]
    fn cjk_line_ranges_are_valid_byte_boundaries() {
        let assets = Assets::new();
        let hl = assets
            .highlighter(Some("rust"), None, DEFAULT_THEME)
            .unwrap();
        let mut st = hl.new_state();
        let line = "    let 인사 = \"안녕하세요 世界\";";
        let ranges = hl.line(&mut st, line);
        assert_eq!(ranges.last().unwrap().0.end, line.len());
        for (r, _) in &ranges {
            assert!(line.is_char_boundary(r.start) && line.is_char_boundary(r.end));
        }
    }

    #[test]
    fn state_can_be_forked_to_highlight_alternative_continuations() {
        let assets = Assets::new();
        let hl = assets
            .highlighter(Some("rust"), None, DEFAULT_THEME)
            .unwrap();
        let mut st = hl.new_state();
        hl.line(&mut st, "fn main() {");

        let mut a = st.clone();
        let mut b = st;
        assert_eq!(
            hl.line(&mut a, "    let x = 1;"),
            hl.line(&mut b, "    let x = 1;")
        );
    }

    #[test]
    fn a_name_without_an_extension_is_still_recognised() {
        let assets = Assets::new();
        let syntax = |p: &str| {
            assets
                .highlighter(None, Some(Path::new(p)), DEFAULT_THEME)
                .unwrap()
                .syntax_name()
                .to_owned()
        };
        assert_eq!(syntax("Makefile"), "Makefile");
        assert_eq!(syntax("src/Makefile"), "Makefile");
        // and a name whose extension lies about it
        assert_eq!(syntax("CMakeLists.txt"), "CMake");
    }

    #[test]
    fn unknown_language_falls_back_to_plain_text() {
        let assets = Assets::new();
        let hl = assets
            .highlighter(Some("no-such-language"), None, DEFAULT_THEME)
            .unwrap();
        assert_eq!(hl.syntax_name(), "Plain Text");
    }

    #[test]
    fn language_resolves_from_the_path_extension() {
        let assets = Assets::new();
        let hl = assets
            .highlighter(None, Some(Path::new("a/b/main.rs")), DEFAULT_THEME)
            .unwrap();
        assert_eq!(hl.syntax_name(), "Rust");
    }

    #[test]
    fn syntax_names_are_sorted_and_unique() {
        let assets = Assets::new();
        let names = assets.syntax_names();
        assert!(names.len() > 50, "bat ships a lot of syntaxes");
        assert!(names.contains(&"Rust"), "{names:?}");
        assert!(names.contains(&"Markdown"));

        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
        let mut deduped = names.clone();
        deduped.dedup();
        assert_eq!(names, deduped);
    }

    #[test]
    fn unknown_theme_is_an_error() {
        assert!(
            Assets::new()
                .highlighter(None, None, "no-such-theme")
                .is_err()
        );
    }

    #[test]
    fn empty_line_yields_no_ranges() {
        let assets = Assets::new();
        let hl = assets
            .highlighter(Some("rust"), None, DEFAULT_THEME)
            .unwrap();
        let mut st = hl.new_state();
        assert!(hl.line(&mut st, "").is_empty());
    }
}
