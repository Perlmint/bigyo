//! Assembles the three revisions into row-aligned columns for the
//! side-by-side view.
//!
//! Where the merged view answers "what does the merge look like, and where did
//! it fail?", this one answers "what did each side actually do to the common
//! ancestor?" — so it is driven entirely by the two structural diffs against
//! base, with no involvement from mergiraf.

use std::ops::Range;

use ratatui::style::Color;

use crate::external::difft::{Differ, SideRanges, line_count};
use crate::merge::locate::conflict_base_ranges;
use crate::merge::session::MergeSession;
use crate::render::align::{AlignedRow, align3};
use crate::render::highlight::Highlighter;
use crate::render::span::{StyledSpan, overlay_spans};
use crate::render::text::{common_dir_prefix, strip_dir_prefix};
use crate::render::theme::{DiffTheme, Side};

/// The file behind each section of the screen, for the title bars.
#[derive(Clone, Debug, Default)]
pub struct SectionNames {
    pub left: String,
    pub base: String,
    pub right: String,
    /// Where the resolved file will be written.
    pub merged: String,
}

impl SectionNames {
    /// Name the four sections, dropping the leading directories the three
    /// revisions share.
    ///
    /// In a merge those directories are usually identical and carry no
    /// information — what distinguishes the columns is the file name. The
    /// output path is shortened too, but only if it sits under the same
    /// directories; otherwise it keeps its full path, since it is somewhere
    /// else entirely and saying so is the useful thing.
    pub fn new(left: &str, base: &str, right: &str, merged: &str) -> Self {
        let shared = common_dir_prefix(&[left, base, right]);
        let prefix: String = left
            .split('/')
            .take(shared)
            .map(|part| format!("{part}/"))
            .collect();
        let short = |p: &str| {
            if p.starts_with(&prefix) {
                strip_dir_prefix(p, shared)
            } else {
                p
            }
            .to_owned()
        };
        Self {
            left: short(left),
            base: short(base),
            right: short(right),
            merged: short(merged),
        }
    }

    pub fn side(&self, side: Side) -> &str {
        match side {
            Side::Left => &self.left,
            Side::Base => &self.base,
            Side::Right => &self.right,
        }
    }
}

/// One revision's contribution to a row.
#[derive(Clone, Debug, Default)]
pub struct Cell {
    /// 1-based line number in that file; `None` where the file has no line here.
    pub line_no: Option<usize>,
    /// Full-width background: the side tint when changed, the gap tint when the
    /// file has no line, and `None` when the line is unchanged.
    pub bg: Option<Color>,
    pub spans: Vec<StyledSpan>,
}

impl Cell {
    pub fn is_gap(&self) -> bool {
        self.line_no.is_none()
    }

    pub fn text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }

    /// Repaint the whole cell.
    ///
    /// `fg: Some(_)` flattens every span to one colour, discarding the syntax
    /// highlighting — that is what makes a rejected side read as *dim* rather
    /// than merely differently tinted. Nothing is lost by it: both documents are
    /// rebuilt from scratch whenever a resolution changes.
    fn paint(&mut self, bg: Option<Color>, fg: Option<Color>) {
        self.bg = bg;
        for span in &mut self.spans {
            span.bg = bg;
            if fg.is_some() {
                span.fg = fg;
            }
        }
    }
}

/// One row of the side-by-side view, indexed by [`Side::index`].
#[derive(Clone, Debug, Default)]
pub struct PaneRow {
    pub cells: [Cell; 3],
    /// True when any side differs here, so this row is part of a hunk.
    pub changed: bool,
    /// Which conflict region this row belongs to, if any.
    pub conflict: Option<usize>,
}

impl PaneRow {
    pub fn cell(&self, side: Side) -> &Cell {
        &self.cells[side.index()]
    }
}

#[derive(Clone, Debug, Default)]
pub struct PaneDocument {
    pub rows: Vec<PaneRow>,
    /// First row of each run of changed rows.
    pub hunks: Vec<usize>,
    /// Row range of each conflict region, in file order, so `n`/`p` can put the
    /// same region on screen here and in the merged panel.
    pub conflicts: Vec<Range<usize>>,
}

