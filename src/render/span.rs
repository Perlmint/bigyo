//! The span list generator: fuses syntax-highlight ranges (foreground) with
//! diff emphasis ranges (background) into a minimal list of styled spans.

use std::ops::Range;

use ratatui::style::Color;

/// A contiguous run of text sharing one foreground and one background color.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StyledSpan {
    pub text: String,
    pub fg: Option<Color>,
    pub bg: Option<Color>,
}

impl StyledSpan {
    pub fn new(text: impl Into<String>, fg: Option<Color>, bg: Option<Color>) -> Self {
        Self {
            text: text.into(),
            fg,
            bg,
        }
    }
}

/// Largest char boundary `<= i`.
fn floor_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Smallest char boundary `>= i`.
fn ceil_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Sort and merge overlapping *or touching* ranges. Empty ranges are dropped.
///
/// Difftastic emits one entry per novel token, so `18..24, 24..25, 25..31`
/// arrives as three ranges that describe one contiguous change.
pub fn coalesce(ranges: &[Range<usize>]) -> Vec<Range<usize>> {
    let mut sorted: Vec<Range<usize>> =
        ranges.iter().filter(|r| r.start < r.end).cloned().collect();
    sorted.sort_unstable_by_key(|r| (r.start, r.end));

    let mut out: Vec<Range<usize>> = Vec::with_capacity(sorted.len());
    for r in sorted {
        match out.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => out.push(r),
        }
    }
    out
}

/// Clamp ranges into `0..text.len()`, snap endpoints outward to char
/// boundaries, then [`coalesce`] them.
///
/// Difftastic reports byte offsets, so a range never *should* land mid-UTF-8 —
/// but a stale or mismatched line would panic on slicing, and a viewer has no
/// business crashing over that.
pub fn normalize_ranges(text: &str, ranges: &[Range<usize>]) -> Vec<Range<usize>> {
    let len = text.len();
    let snapped: Vec<Range<usize>> = ranges
        .iter()
        .map(|r| {
            let start = floor_boundary(text, r.start.min(len));
            let end = ceil_boundary(text, r.end.min(len));
            start..end.max(start)
        })
        .collect();
    coalesce(&snapped)
}

/// Fuse syntax highlighting with diff emphasis into a coalesced span list.
///
/// `highlights` are `(byte range, foreground)` pairs from the syntax
/// highlighter, expected to be sorted and to cover `0..text.len()`; bytes not
/// covered by any highlight get no foreground.
///
/// `emphasis` are difftastic's novel byte ranges for this line. They may be
/// unsorted, overlapping, adjacent, out of range, or (defensively) misaligned —
/// [`normalize_ranges`] deals with all of that.
///
/// `base_bg` paints the whole line, `emphasis_bg` paints the changed bytes.
pub fn overlay_spans(
    text: &str,
    highlights: &[(Range<usize>, Color)],
    emphasis: &[Range<usize>],
    base_bg: Option<Color>,
    emphasis_bg: Option<Color>,
) -> Vec<StyledSpan> {
    let len = text.len();
    if len == 0 {
        return Vec::new();
    }

    let emph = normalize_ranges(text, emphasis);

    let mut bounds: Vec<usize> = Vec::with_capacity(2 * (highlights.len() + emph.len()) + 2);
    bounds.push(0);
    bounds.push(len);
    for (r, _) in highlights {
        bounds.push(floor_boundary(text, r.start.min(len)));
        bounds.push(ceil_boundary(text, r.end.min(len)));
    }
    for r in &emph {
        bounds.push(r.start);
        bounds.push(r.end);
    }
    bounds.sort_unstable();
    bounds.dedup();

    let mut spans: Vec<StyledSpan> = Vec::new();
    for w in bounds.windows(2) {
        let (a, b) = (w[0], w[1]);
        let fg = highlights
            .iter()
            .find(|(r, _)| r.start <= a && a < r.end)
            .map(|(_, c)| *c);
        let bg = if emph.iter().any(|r| r.start <= a && a < r.end) {
            emphasis_bg
        } else {
            base_bg
        };

        match spans.last_mut() {
            Some(last) if last.fg == fg && last.bg == bg => last.text.push_str(&text[a..b]),
            _ => spans.push(StyledSpan::new(&text[a..b], fg, bg)),
        }
    }
    spans
}

