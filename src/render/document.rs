//! Assembles the merge into a flat list of renderable rows — the merged panel.
//!
//! Auto-merged regions become plain highlighted rows. A conflict region that is
//! still undecided expands into three candidate blocks — LEFT, BASE, RIGHT —
//! with difftastic supplying the intra-line emphasis inside them. Once decided,
//! a region collapses to just the chosen lines and reads as ordinary merged
//! content: the choice is the point, not the markers it came from.

use ratatui::style::Color;

use crate::external::difft::{Differ, SideRanges};
use crate::merge::diff3::MergedChunk;
use crate::merge::session::{MergeSession, Resolution};
use crate::render::highlight::{Highlighter, HlState};
use crate::render::span::{StyledSpan, overlay_spans};
use crate::render::theme::{DiffTheme, Side};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    /// A line mergiraf merged cleanly.
    Context,
    /// The header opening a conflict region, showing its state.
    ConflictOpen,
    /// A candidate line from one side of an undecided region.
    Side(Side),
    /// A line of a decided region's chosen content.
    Resolved(Side),
    /// The rule closing a conflict region.
    ConflictClose,
}

#[derive(Clone, Debug)]
pub struct DisplayRow {
    pub kind: RowKind,
    /// Which conflict region this row belongs to, if any.
    pub conflict: Option<usize>,
    /// For [`RowKind::Context`], the running line number in the merged file.
    /// For a conflict side, the 1-based line number within that side's block.
    /// `None` for separators.
    pub line_no: Option<usize>,
    /// Background for the full width of the row, so short and blank lines
    /// inside a conflict block still read as part of it.
    pub row_bg: Option<Color>,
    pub spans: Vec<StyledSpan>,
    /// Text for a [`RowKind::ConflictOpen`] header row.
    pub header: Option<String>,
}

impl DisplayRow {
    /// Stash the header text on the row so the renderer needs no session.
    fn with_header(mut self, conflict: usize, total: usize, resolution: Resolution) -> Self {
        self.header = Some(conflict_header(conflict, total, resolution));
        self
    }
}

#[derive(Clone, Debug, Default)]
pub struct Document {
    pub rows: Vec<DisplayRow>,
    /// Row index of each [`RowKind::ConflictOpen`], for jump navigation.
    pub conflicts: Vec<usize>,
}

/// Header text for a conflict region: its position and what was chosen.
pub fn conflict_header(index: usize, total: usize, resolution: Resolution) -> String {
    format!(
        "── CONFLICT {}/{} · {} ",
        index + 1,
        total,
        resolution.label()
    )
}