impl PaneDocument {
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// Build the row-aligned three-column document.
///
/// Runs the differ twice against the common ancestor, joins the two alignments
/// with [`align3`], and highlights each file once.
pub fn build_panes(
    session: &MergeSession,
    base: &str,
    left: &str,
    right: &str,
    highlighter: &Highlighter<'_>,
    differ: &dyn Differ,
    theme: &DiffTheme,
) -> PaneDocument {
    let vs_left = differ.diff(base, left);
    let vs_right = differ.diff(base, right);

    // The base column shows whatever either side touched.
    let base_ranges = vs_left.lhs.clone().union(&vs_right.lhs);
    let ranges: [&SideRanges; 3] = [&vs_left.rhs, &base_ranges, &vs_right.rhs];

    // Split on '\n' rather than lines() so the indices match the ones
    // difftastic reports.
    let texts: [Vec<&str>; 3] = [
        left.split('\n').collect(),
        base.split('\n').collect(),
        right.split('\n').collect(),
    ];
    let highlights: [Vec<Vec<_>>; 3] = [
        highlighter.file(&texts[0]),
        highlighter.file(&texts[1]),
        highlighter.file(&texts[2]),
    ];

    let mut aligned = align3(&vs_left.aligned, &vs_right.aligned);
    drop_trailing_empty_row(&mut aligned, base, left, right);

    // Where each conflict region falls, mapped from base lines onto rows.
    let base_ranges = conflict_base_ranges(session.chunks(), &texts[1]);
    let owners = region_owners(&aligned, &base_ranges);

    let mut doc = PaneDocument::default();
    for (index, row) in aligned.iter().enumerate() {
        let numbers = [row.left, row.base, row.right];
        let mut cells: [Cell; 3] = Default::default();
        let mut changed = false;

        for side in Side::ALL {
            let s = side.index();
            let Some(n) = numbers[s].map(|n| n as usize) else {
                // The file has no line here at all.
                cells[s] = Cell {
                    line_no: None,
                    bg: Some(theme.gap_bg),
                    spans: Vec::new(),
                };
                changed = true;
                continue;
            };
            let text = texts[s].get(n).copied().unwrap_or_default();
            let emphasis = ranges[s].get(n as u32);
            // A line counts as changed when the diff flagged bytes in it, or
            // when its counterpart on the base side is missing entirely.
            let is_changed = !emphasis.is_empty() || row.has_gap();
            let (bg, emph_bg) = if is_changed {
                theme.side_colors(side)
            } else {
                (None, None)
            };
            changed |= is_changed;

            cells[s] = Cell {
                line_no: Some(n + 1),
                bg,
                spans: overlay_spans(
                    text,
                    highlights[s].get(n).map_or(&[][..], Vec::as_slice),
                    emphasis,
                    bg,
                    emph_bg,
                ),
            };
        }

        let conflict = owners[index];
        if let Some(c) = conflict {
            // The region is the unit of work, so it outranks the per-line tint.
            let resolution = session.resolution(c);
            for side in Side::ALL {
                let cell = &mut cells[side.index()];
                if cell.is_gap() {
                    continue;
                }
                match resolution.takes(side) {
                    // Still a three-way choice: every side reads the same.
                    None => cell.paint(Some(theme.conflict_bg), None),
                    // Chosen: keeps its syntax colours, lit on the resolved ground.
                    Some(true) => cell.paint(Some(theme.resolved_bg), None),
                    // Rejected: flattened to grey so it recedes at a glance.
                    Some(false) => cell.paint(Some(theme.dimmed_bg), Some(theme.dimmed_fg)),
                }
            }
            changed = true;
        }

        doc.rows.push(PaneRow {
            cells,
            changed,
            conflict,
        });
    }

    doc.hunks = hunk_starts(&doc.rows);
    doc.conflicts = region_ranges(&doc.rows, base_ranges.len());
    doc
}

/// Which conflict region owns each aligned row, if any.
///
/// Rows are given an integer position — a real base line `b` sits at `2b`, and a
/// gap row sits at `2b + 1` where `b` is the base line it *follows* (`-1` for a
/// gap before any base line). A region covering base lines `[s, e)` then claims
/// positions `[2s, 2e)`: insertions trailing the region's last base line fall
/// inside it, while the gap immediately before it (position `2s - 1`) correctly
/// belongs to whatever precedes. A region with an empty base run at `p` claims
/// `[2p - 1, 2p)` — exactly the gap rows sitting at that point.
fn region_owners(rows: &[AlignedRow], base_ranges: &[Range<usize>]) -> Vec<Option<usize>> {
    let mut positions: Vec<isize> = Vec::with_capacity(rows.len());
    let mut last_base: Option<isize> = None;
    for row in rows {
        match row.base {
            Some(b) => {
                last_base = Some(b as isize);
                positions.push(2 * b as isize);
            }
            None => positions.push(2 * last_base.unwrap_or(-1) + 1),
        }
    }

    positions
        .iter()
        .map(|&pos| {
            base_ranges.iter().position(|r| {
                let (start, end) = (r.start as isize, r.end as isize);
                let (lo, hi) = if r.is_empty() {
                    (2 * start - 1, 2 * start)
                } else {
                    (2 * start, 2 * end)
                };
                (lo..hi).contains(&pos)
            })
        })
        .collect()
}

/// The row range each region covers, empty where a region matched no rows.
fn region_ranges(rows: &[PaneRow], count: usize) -> Vec<Range<usize>> {
    (0..count)
        .map(|c| {
            let first = rows.iter().position(|r| r.conflict == Some(c));
            match first {
                Some(start) => {
                    let end = rows[start..]
                        .iter()
                        .position(|r| r.conflict != Some(c))
                        .map_or(rows.len(), |n| start + n);
                    start..end
                }
                None => 0..0,
            }
        })
        .collect()
}

/// Whether `split('\n')` leaves a phantom empty element at the end of `text`.
///
/// True for a file ending in a newline (its content stops on the line before),
/// and for an empty file, which has no lines at all.
fn has_phantom_last_line(text: &str) -> bool {
    text.is_empty() || text.ends_with('\n')
}

/// Drop the phantom final row that `split('\n')` leaves behind — every file
/// would otherwise gain a trailing blank row.
fn drop_trailing_empty_row(rows: &mut Vec<AlignedRow>, base: &str, left: &str, right: &str) {
    let is_phantom = |n: Option<u32>, text: &str| match n {
        // A gap is vacuously phantom: nothing of that file is shown here.
        None => true,
        Some(n) => has_phantom_last_line(text) && n as usize == line_count(text) - 1,
    };

    if let Some(row) = rows.last()
        && is_phantom(row.left, left)
        && is_phantom(row.base, base)
        && is_phantom(row.right, right)
    {
        rows.pop();
    }
}

/// The index of the first row of each run of changed rows.
fn hunk_starts(rows: &[PaneRow]) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter(|(i, row)| row.changed && (*i == 0 || !rows[i - 1].changed))
        .map(|(i, _)| i)
        .collect()
}