#[cfg(test)]
// One-element slices of ranges here are intentional; clippy mistakes them for
// an attempt to build a collection *from* a range.
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;

    const RED: Color = Color::Rgb(255, 0, 0);
    const GRN: Color = Color::Rgb(0, 255, 0);
    const BLU: Color = Color::Rgb(0, 0, 255);
    const BASE_BG: Option<Color> = Some(Color::Rgb(10, 10, 10));
    const EMPH_BG: Option<Color> = Some(Color::Rgb(90, 90, 90));

    /// `let x = 1;` split as `let` / ` x = ` / `1;`
    fn sample() -> (&'static str, Vec<(Range<usize>, Color)>) {
        ("let x = 1;", vec![(0..3, RED), (3..8, GRN), (8..10, BLU)])
    }

    fn texts(spans: &[StyledSpan]) -> Vec<&str> {
        spans.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn no_emphasis_yields_highlight_segments() {
        let (text, hl) = sample();
        let spans = overlay_spans(text, &hl, &[], BASE_BG, EMPH_BG);
        assert_eq!(texts(&spans), ["let", " x = ", "1;"]);
        assert!(spans.iter().all(|s| s.bg == BASE_BG));
        assert_eq!(
            spans.iter().map(|s| s.fg).collect::<Vec<_>>(),
            [Some(RED), Some(GRN), Some(BLU)]
        );
    }

    #[test]
    fn emphasis_matching_a_segment_marks_only_that_segment() {
        let (text, hl) = sample();
        let spans = overlay_spans(text, &hl, &[8..10], BASE_BG, EMPH_BG);
        assert_eq!(texts(&spans), ["let", " x = ", "1;"]);
        assert_eq!(
            spans.iter().map(|s| s.bg).collect::<Vec<_>>(),
            [BASE_BG, BASE_BG, EMPH_BG]
        );
    }

    #[test]
    fn emphasis_inside_a_segment_splits_it_in_three() {
        let (text, hl) = sample();
        let spans = overlay_spans(text, &hl, &[4..5], BASE_BG, EMPH_BG);
        assert_eq!(texts(&spans), ["let", " ", "x", " = ", "1;"]);
        // the three pieces of the middle segment all keep its foreground
        assert!(spans[1..4].iter().all(|s| s.fg == Some(GRN)));
        assert_eq!(spans[2].bg, EMPH_BG);
        assert_eq!(spans[1].bg, BASE_BG);
        assert_eq!(spans[3].bg, BASE_BG);
    }

    #[test]
    fn emphasis_straddling_two_segments_keeps_each_foreground() {
        let (text, hl) = sample();
        // covers the tail of `let` and the head of ` x = `
        let spans = overlay_spans(text, &hl, &[2..4], BASE_BG, EMPH_BG);
        assert_eq!(texts(&spans), ["le", "t", " ", "x = ", "1;"]);
        assert_eq!(spans[1].fg, Some(RED));
        assert_eq!(spans[1].bg, EMPH_BG);
        assert_eq!(spans[2].fg, Some(GRN));
        assert_eq!(spans[2].bg, EMPH_BG);
        assert_eq!(spans[3].bg, BASE_BG);
    }

    #[test]
    fn adjacent_emphasis_ranges_merge() {
        let (text, hl) = sample();
        // difftastic's per-token output style: 3..5 then 5..8
        let spans = overlay_spans(text, &hl, &[3..5, 5..8], BASE_BG, EMPH_BG);
        assert_eq!(texts(&spans), ["let", " x = ", "1;"]);
        assert_eq!(spans[1].bg, EMPH_BG);
    }

    #[test]
    fn unsorted_overlapping_emphasis_is_normalized() {
        let text = "abcdefghijkl";
        let hl = vec![(0..12, RED)];
        let spans = overlay_spans(text, &hl, &[8..12, 2..5, 3..9], BASE_BG, EMPH_BG);
        assert_eq!(texts(&spans), ["ab", "cdefghijkl"]);
        assert_eq!(spans[0].bg, BASE_BG);
        assert_eq!(spans[1].bg, EMPH_BG);
    }

    #[test]
    fn emphasis_covering_whole_line() {
        let (text, hl) = sample();
        let spans = overlay_spans(text, &hl, &[0..10], BASE_BG, EMPH_BG);
        assert_eq!(texts(&spans), ["let", " x = ", "1;"]);
        assert!(spans.iter().all(|s| s.bg == EMPH_BG));
    }

    #[test]
    fn empty_line_produces_no_spans() {
        let spans = overlay_spans("", &[], &[0..5], BASE_BG, EMPH_BG);
        assert!(spans.is_empty());
    }

    #[test]
    fn cjk_byte_offsets_from_difftastic() {
        // The exact line and range difftastic reports for this source:
        //   `    let 인사 = "안녕하세요 世界";`  ->  lhs change 18..33
        let text = "    let 인사 = \"안녕하세요 世界\";";
        let hl = vec![(0..text.len(), RED)];
        let spans = overlay_spans(text, &hl, &[18..33], BASE_BG, EMPH_BG);
        let emphasized: Vec<&str> = spans
            .iter()
            .filter(|s| s.bg == EMPH_BG)
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(emphasized, ["안녕하세요"]);
        assert_eq!(spans.concat_text(), text);
    }

    #[test]
    fn out_of_range_emphasis_is_clamped() {
        let (text, hl) = sample();
        let spans = overlay_spans(text, &hl, &[8..999], BASE_BG, EMPH_BG);
        assert_eq!(spans.concat_text(), text);
        assert_eq!(spans.last().unwrap().bg, EMPH_BG);
    }

    #[test]
    fn emphasis_landing_mid_utf8_snaps_outward() {
        let text = "a世b";
        let hl = vec![(0..text.len(), RED)];
        // 2 is inside the 3-byte 世 (1..4); expect it to snap out to 1..4
        let spans = overlay_spans(text, &hl, &[2..3], BASE_BG, EMPH_BG);
        assert_eq!(spans.concat_text(), text);
        let emphasized: Vec<&str> = spans
            .iter()
            .filter(|s| s.bg == EMPH_BG)
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(emphasized, ["世"]);
    }

    #[test]
    fn same_style_neighbours_coalesce() {
        let text = "abcdef";
        let hl = vec![(0..3, RED), (3..6, RED)];
        let spans = overlay_spans(text, &hl, &[], BASE_BG, EMPH_BG);
        assert_eq!(texts(&spans), ["abcdef"]);
    }

    #[test]
    fn bytes_outside_any_highlight_get_no_foreground() {
        let text = "abcdef";
        let hl = vec![(2..4, RED)];
        let spans = overlay_spans(text, &hl, &[], None, EMPH_BG);
        assert_eq!(texts(&spans), ["ab", "cd", "ef"]);
        assert_eq!(
            spans.iter().map(|s| s.fg).collect::<Vec<_>>(),
            [None, Some(RED), None]
        );
    }

    #[test]
    fn coalesce_merges_difftastic_token_runs() {
        assert_eq!(coalesce(&[18..24, 24..25, 25..31]), vec![18..31]);
        assert_eq!(coalesce(&[5..5, 1..3]), vec![1..3]);
        assert_eq!(coalesce(&[10..12, 1..3]), vec![1..3, 10..12]);
    }

    trait ConcatText {
        fn concat_text(&self) -> String;
    }
    impl ConcatText for Vec<StyledSpan> {
        fn concat_text(&self) -> String {
            self.iter().map(|s| s.text.as_str()).collect()
        }
    }
}