impl Document {
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// Build the renderable document.
///
/// Context line numbers advance across a conflict by the length of its *left*
/// side, i.e. numbering continues as though left had been taken. The three
/// sides genuinely disagree about how many lines exist there, so any choice is
/// a convention; this one keeps post-conflict numbers matching the left file.
pub fn build_document(
    session: &MergeSession,
    highlighter: &Highlighter<'_>,
    differ: &dyn Differ,
    theme: &DiffTheme,
) -> Document {
    let mut doc = Document::default();
    let mut state = highlighter.new_state();
    let mut line_no = 1usize;
    let mut conflict = 0usize;
    let total = session.conflict_count();

    for chunk in session.chunks() {
        match chunk {
            MergedChunk::Resolved { lines } => {
                for line in lines {
                    let highlights = highlighter.line(&mut state, line);
                    doc.rows.push(DisplayRow {
                        kind: RowKind::Context,
                        conflict: None,
                        line_no: Some(line_no),
                        row_bg: None,
                        spans: overlay_spans(line, &highlights, &[], None, None),
                        header: None,
                    });
                    line_no += 1;
                }
            }
            MergedChunk::Conflict { left, base, right } => {
                let resolution = session.resolution(conflict);
                doc.conflicts.push(doc.rows.len());
                doc.rows.push(header_row(conflict, total, resolution));

                let advance = match session.resolved_lines(conflict) {
                    // Decided: show only what was chosen, as ordinary content.
                    Some(lines) => {
                        let side = chosen_side(resolution);
                        push_resolved(
                            &mut doc,
                            highlighter,
                            &mut state,
                            theme,
                            conflict,
                            side,
                            &lines,
                            &mut line_no,
                        );
                        // push_resolved already advanced the running number.
                        0
                    }
                    // Undecided: show all three candidates.
                    None => {
                        push_candidates(
                            &mut doc,
                            highlighter,
                            &mut state,
                            differ,
                            theme,
                            conflict,
                            left,
                            base,
                            right,
                        );
                        left.len()
                    }
                };

                doc.rows.push(close_row(conflict));
                line_no += advance;
                conflict += 1;
            }
        }
    }

    doc
}

/// Which side's colour a decided region borrows. "Both" reads as left.
fn chosen_side(resolution: Resolution) -> Side {
    match resolution {
        Resolution::Right => Side::Right,
        Resolution::Base => Side::Base,
        _ => Side::Left,
    }
}

/// The chosen content of a decided region, rendered as ordinary merged lines.
#[allow(clippy::too_many_arguments)]
fn push_resolved(
    doc: &mut Document,
    highlighter: &Highlighter<'_>,
    state: &mut HlState,
    theme: &DiffTheme,
    conflict: usize,
    side: Side,
    lines: &[String],
    line_no: &mut usize,
) {
    let bg = Some(theme.resolved_bg);
    for line in lines {
        let highlights = highlighter.line(state, line);
        doc.rows.push(DisplayRow {
            kind: RowKind::Resolved(side),
            conflict: Some(conflict),
            line_no: Some(*line_no),
            row_bg: bg,
            spans: overlay_spans(line, &highlights, &[], bg, bg),
            header: None,
        });
        *line_no += 1;
    }
}

/// The three candidate blocks of an undecided region.
#[allow(clippy::too_many_arguments)]
fn push_candidates(
    doc: &mut Document,
    highlighter: &Highlighter<'_>,
    state: &mut HlState,
    differ: &dyn Differ,
    theme: &DiffTheme,
    conflict: usize,
    left: &[String],
    base: &[String],
    right: &[String],
) {
    let base_text = base.join("\n");
    // Two structural diffs against the common ancestor.
    let vs_left = differ.diff(&base_text, &left.join("\n"));
    let vs_right = differ.diff(&base_text, &right.join("\n"));
    // The base block shows everything either side touched.
    let base_ranges = vs_left.lhs.union(&vs_right.lhs);
    let (left_ranges, right_ranges) = (vs_left.rhs, vs_right.rhs);

    // Each side continues from the same parser state; the main stream then
    // resumes from the left side's fork.
    let mut left_state = state.clone();
    push_side(
        doc,
        highlighter,
        &mut left_state,
        theme,
        conflict,
        Side::Left,
        left,
        &left_ranges,
    );

    let mut base_state = state.clone();
    push_side(
        doc,
        highlighter,
        &mut base_state,
        theme,
        conflict,
        Side::Base,
        base,
        &base_ranges,
    );

    let mut right_state = state.clone();
    push_side(
        doc,
        highlighter,
        &mut right_state,
        theme,
        conflict,
        Side::Right,
        right,
        &right_ranges,
    );

    *state = left_state;
}

#[allow(clippy::too_many_arguments)]
fn push_side(
    doc: &mut Document,
    highlighter: &Highlighter<'_>,
    state: &mut HlState,
    theme: &DiffTheme,
    conflict: usize,
    side: Side,
    lines: &[String],
    ranges: &SideRanges,
) {
    let (bg, emph_bg) = theme.side_colors(side);
    for (i, line) in lines.iter().enumerate() {
        let highlights = highlighter.line(state, line);
        // difftastic reports 0-based line numbers.
        let emphasis = ranges.get(i as u32);
        doc.rows.push(DisplayRow {
            kind: RowKind::Side(side),
            conflict: Some(conflict),
            line_no: Some(i + 1),
            row_bg: bg,
            spans: overlay_spans(line, &highlights, emphasis, bg, emph_bg),
            header: None,
        });
    }
}

fn header_row(conflict: usize, total: usize, resolution: Resolution) -> DisplayRow {
    DisplayRow {
        kind: RowKind::ConflictOpen,
        conflict: Some(conflict),
        line_no: None,
        row_bg: None,
        spans: Vec::new(),
        header: None,
    }
    .with_header(conflict, total, resolution)
}

fn close_row(conflict: usize) -> DisplayRow {
    DisplayRow {
        kind: RowKind::ConflictClose,
        conflict: Some(conflict),
        line_no: None,
        row_bg: None,
        spans: Vec::new(),
        header: None,
    }
}

#[cfg(test)]
// One-element slices/vecs of ranges here are intentional; clippy mistakes them
// for an attempt to build a collection *from* a range.
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::external::difft::StaticDiffer;
    use crate::merge::session::Resolution;
    use crate::render::highlight::{Assets, DEFAULT_THEME};