#[cfg(test)]
// One-element slices/vecs of ranges here are intentional; clippy mistakes them
// for an attempt to build a collection *from* a range.
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::external::difft::StaticDiffer;
    use crate::render::highlight::{Assets, DEFAULT_THEME};

    const BASE: &str =
        "fn compute(x: i32) -> i32 {\n    let y = x + 1;\n    let z = y * 2;\n    z\n}\n";
    const LEFT: &str = "fn compute(x: i32) -> i32 {\n    let y = x + 10;\n    let z = y * 2;\n    let w = z - 1;\n    w\n}\n";
    const RIGHT: &str = "fn compute(x: i32) -> i32 {\n    x * 3\n}\n";

    #[test]
    fn section_names_drop_the_directories_the_revisions_share() {
        let names = SectionNames::new(
            "tests/fixtures/chunks/left.rs",
            "tests/fixtures/chunks/base.rs",
            "tests/fixtures/chunks/right.rs",
            "tests/fixtures/chunks/merged.rs",
        );
        assert_eq!(names.left, "left.rs");
        assert_eq!(names.base, "base.rs");
        assert_eq!(names.right, "right.rs");
        assert_eq!(names.merged, "merged.rs");
    }

    #[test]
    fn section_names_keep_whatever_actually_differs() {
        let names = SectionNames::new("work/a/x.rs", "work/b/x.rs", "work/c/x.rs", "work/out.rs");
        assert_eq!(names.left, "a/x.rs");
        assert_eq!(names.base, "b/x.rs");
        assert_eq!(names.right, "c/x.rs");
        assert_eq!(names.merged, "out.rs");
    }

    #[test]
    fn an_output_somewhere_else_keeps_its_full_path() {
        // the output is not under the shared directories, so saying where it is
        // is the useful thing
        let names = SectionNames::new(
            "tests/fixtures/chunks/left.rs",
            "tests/fixtures/chunks/base.rs",
            "tests/fixtures/chunks/right.rs",
            "/tmp/merged.rs",
        );
        assert_eq!(names.left, "left.rs");
        assert_eq!(names.merged, "/tmp/merged.rs");
    }

    #[test]
    fn section_names_with_nothing_in_common_are_untouched() {
        let names = SectionNames::new("a/x.rs", "b/y.rs", "c/z.rs", "d/out.rs");
        assert_eq!(names.left, "a/x.rs");
        assert_eq!(names.base, "b/y.rs");
        assert_eq!(names.right, "c/z.rs");
        assert_eq!(names.merged, "d/out.rs");
    }

    #[test]
    fn bare_file_names_survive_intact() {
        let names = SectionNames::new("left.rs", "base.rs", "right.rs", "merged.rs");
        assert_eq!(names.left, "left.rs");
        assert_eq!(names.merged, "merged.rs");
    }

    #[test]
    fn identical_revision_paths_still_show_a_file_name() {
        // git hands the same path three times in some merge-driver setups
        let names = SectionNames::new("w/f.rs", "w/f.rs", "w/f.rs", "w/f.rs");
        assert_eq!(names.left, "f.rs");
        assert_eq!(names.side(Side::Base), "f.rs");
    }

    /// A session with nothing for the panes to treat as a conflict region.
    fn no_conflicts() -> MergeSession {
        MergeSession::new(Vec::new(), Default::default())
    }

    /// The alignments difftastic really reports for `tests/fixtures/multi`.
    fn multi_differ() -> StaticDiffer {
        StaticDiffer {
            aligned: Vec::new(),
            lhs: SideRanges::default(),
            rhs: SideRanges::default(),
        }
    }

    struct TwoWay {
        left: crate::external::difft::DiffResult,
        right: crate::external::difft::DiffResult,
    }

    impl Differ for TwoWay {
        fn diff(&self, _lhs: &str, rhs: &str) -> crate::external::difft::DiffResult {
            if rhs == LEFT {
                self.left.clone()
            } else {
                self.right.clone()
            }
        }
    }

    fn multi_two_way() -> TwoWay {
        use crate::external::difft::DiffResult;
        TwoWay {
            left: DiffResult {
                aligned: vec![
                    (Some(0), Some(0)),
                    (Some(1), Some(1)),
                    (Some(2), Some(2)),
                    (Some(3), Some(3)),
                    (None, Some(4)),
                    (Some(4), Some(5)),
                    (Some(5), Some(6)),
                ],
                // base `x + 1` -> left `x + 10`, the `1`/`10` at byte 16
                lhs: SideRanges(HashMap::from([(1, vec![16..17])])),
                rhs: SideRanges(HashMap::from([(1, vec![16..18])])),
            },
            right: DiffResult {
                aligned: vec![
                    (Some(0), Some(0)),
                    (Some(1), Some(1)),
                    (Some(2), None),
                    (Some(3), None),
                    (Some(4), Some(2)),
                    (Some(5), Some(3)),
                ],
                lhs: SideRanges(HashMap::from([(1, vec![4..18])])),
                rhs: SideRanges(HashMap::from([(1, vec![4..9])])),
            },
        }
    }

    fn build(differ: &dyn Differ) -> (PaneDocument, DiffTheme) {
        let assets = Assets::new();
        let hl = assets
            .highlighter(Some("rust"), None, DEFAULT_THEME)
            .unwrap();
        let theme = DiffTheme::default();
        let doc = build_panes(&no_conflicts(), BASE, LEFT, RIGHT, &hl, differ, &theme);
        (doc, theme)
    }

    #[test]
    fn identical_inputs_have_no_changed_rows() {
        let assets = Assets::new();
        let hl = assets
            .highlighter(Some("rust"), None, DEFAULT_THEME)
            .unwrap();
        let theme = DiffTheme::default();
        let doc = build_panes(
            &no_conflicts(),
            BASE,
            BASE,
            BASE,
            &hl,
            &multi_differ(),
            &theme,
        );

        assert_eq!(doc.len(), 5, "the trailing empty row is dropped");
        assert!(doc.rows.iter().all(|r| !r.changed));
        assert!(doc.hunks.is_empty());
        assert!(
            doc.rows
                .iter()
                .flat_map(|r| &r.cells)
                .all(|c| c.bg.is_none())
        );
    }

    #[test]
    fn multi_line_conflict_produces_the_expected_gaps() {
        let (doc, theme) = build(&multi_two_way());

        // seven aligned rows, minus the dropped trailing empty row
        assert_eq!(doc.len(), 6);

        let numbers: Vec<[Option<usize>; 3]> = doc
            .rows
            .iter()
            .map(|r| [r.cells[0].line_no, r.cells[1].line_no, r.cells[2].line_no])
            .collect();
        assert_eq!(
            numbers,
            vec![
                [Some(1), Some(1), Some(1)],
                [Some(2), Some(2), Some(2)],
                [Some(3), Some(3), None],
                [Some(4), Some(4), None],
                [Some(5), None, None],
                [Some(6), Some(5), Some(3)],
            ]
        );

        // the two-row right gap, and the left-only row gapped on both sides
        assert_eq!(doc.rows[2].cell(Side::Right).bg, Some(theme.gap_bg));
        assert_eq!(doc.rows[3].cell(Side::Right).bg, Some(theme.gap_bg));
        assert_eq!(doc.rows[4].cell(Side::Base).bg, Some(theme.gap_bg));
        assert_eq!(doc.rows[4].cell(Side::Right).bg, Some(theme.gap_bg));
        assert_eq!(doc.rows[4].cell(Side::Left).text(), "    w");
        assert!(doc.rows[4].cell(Side::Base).spans.is_empty());
        assert!(doc.rows[4].cell(Side::Right).spans.is_empty());
    }

    #[test]
    fn consecutive_changed_rows_form_a_single_hunk() {
        let (doc, _) = build(&multi_two_way());
        // rows 1..=4 all differ; row 0 and the closing brace do not
        assert_eq!(doc.hunks, vec![1]);
        assert!(!doc.rows[0].changed);
        assert!(doc.rows[1..5].iter().all(|r| r.changed));
    }

    #[test]
    fn changed_lines_get_their_side_tint_and_emphasis() {
        let (doc, theme) = build(&multi_two_way());
        let row = &doc.rows[1];

        assert_eq!(row.cell(Side::Left).bg, Some(theme.left_bg));
        assert_eq!(row.cell(Side::Base).bg, Some(theme.base_bg));
        assert_eq!(row.cell(Side::Right).bg, Some(theme.right_bg));

        let emphasized = |side: Side, emph: Color| -> String {
            row.cell(side)
                .spans
                .iter()
                .filter(|s| s.bg == Some(emph))
                .map(|s| s.text.as_str())
                .collect()
        };
        assert_eq!(emphasized(Side::Left, theme.left_emph_bg), "10");
        assert_eq!(emphasized(Side::Right, theme.right_emph_bg), "x * 3");
        // the base cell takes the union of what both sides touched
        assert_eq!(emphasized(Side::Base, theme.base_emph_bg), "let y = x + 1;");
    }

    #[test]
    fn unchanged_rows_keep_the_page_background() {
        let (doc, _) = build(&multi_two_way());
        assert!(doc.rows[0].cells.iter().all(|c| c.bg.is_none()));
        assert!(doc.rows[5].cells.iter().all(|c| c.bg.is_none()));
    }

    #[test]
    fn cell_spans_reconstruct_the_original_lines() {
        let (doc, _) = build(&multi_two_way());
        assert_eq!(
            doc.rows[0].cell(Side::Left).text(),
            "fn compute(x: i32) -> i32 {"
        );
        assert_eq!(doc.rows[1].cell(Side::Left).text(), "    let y = x + 10;");
        assert_eq!(doc.rows[1].cell(Side::Base).text(), "    let y = x + 1;");
        assert_eq!(doc.rows[1].cell(Side::Right).text(), "    x * 3");
        assert_eq!(doc.rows[5].cell(Side::Right).text(), "}");
    }

    #[test]
    fn a_file_without_a_trailing_newline_keeps_its_last_line() {
        let assets = Assets::new();
        let hl = assets
            .highlighter(Some("rust"), None, DEFAULT_THEME)
            .unwrap();
        let theme = DiffTheme::default();
        let text = "let a = 1;\nlet b = 2;";
        let doc = build_panes(
            &no_conflicts(),
            text,
            text,
            text,
            &hl,
            &multi_differ(),
            &theme,
        );
        assert_eq!(doc.len(), 2);
        assert_eq!(doc.rows[1].cell(Side::Base).text(), "let b = 2;");
    }

    #[test]
    fn an_empty_side_renders_as_gaps() {
        use crate::external::difft::DiffResult;
        struct Empty;
        impl Differ for Empty {
            fn diff(&self, lhs: &str, rhs: &str) -> DiffResult {
                if rhs.is_empty() {
                    DiffResult {
                        aligned: (0..line_count(lhs) as u32)
                            .map(|i| (Some(i), None))
                            .collect(),
                        ..DiffResult::default()
                    }
                } else {
                    DiffResult {
                        aligned: (0..line_count(lhs) as u32)
                            .map(|i| (Some(i), Some(i)))
                            .collect(),
                        ..DiffResult::default()
                    }
                }
            }
        }
        let assets = Assets::new();
        let hl = assets
            .highlighter(Some("rust"), None, DEFAULT_THEME)
            .unwrap();
        let theme = DiffTheme::default();
        let doc = build_panes(&no_conflicts(), BASE, BASE, "", &hl, &Empty, &theme);

        assert!(!doc.is_empty());
        assert!(doc.rows.iter().all(|r| r.cell(Side::Right).is_gap()));
        assert!(doc.rows.iter().all(|r| r.changed));
        assert_eq!(doc.hunks, vec![0]);
    }

    /// A session whose single conflict covers base lines 1..4 — the shape
    /// mergiraf produces for the `multi` fixture.
    fn multi_session() -> MergeSession {
        use crate::merge::diff3::MergedChunk;
        let own = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        MergeSession::new(
            vec![
                MergedChunk::Resolved {
                    lines: own(&["fn compute(x: i32) -> i32 {"]),
                },
                MergedChunk::Conflict {
                    left: own(&[
                        "    let y = x + 10;",
                        "    let z = y * 2;",
                        "    let w = z - 1;",
                        "    w",
                    ]),
                    base: own(&["    let y = x + 1;", "    let z = y * 2;", "    z"]),
                    right: own(&["    x * 3"]),
                },
                MergedChunk::Resolved { lines: own(&["}"]) },
            ],
            Default::default(),
        )
    }

    fn build_with(session: &MergeSession) -> (PaneDocument, DiffTheme) {
        let assets = Assets::new();
        let hl = assets
            .highlighter(Some("rust"), None, DEFAULT_THEME)
            .unwrap();
        let theme = DiffTheme::default();
        let doc = build_panes(session, BASE, LEFT, RIGHT, &hl, &multi_two_way(), &theme);
        (doc, theme)
    }

    #[test]
    fn a_conflict_region_claims_its_rows_and_its_trailing_insertion() {
        let (doc, _) = build_with(&multi_session());
        let owners: Vec<Option<usize>> = doc.rows.iter().map(|r| r.conflict).collect();
        // base lines 1..4 plus the left-only insertion that trails the region
        assert_eq!(
            owners,
            vec![None, Some(0), Some(0), Some(0), Some(0), None],
            "the gap row before the region and the closing brace stay outside"
        );
        assert_eq!(doc.conflicts, vec![1..5]);
    }

    #[test]
    fn rows_in_an_undecided_region_take_the_conflict_tint() {
        let (doc, theme) = build_with(&multi_session());
        for row in &doc.rows[1..5] {
            for side in Side::ALL {
                let cell = row.cell(side);
                let expected = if cell.is_gap() {
                    theme.gap_bg
                } else {
                    theme.conflict_bg
                };
                assert_eq!(cell.bg, Some(expected), "{side:?}");
            }
            assert!(row.changed);
        }
        // rows outside the region keep the page background
        assert!(doc.rows[0].cells.iter().all(|c| c.bg.is_none()));
    }

    #[test]
    fn deciding_a_region_lights_the_chosen_side_and_dims_the_others() {
        use crate::merge::session::Resolution;

        let mut session = multi_session();
        session.set_resolution(0, Resolution::Left);
        let (doc, theme) = build_with(&session);

        for row in &doc.rows[1..5] {
            for side in Side::ALL {
                let cell = row.cell(side);
                if cell.is_gap() {
                    continue;
                }
                if side == Side::Left {
                    assert_eq!(cell.bg, Some(theme.resolved_bg), "the chosen side is lit");
                    assert!(
                        cell.spans.iter().any(|s| s.fg != Some(theme.dimmed_fg)),
                        "the chosen side keeps its syntax colours"
                    );
                } else {
                    assert_eq!(cell.bg, Some(theme.dimmed_bg), "{side:?} is dimmed");
                    assert!(
                        cell.spans.iter().all(|s| s.fg == Some(theme.dimmed_fg)),
                        "{side:?} is flattened to grey"
                    );
                }
            }
        }
    }

    #[test]
    fn changing_the_choice_moves_the_highlight() {
        use crate::merge::session::Resolution;

        let lit = |resolution| {
            let mut session = multi_session();
            session.set_resolution(0, resolution);
            let (doc, theme) = build_with(&session);
            Side::ALL
                .into_iter()
                .filter(|&side| {
                    let cell = doc.rows[1].cell(side);
                    !cell.is_gap() && cell.bg == Some(theme.resolved_bg)
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(lit(Resolution::Left), vec![Side::Left]);
        assert_eq!(lit(Resolution::Right), vec![Side::Right]);
        assert_eq!(lit(Resolution::Base), vec![Side::Base]);
        // taking both keeps left and right, and discards base
        assert_eq!(
            lit(Resolution::BothLeftFirst),
            vec![Side::Left, Side::Right]
        );
        assert_eq!(
            lit(Resolution::BothRightFirst),
            vec![Side::Left, Side::Right]
        );
    }

    #[test]
    fn an_undecided_region_keeps_every_side_lit_and_coloured() {
        let (doc, theme) = build_with(&multi_session());
        for side in Side::ALL {
            let cell = doc.rows[1].cell(side);
            if cell.is_gap() {
                continue;
            }
            assert_eq!(cell.bg, Some(theme.conflict_bg), "{side:?}");
            assert!(
                cell.spans.iter().all(|s| s.fg != Some(theme.dimmed_fg)),
                "{side:?} should keep its syntax colours while undecided"
            );
        }
    }

    #[test]
    fn gap_cells_stay_gaps_whatever_the_resolution() {
        use crate::merge::session::Resolution;

        for resolution in [
            Resolution::Unresolved,
            Resolution::Left,
            Resolution::Right,
            Resolution::BothLeftFirst,
        ] {
            let mut session = multi_session();
            session.set_resolution(0, resolution);
            let (doc, theme) = build_with(&session);
            // row 4 is the left-only insertion: base and right have no line
            let row = &doc.rows[4];
            assert_eq!(
                row.cell(Side::Base).bg,
                Some(theme.gap_bg),
                "{resolution:?}"
            );
            assert_eq!(
                row.cell(Side::Right).bg,
                Some(theme.gap_bg),
                "{resolution:?}"
            );
        }
    }

    #[test]
    fn a_region_tint_overrides_the_per_line_change_tint() {
        // without a session the `let y` row is tinted per side...
        let (plain, theme) = build(&multi_two_way());
        assert_eq!(plain.rows[1].cell(Side::Left).bg, Some(theme.left_bg));
        // ...and with one it belongs to the region instead
        let (regioned, _) = build_with(&multi_session());
        assert_eq!(
            regioned.rows[1].cell(Side::Left).bg,
            Some(theme.conflict_bg)
        );
    }

    #[test]
    fn a_session_with_no_conflicts_tags_no_rows() {
        let (doc, _) = build_with(&no_conflicts());
        assert!(doc.rows.iter().all(|r| r.conflict.is_none()));
        assert!(doc.conflicts.is_empty());
    }

    #[test]
    fn a_region_matching_no_rows_still_gets_an_entry() {
        use crate::merge::diff3::MergedChunk;
        // a base side that appears nowhere in the base file
        let session = MergeSession::new(
            vec![MergedChunk::Conflict {
                left: vec!["l".into()],
                base: vec!["nowhere in base".into()],
                right: vec!["r".into()],
            }],
            Default::default(),
        );
        let (doc, _) = build_with(&session);
        assert_eq!(doc.conflicts.len(), 1, "the region keeps its slot");
    }

    /// Three files whose two conflicts sit at base lines 1 and 5, with an
    /// auto-merged region between them.
    const C_BASE: &str = "fn one() {\n    1\n}\nfn two() {\n    2\n}\n";
    const C_LEFT: &str = "fn one() {\n    10\n}\nfn two() {\n    20\n}\n";
    const C_RIGHT: &str = "fn one() {\n    11\n}\nfn two() {\n    22\n}\n";

    fn two_region_session() -> MergeSession {
        use crate::merge::diff3::MergedChunk;
        let own = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        MergeSession::new(
            vec![
                MergedChunk::Resolved {
                    lines: own(&["fn one() {"]),
                },
                MergedChunk::Conflict {
                    left: own(&["    10"]),
                    base: own(&["    1"]),
                    right: own(&["    11"]),
                },
                MergedChunk::Resolved {
                    lines: own(&["}", "fn two() {"]),
                },
                MergedChunk::Conflict {
                    left: own(&["    20"]),
                    base: own(&["    2"]),
                    right: own(&["    22"]),
                },
                MergedChunk::Resolved { lines: own(&["}"]) },
            ],
            Default::default(),
        )
    }

    #[test]
    fn several_regions_each_claim_their_own_rows() {
        let assets = Assets::new();
        let hl = assets
            .highlighter(Some("rust"), None, DEFAULT_THEME)
            .unwrap();
        let theme = DiffTheme::default();
        let session = two_region_session();
        let doc = build_panes(
            &session,
            C_BASE,
            C_LEFT,
            C_RIGHT,
            &hl,
            &multi_differ(),
            &theme,
        );

        assert_eq!(doc.conflicts, vec![1..2, 4..5]);
        let owners: Vec<Option<usize>> = doc.rows.iter().map(|r| r.conflict).collect();
        assert_eq!(
            owners,
            vec![None, Some(0), None, None, Some(1), None],
            "only the conflicting lines belong to a region"
        );
    }

    #[test]
    fn regions_are_tinted_according_to_their_own_resolution() {
        let assets = Assets::new();
        let hl = assets
            .highlighter(Some("rust"), None, DEFAULT_THEME)
            .unwrap();
        let theme = DiffTheme::default();
        let mut session = two_region_session();
        session.set_resolution(0, crate::merge::session::Resolution::Left);

        let doc = build_panes(
            &session,
            C_BASE,
            C_LEFT,
            C_RIGHT,
            &hl,
            &multi_differ(),
            &theme,
        );
        // region 0 is decided, region 1 is not
        assert_eq!(
            doc.rows[1].cell(Side::Left).bg,
            Some(theme.resolved_bg),
            "the decided region"
        );
        assert_eq!(
            doc.rows[4].cell(Side::Left).bg,
            Some(theme.conflict_bg),
            "the one still open"
        );
    }

    #[test]
    fn empty_inputs_yield_an_empty_document() {
        let assets = Assets::new();
        let hl = assets
            .highlighter(Some("rust"), None, DEFAULT_THEME)
            .unwrap();
        let doc = build_panes(
            &no_conflicts(),
            "",
            "",
            "",
            &hl,
            &multi_differ(),
            &DiffTheme::default(),
        );
        assert!(doc.is_empty());
    }
}