    fn session(chunks: Vec<MergedChunk>) -> crate::merge::session::MergeSession {
        crate::merge::session::MergeSession::new(chunks, Default::default())
    }

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn sample_chunks() -> Vec<MergedChunk> {
        vec![
            MergedChunk::Resolved {
                lines: lines(&["fn main() {"]),
            },
            MergedChunk::Conflict {
                left: lines(&["    let a = 100;"]),
                base: lines(&["    let a = 1;"]),
                right: lines(&["    let a = 999;", "    let extra = 0;"]),
            },
            MergedChunk::Resolved {
                lines: lines(&["    let b = 2;", "}"]),
            },
        ]
    }

    fn build(differ: &dyn Differ) -> (Document, DiffTheme) {
        let assets = Assets::new();
        let hl = assets
            .highlighter(Some("rust"), None, DEFAULT_THEME)
            .unwrap();
        let theme = DiffTheme::default();
        let doc = build_document(&session(sample_chunks()), &hl, differ, &theme);
        (doc, theme)
    }

    #[test]
    fn rows_are_laid_out_context_then_conflict_blocks() {
        let (doc, _) = build(&StaticDiffer::default());
        let kinds: Vec<RowKind> = doc.rows.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            vec![
                RowKind::Context,
                RowKind::ConflictOpen,
                RowKind::Side(Side::Left),
                RowKind::Side(Side::Base),
                RowKind::Side(Side::Right),
                RowKind::Side(Side::Right),
                RowKind::ConflictClose,
                RowKind::Context,
                RowKind::Context,
            ]
        );
    }

    #[test]
    fn conflict_row_indexes_are_recorded_for_navigation() {
        let (doc, _) = build(&StaticDiffer::default());
        assert_eq!(doc.conflicts, vec![1]);
        assert_eq!(doc.rows[doc.conflicts[0]].kind, RowKind::ConflictOpen);
    }

    #[test]
    fn context_numbering_continues_past_a_conflict_using_the_left_side() {
        let (doc, _) = build(&StaticDiffer::default());
        let context_nums: Vec<Option<usize>> = doc
            .rows
            .iter()
            .filter(|r| r.kind == RowKind::Context)
            .map(|r| r.line_no)
            .collect();
        // one left line in the conflict, so numbering resumes at 3
        assert_eq!(context_nums, vec![Some(1), Some(3), Some(4)]);
    }

    #[test]
    fn conflict_sides_are_numbered_within_their_own_block() {
        let (doc, _) = build(&StaticDiffer::default());
        let right: Vec<Option<usize>> = doc
            .rows
            .iter()
            .filter(|r| r.kind == RowKind::Side(Side::Right))
            .map(|r| r.line_no)
            .collect();
        assert_eq!(right, vec![Some(1), Some(2)]);
    }

    #[test]
    fn each_side_carries_its_own_background() {
        let (doc, theme) = build(&StaticDiffer::default());
        let bg_of = |side| {
            doc.rows
                .iter()
                .find(|r| r.kind == RowKind::Side(side))
                .unwrap()
                .row_bg
        };
        assert_eq!(bg_of(Side::Left), Some(theme.left_bg));
        assert_eq!(bg_of(Side::Base), Some(theme.base_bg));
        assert_eq!(bg_of(Side::Right), Some(theme.right_bg));
        // context rows stay on the terminal's own background
        assert!(
            doc.rows
                .iter()
                .filter(|r| r.kind == RowKind::Context)
                .all(|r| r.row_bg.is_none())
        );
    }

    #[test]
    fn difftastic_ranges_become_emphasis_backgrounds() {
        // `100` is at bytes 12..15 of "    let a = 100;"
        let differ = StaticDiffer {
            aligned: Vec::new(),
            lhs: SideRanges(HashMap::from([(0, vec![12..13])])),
            rhs: SideRanges(HashMap::from([(0, vec![12..15])])),
        };
        let (doc, theme) = build(&differ);

        let left = doc
            .rows
            .iter()
            .find(|r| r.kind == RowKind::Side(Side::Left))
            .unwrap();
        let emphasized: String = left
            .spans
            .iter()
            .filter(|s| s.bg == Some(theme.left_emph_bg))
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(emphasized, "100");

        // the base block takes the union of both diffs' base-side ranges
        let base = doc
            .rows
            .iter()
            .find(|r| r.kind == RowKind::Side(Side::Base))
            .unwrap();
        let emphasized: String = base
            .spans
            .iter()
            .filter(|s| s.bg == Some(theme.base_emph_bg))
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(emphasized, "1");
    }

    #[test]
    fn spans_reconstruct_the_original_line_text() {
        let differ = StaticDiffer {
            aligned: Vec::new(),
            lhs: SideRanges(HashMap::from([(0, vec![12..13])])),
            rhs: SideRanges(HashMap::from([(0, vec![12..15])])),
        };
        let (doc, _) = build(&differ);
        let text_of =
            |row: &DisplayRow| -> String { row.spans.iter().map(|s| s.text.as_str()).collect() };
        assert_eq!(text_of(&doc.rows[0]), "fn main() {");
        assert_eq!(text_of(&doc.rows[2]), "    let a = 100;");
        assert_eq!(text_of(&doc.rows[3]), "    let a = 1;");
        assert_eq!(text_of(&doc.rows[4]), "    let a = 999;");
    }

    /// The `tests/fixtures/multi` shape: both sides replace the same region
    /// with a different number of lines.
    #[test]
    fn a_conflict_whose_sides_have_different_lengths() {
        let assets = Assets::new();
        let hl = assets
            .highlighter(Some("rust"), None, DEFAULT_THEME)
            .unwrap();
        let theme = DiffTheme::default();
        let chunks = vec![
            MergedChunk::Resolved {
                lines: lines(&["fn compute(x: i32) -> i32 {"]),
            },
            MergedChunk::Conflict {
                left: lines(&[
                    "    let y = x + 10;",
                    "    let z = y * 2;",
                    "    let w = z - 1;",
                    "    w",
                ]),
                base: lines(&["    let y = x + 1;", "    let z = y * 2;", "    z"]),
                right: lines(&["    x * 3"]),
            },
            MergedChunk::Resolved {
                lines: lines(&["}"]),
            },
        ];
        let doc = build_document(&session(chunks), &hl, &StaticDiffer::default(), &theme);

        let count = |kind| doc.rows.iter().filter(|r| r.kind == kind).count();
        assert_eq!(count(RowKind::Side(Side::Left)), 4);
        assert_eq!(count(RowKind::Side(Side::Base)), 3);
        assert_eq!(count(RowKind::Side(Side::Right)), 1);

        // each side is numbered 1..n within its own block
        let nums = |side| {
            doc.rows
                .iter()
                .filter(|r| r.kind == RowKind::Side(side))
                .map(|r| r.line_no)
                .collect::<Vec<_>>()
        };
        assert_eq!(nums(Side::Left), vec![Some(1), Some(2), Some(3), Some(4)]);
        assert_eq!(nums(Side::Base), vec![Some(1), Some(2), Some(3)]);
        assert_eq!(nums(Side::Right), vec![Some(1)]);

        // context numbering resumes past the conflict by the left side's 4 lines
        let context: Vec<Option<usize>> = doc
            .rows
            .iter()
            .filter(|r| r.kind == RowKind::Context)
            .map(|r| r.line_no)
            .collect();
        assert_eq!(context, vec![Some(1), Some(6)]);

        // the blocks stay in order and are bracketed by the separators
        let kinds: Vec<RowKind> = doc.rows.iter().map(|r| r.kind).collect();
        assert_eq!(kinds[1], RowKind::ConflictOpen);
        assert_eq!(kinds[kinds.len() - 2], RowKind::ConflictClose);
    }

    #[test]
    fn a_conflict_with_an_empty_side_still_renders() {
        let assets = Assets::new();
        let hl = assets
            .highlighter(Some("rust"), None, DEFAULT_THEME)
            .unwrap();
        let chunks = vec![MergedChunk::Conflict {
            left: lines(&["added();"]),
            base: vec![],
            right: vec![],
        }];
        let doc = build_document(
            &session(chunks),
            &hl,
            &StaticDiffer::default(),
            &DiffTheme::default(),
        );
        assert_eq!(
            doc.rows.iter().map(|r| r.kind).collect::<Vec<_>>(),
            vec![
                RowKind::ConflictOpen,
                RowKind::Side(Side::Left),
                RowKind::ConflictClose
            ]
        );
    }

    #[test]
    fn an_undecided_region_still_offers_all_three_candidates() {
        let assets = Assets::new();
        let hl = assets
            .highlighter(Some("rust"), None, DEFAULT_THEME)
            .unwrap();
        let doc = build_document(
            &session(sample_chunks()),
            &hl,
            &StaticDiffer::default(),
            &DiffTheme::default(),
        );
        let count = |k| doc.rows.iter().filter(|r| r.kind == k).count();
        assert_eq!(count(RowKind::Side(Side::Left)), 1);
        assert_eq!(count(RowKind::Side(Side::Base)), 1);
        assert_eq!(count(RowKind::Side(Side::Right)), 2);
        assert_eq!(count(RowKind::Resolved(Side::Left)), 0);
        assert_eq!(
            doc.rows[doc.conflicts[0]].header.as_deref(),
            Some("── CONFLICT 1/1 · unresolved ")
        );
    }

    #[test]
    fn deciding_a_region_collapses_it_to_the_chosen_lines() {
        let assets = Assets::new();
        let hl = assets
            .highlighter(Some("rust"), None, DEFAULT_THEME)
            .unwrap();
        let theme = DiffTheme::default();
        let mut s = session(sample_chunks());
        s.set_resolution(0, Resolution::Left);
        let doc = build_document(&s, &hl, &StaticDiffer::default(), &theme);

        let count = |k| doc.rows.iter().filter(|r| r.kind == k).count();
        assert_eq!(count(RowKind::Resolved(Side::Left)), 1);
        assert_eq!(count(RowKind::Side(Side::Left)), 0, "no candidates remain");
        assert_eq!(count(RowKind::Side(Side::Base)), 0);
        assert_eq!(count(RowKind::Side(Side::Right)), 0);

        let row = doc
            .rows
            .iter()
            .find(|r| r.kind == RowKind::Resolved(Side::Left))
            .unwrap();
        assert_eq!(row.row_bg, Some(theme.resolved_bg));
        assert_eq!(
            row.spans
                .iter()
                .map(|s| s.text.as_str())
                .collect::<String>(),
            "    let a = 100;"
        );
        assert_eq!(
            doc.rows[doc.conflicts[0]].header.as_deref(),
            Some("── CONFLICT 1/1 · left ")
        );
    }

    #[test]
    fn taking_right_shows_the_right_lines_and_its_own_length() {
        let assets = Assets::new();
        let hl = assets
            .highlighter(Some("rust"), None, DEFAULT_THEME)
            .unwrap();
        let mut s = session(sample_chunks());
        s.set_resolution(0, Resolution::Right);
        let doc = build_document(&s, &hl, &StaticDiffer::default(), &DiffTheme::default());

        // the right side of `sample_chunks` has two lines
        assert_eq!(
            doc.rows
                .iter()
                .filter(|r| r.kind == RowKind::Resolved(Side::Right))
                .count(),
            2
        );
        // and numbering afterwards advances by that, not by the left side's one
        let context: Vec<Option<usize>> = doc
            .rows
            .iter()
            .filter(|r| r.kind == RowKind::Context)
            .map(|r| r.line_no)
            .collect();
        assert_eq!(context, vec![Some(1), Some(4), Some(5)]);
    }

    #[test]
    fn taking_both_emits_both_sides_in_order() {
        let assets = Assets::new();
        let hl = assets
            .highlighter(Some("rust"), None, DEFAULT_THEME)
            .unwrap();
        let mut s = session(sample_chunks());
        s.set_resolution(0, Resolution::BothLeftFirst);
        let doc = build_document(&s, &hl, &StaticDiffer::default(), &DiffTheme::default());

        let text: Vec<String> = doc
            .rows
            .iter()
            .filter(|r| matches!(r.kind, RowKind::Resolved(_)))
            .map(|r| r.spans.iter().map(|s| s.text.as_str()).collect())
            .collect();
        assert_eq!(text[0], "    let a = 100;");
        assert_eq!(text[1], "    let a = 999;");
    }

    #[test]
    fn every_conflict_row_carries_its_region_index() {
        let assets = Assets::new();
        let hl = assets
            .highlighter(Some("rust"), None, DEFAULT_THEME)
            .unwrap();
        let doc = build_document(
            &session(sample_chunks()),
            &hl,
            &StaticDiffer::default(),
            &DiffTheme::default(),
        );
        for row in &doc.rows {
            match row.kind {
                RowKind::Context => assert_eq!(row.conflict, None),
                _ => assert_eq!(row.conflict, Some(0), "{:?}", row.kind),
            }
        }
    }

    /// Three conflicts of different shapes with auto-merged content between —
    /// the `tests/fixtures/chunks` shape.
    fn three_region_chunks() -> Vec<MergedChunk> {
        vec![
            MergedChunk::Resolved {
                lines: lines(&["fn one() -> i32 {"]),
            },
            MergedChunk::Conflict {
                left: lines(&["    10"]),
                base: lines(&["    1"]),
                right: lines(&["    11"]),
            },
            MergedChunk::Resolved {
                lines: lines(&["}", "fn three() -> i32 {"]),
            },
            MergedChunk::Conflict {
                left: lines(&["    let a = 30;", "    let b = 0;", "    a + b"]),
                base: lines(&["    3"]),
                right: lines(&["    33"]),
            },
            MergedChunk::Resolved {
                lines: lines(&["}", "fn four() -> i32 {"]),
            },
            MergedChunk::Conflict {
                left: lines(&["    40"]),
                base: lines(&["    4"]),
                right: lines(&["    44", "    // note"]),
            },
            MergedChunk::Resolved {
                lines: lines(&["}"]),
            },
        ]
    }

    #[test]
    fn several_regions_are_numbered_and_indexed_in_order() {
        let assets = Assets::new();
        let hl = assets
            .highlighter(Some("rust"), None, DEFAULT_THEME)
            .unwrap();
        let doc = build_document(
            &session(three_region_chunks()),
            &hl,
            &StaticDiffer::default(),
            &DiffTheme::default(),
        );

        assert_eq!(doc.conflicts.len(), 3);
        let headers: Vec<&str> = doc
            .conflicts
            .iter()
            .map(|&r| doc.rows[r].header.as_deref().unwrap())
            .collect();
        assert_eq!(
            headers,
            [
                "── CONFLICT 1/3 · unresolved ",
                "── CONFLICT 2/3 · unresolved ",
                "── CONFLICT 3/3 · unresolved ",
            ]
        );
        // and the row indexes come out in order
        for pair in doc.conflicts.windows(2) {
            assert!(pair[0] < pair[1]);
        }
    }

    #[test]
    fn every_row_belongs_to_the_region_that_produced_it() {
        let assets = Assets::new();
        let hl = assets
            .highlighter(Some("rust"), None, DEFAULT_THEME)
            .unwrap();
        let doc = build_document(
            &session(three_region_chunks()),
            &hl,
            &StaticDiffer::default(),
            &DiffTheme::default(),
        );

        // each region's candidate rows carry its own index, and the second
        // region's left side really is three lines long
        let sides = |c: usize, side: Side| {
            doc.rows
                .iter()
                .filter(|r| r.conflict == Some(c) && r.kind == RowKind::Side(side))
                .count()
        };
        assert_eq!((sides(0, Side::Left), sides(0, Side::Right)), (1, 1));
        assert_eq!((sides(1, Side::Left), sides(1, Side::Right)), (3, 1));
        assert_eq!((sides(2, Side::Left), sides(2, Side::Right)), (1, 2));
        assert!(doc.rows.iter().all(|r| r.conflict.is_none_or(|c| c < 3)));
    }

    #[test]
    fn resolving_one_region_leaves_the_others_expanded() {
        let assets = Assets::new();
        let hl = assets
            .highlighter(Some("rust"), None, DEFAULT_THEME)
            .unwrap();
        let mut s = session(three_region_chunks());
        s.set_resolution(1, Resolution::Right);
        let doc = build_document(&s, &hl, &StaticDiffer::default(), &DiffTheme::default());

        let expanded = |c: usize| {
            doc.rows
                .iter()
                .any(|r| r.conflict == Some(c) && matches!(r.kind, RowKind::Side(_)))
        };
        assert!(expanded(0), "region 0 is still a three-way choice");
        assert!(!expanded(1), "region 1 collapsed to its choice");
        assert!(expanded(2), "region 2 is still a three-way choice");

        assert_eq!(
            doc.rows[doc.conflicts[1]].header.as_deref(),
            Some("── CONFLICT 2/3 · right ")
        );
        assert_eq!(
            doc.rows
                .iter()
                .filter(|r| r.kind == RowKind::Resolved(Side::Right))
                .count(),
            1
        );
    }

    #[test]
    fn context_numbering_runs_through_every_region() {
        let assets = Assets::new();
        let hl = assets
            .highlighter(Some("rust"), None, DEFAULT_THEME)
            .unwrap();
        let mut s = session(three_region_chunks());
        // left: 1 line, right: 1 line, base: 1 line -> each region contributes 1
        for i in 0..3 {
            s.set_resolution(i, Resolution::Left);
        }
        let doc = build_document(&s, &hl, &StaticDiffer::default(), &DiffTheme::default());

        let numbers: Vec<usize> = doc.rows.iter().filter_map(|r| r.line_no).collect();
        // region 1 takes three left lines, so the file is 1+1+2+3+2+1+1 = 11 lines
        assert_eq!(numbers, (1..=11).collect::<Vec<_>>());
    }

    #[test]
    fn no_chunks_yields_an_empty_document() {
        let assets = Assets::new();
        let hl = assets
            .highlighter(Some("rust"), None, DEFAULT_THEME)
            .unwrap();
        let doc = build_document(
            &session(Vec::new()),
            &hl,
            &StaticDiffer::default(),
            &DiffTheme::default(),
        );
        assert!(doc.is_empty() && doc.conflicts.is_empty());
    }
}
