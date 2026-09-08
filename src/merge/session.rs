//! Resolution state over a parsed merge, and the file it writes out.
//!
//! Conflict markers are git's interchange format, not something this tool
//! reasons in: the unit of work is a **conflict region with a chosen
//! resolution**, and the output is assembled from auto-merged content plus
//! those choices. Markers reappear in exactly one place — a region still
//! undecided at save time is written back in diff3 form so an unfinished merge
//! stays readable to git and to an editor.

use crate::merge::diff3::{DEFAULT_MARKER_SIZE, MergedChunk};
use crate::render::theme::Side;

/// What the user chose for one conflict region.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Resolution {
    #[default]
    Unresolved,
    Left,
    Base,
    Right,
    BothLeftFirst,
    BothRightFirst,
}

impl Resolution {
    pub fn is_resolved(self) -> bool {
        self != Self::Unresolved
    }

    /// Whether `side` contributes to the resolved output, or `None` while the
    /// region is still undecided.
    ///
    /// The single answer to "which side won", so the panes and anything else
    /// that displays a decision agree on it.
    pub fn takes(self, side: Side) -> Option<bool> {
        let taken = match self {
            Self::Unresolved => return None,
            Self::Left => side == Side::Left,
            Self::Base => side == Side::Base,
            Self::Right => side == Side::Right,
            // Both orders keep left and right; base is what they discard.
            Self::BothLeftFirst | Self::BothRightFirst => side != Side::Base,
        };
        Some(taken)
    }

    /// Short word for the status bar and region headers.
    pub fn label(self) -> &'static str {
        match self {
            Self::Unresolved => "unresolved",
            Self::Left => "left",
            Self::Base => "base",
            Self::Right => "right",
            Self::BothLeftFirst => "both (left first)",
            Self::BothRightFirst => "both (right first)",
        }
    }
}

/// Names written into conflict markers for regions still undecided.
#[derive(Clone, Debug)]
pub struct MarkerLabels {
    pub left: String,
    pub base: String,
    pub right: String,
    pub size: usize,
}

impl Default for MarkerLabels {
    fn default() -> Self {
        Self {
            left: "LEFT".into(),
            base: "BASE".into(),
            right: "RIGHT".into(),
            size: DEFAULT_MARKER_SIZE,
        }
    }
}

/// A parsed merge plus the resolution chosen for each of its conflicts.
#[derive(Clone, Debug)]
pub struct MergeSession {
    chunks: Vec<MergedChunk>,
    /// One entry per conflict, in file order.
    resolutions: Vec<Resolution>,
    labels: MarkerLabels,
}

impl MergeSession {
    pub fn new(chunks: Vec<MergedChunk>, labels: MarkerLabels) -> Self {
        let count = chunks
            .iter()
            .filter(|c| matches!(c, MergedChunk::Conflict { .. }))
            .count();
        Self {
            chunks,
            resolutions: vec![Resolution::Unresolved; count],
            labels,
        }
    }

    pub fn chunks(&self) -> &[MergedChunk] {
        &self.chunks
    }

    pub fn labels(&self) -> &MarkerLabels {
        &self.labels
    }

    /// The conflict regions, in file order.
    pub fn conflicts(&self) -> impl Iterator<Item = (&[String], &[String], &[String])> {
        self.chunks.iter().filter_map(|c| match c {
            MergedChunk::Conflict { left, base, right } => {
                Some((left.as_slice(), base.as_slice(), right.as_slice()))
            }
            MergedChunk::Resolved { .. } => None,
        })
    }

    pub fn conflict_count(&self) -> usize {
        self.resolutions.len()
    }

    pub fn resolved_count(&self) -> usize {
        self.resolutions.iter().filter(|r| r.is_resolved()).count()
    }

    pub fn is_fully_resolved(&self) -> bool {
        self.resolved_count() == self.conflict_count()
    }

    pub fn resolution(&self, index: usize) -> Resolution {
        self.resolutions.get(index).copied().unwrap_or_default()
    }

    pub fn set_resolution(&mut self, index: usize, resolution: Resolution) {
        if let Some(slot) = self.resolutions.get_mut(index) {
            *slot = resolution;
        }
    }

    /// The lines conflict `index` contributes to the output, or `None` while it
    /// is still undecided.
    pub fn resolved_lines(&self, index: usize) -> Option<Vec<String>> {
        let (left, base, right) = self.conflicts().nth(index)?;
        let joined = |a: &[String], b: &[String]| {
            let mut v = a.to_vec();
            v.extend_from_slice(b);
            v
        };
        match self.resolution(index) {
            Resolution::Unresolved => None,
            Resolution::Left => Some(left.to_vec()),
            Resolution::Base => Some(base.to_vec()),
            Resolution::Right => Some(right.to_vec()),
            Resolution::BothLeftFirst => Some(joined(left, right)),
            Resolution::BothRightFirst => Some(joined(right, left)),
        }
    }

    /// The file to write: chosen content for decided regions, diff3 markers for
    /// the rest.
    ///
    /// A fully resolved session produces a file with no markers in it at all.
    pub fn to_output(&self) -> String {
        let mut out = String::new();
        let mut conflict = 0usize;

        for chunk in &self.chunks {
            match chunk {
                MergedChunk::Resolved { lines } => {
                    for line in lines {
                        out.push_str(line);
                        out.push('\n');
                    }
                }
                MergedChunk::Conflict { left, base, right } => {
                    match self.resolved_lines(conflict) {
                        Some(lines) => {
                            for line in lines {
                                out.push_str(&line);
                                out.push('\n');
                            }
                        }
                        None => self.write_markers(&mut out, left, base, right),
                    }
                    conflict += 1;
                }
            }
        }
        out
    }

    fn write_markers(&self, out: &mut String, left: &[String], base: &[String], right: &[String]) {
        let m = |c: char| c.to_string().repeat(self.labels.size);
        let section = |out: &mut String, lines: &[String]| {
            for line in lines {
                out.push_str(line);
                out.push('\n');
            }
        };

        out.push_str(&format!("{} {}\n", m('<'), self.labels.left));
        section(out, left);
        out.push_str(&format!("{} {}\n", m('|'), self.labels.base));
        section(out, base);
        out.push_str(&format!("{}\n", m('=')));
        section(out, right);
        out.push_str(&format!("{} {}\n", m('>'), self.labels.right));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge::diff3::parse_diff3;

    fn own(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn sample() -> MergeSession {
        let chunks = vec![
            MergedChunk::Resolved {
                lines: own(&["fn main() {"]),
            },
            MergedChunk::Conflict {
                left: own(&["    let a = 100;"]),
                base: own(&["    let a = 1;"]),
                right: own(&["    let a = 999;"]),
            },
            MergedChunk::Resolved {
                lines: own(&["    let b = 2;"]),
            },
            MergedChunk::Conflict {
                left: own(&["    x();"]),
                base: own(&[]),
                right: own(&["    y();"]),
            },
            MergedChunk::Resolved { lines: own(&["}"]) },
        ];
        MergeSession::new(chunks, MarkerLabels::default())
    }

    #[test]
    fn a_new_session_is_entirely_unresolved() {
        let s = sample();
        assert_eq!(s.conflict_count(), 2);
        assert_eq!(s.resolved_count(), 0);
        assert!(!s.is_fully_resolved());
        assert_eq!(s.resolution(0), Resolution::Unresolved);
        assert_eq!(s.resolved_lines(0), None);
    }

    #[test]
    fn each_resolution_picks_the_right_lines() {
        let mut s = sample();
        s.set_resolution(0, Resolution::Left);
        assert_eq!(s.resolved_lines(0), Some(own(&["    let a = 100;"])));
        s.set_resolution(0, Resolution::Base);
        assert_eq!(s.resolved_lines(0), Some(own(&["    let a = 1;"])));
        s.set_resolution(0, Resolution::Right);
        assert_eq!(s.resolved_lines(0), Some(own(&["    let a = 999;"])));
    }

    #[test]
    fn taking_both_differs_in_order() {
        let mut s = sample();
        s.set_resolution(1, Resolution::BothLeftFirst);
        assert_eq!(s.resolved_lines(1), Some(own(&["    x();", "    y();"])));
        s.set_resolution(1, Resolution::BothRightFirst);
        assert_eq!(s.resolved_lines(1), Some(own(&["    y();", "    x();"])));
    }

    #[test]
    fn resolved_count_tracks_choices_both_ways() {
        let mut s = sample();
        s.set_resolution(0, Resolution::Left);
        assert_eq!(s.resolved_count(), 1);
        s.set_resolution(1, Resolution::Right);
        assert!(s.is_fully_resolved());
        s.set_resolution(0, Resolution::Unresolved);
        assert_eq!(s.resolved_count(), 1);
    }

    #[test]
    fn a_fully_resolved_session_writes_no_markers_at_all() {
        let mut s = sample();
        s.set_resolution(0, Resolution::Left);
        s.set_resolution(1, Resolution::Right);
        let out = s.to_output();

        for marker in ['<', '|', '=', '>'] {
            assert!(
                !out.contains(&marker.to_string().repeat(DEFAULT_MARKER_SIZE)),
                "found a {marker:?} marker in:\n{out}"
            );
        }
        assert_eq!(
            out,
            "fn main() {\n    let a = 100;\n    let b = 2;\n    y();\n}\n"
        );
    }

    #[test]
    fn an_unresolved_session_round_trips_through_the_parser() {
        let s = sample();
        let reparsed = parse_diff3(&s.to_output(), DEFAULT_MARKER_SIZE);
        assert_eq!(reparsed, s.chunks());
    }

    #[test]
    fn a_mixed_session_marks_up_only_the_undecided_regions() {
        let mut s = sample();
        s.set_resolution(0, Resolution::Left);
        let out = s.to_output();

        assert_eq!(out.matches("<<<<<<<").count(), 1, "in:\n{out}");
        assert!(out.contains("    let a = 100;"));
        assert!(
            !out.contains("    let a = 999;"),
            "the resolved region kept a candidate"
        );
        // and what comes back out still describes the same two regions
        let reparsed = parse_diff3(&out, DEFAULT_MARKER_SIZE);
        assert_eq!(
            reparsed
                .iter()
                .filter(|c| matches!(c, MergedChunk::Conflict { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn an_empty_base_side_still_round_trips() {
        let s = sample();
        let out = s.to_output();
        assert!(out.contains("|||||||"), "in:\n{out}");
        assert_eq!(parse_diff3(&out, DEFAULT_MARKER_SIZE), s.chunks());
    }

    #[test]
    fn custom_marker_labels_and_size_are_used() {
        let chunks = vec![MergedChunk::Conflict {
            left: own(&["l"]),
            base: own(&["b"]),
            right: own(&["r"]),
        }];
        let labels = MarkerLabels {
            left: "ours".into(),
            base: "ancestor".into(),
            right: "theirs".into(),
            size: 4,
        };
        let s = MergeSession::new(chunks, labels);
        let out = s.to_output();
        assert!(out.starts_with("<<<< ours\n"), "in:\n{out}");
        assert!(out.contains("|||| ancestor\n"));
        assert!(out.contains("====\n"));
        assert!(out.ends_with(">>>> theirs\n"));
        assert_eq!(parse_diff3(&out, 4), s.chunks());
    }

    /// Three conflicts of different shapes, with auto-merged content between
    /// them — the `tests/fixtures/chunks` shape.
    fn three_regions() -> MergeSession {
        let chunks = vec![
            MergedChunk::Resolved {
                lines: own(&["fn one() -> i32 {"]),
            },
            MergedChunk::Conflict {
                left: own(&["    10"]),
                base: own(&["    1"]),
                right: own(&["    11"]),
            },
            MergedChunk::Resolved {
                lines: own(&["}", "fn three() -> i32 {"]),
            },
            MergedChunk::Conflict {
                left: own(&["    let a = 30;", "    let b = 0;", "    a + b"]),
                base: own(&["    3"]),
                right: own(&["    33"]),
            },
            MergedChunk::Resolved {
                lines: own(&["}", "fn four() -> i32 {"]),
            },
            MergedChunk::Conflict {
                left: own(&["    40"]),
                base: own(&["    4"]),
                right: own(&["    44", "    // note"]),
            },
            MergedChunk::Resolved { lines: own(&["}"]) },
        ];
        MergeSession::new(chunks, MarkerLabels::default())
    }

    #[test]
    fn regions_resolve_independently_of_one_another() {
        let mut s = three_regions();
        assert_eq!(s.conflict_count(), 3);

        s.set_resolution(0, Resolution::Left);
        s.set_resolution(1, Resolution::Right);
        s.set_resolution(2, Resolution::Base);
        assert!(s.is_fully_resolved());

        assert_eq!(
            s.to_output(),
            "fn one() -> i32 {\n    10\n}\nfn three() -> i32 {\n    33\n}\nfn four() -> i32 {\n    4\n}\n"
        );
        // changing one region leaves the others alone
        s.set_resolution(1, Resolution::Left);
        assert_eq!(s.resolution(0), Resolution::Left);
        assert_eq!(s.resolution(2), Resolution::Base);
        assert!(s.to_output().contains("    a + b"));
    }

    #[test]
    fn a_partly_resolved_multi_region_session_marks_up_only_what_is_left() {
        let mut s = three_regions();
        s.set_resolution(0, Resolution::Left);
        s.set_resolution(2, Resolution::Right);
        assert_eq!(s.resolved_count(), 2);

        let out = s.to_output();
        assert_eq!(
            out.matches("<<<<<<<").count(),
            1,
            "only region 1 is open:\n{out}"
        );
        // the decided regions contributed their choices, not markers
        assert!(out.contains("    10") && !out.contains("    11"));
        assert!(out.contains("    44") && out.contains("    // note") && !out.contains("    40"));
        // and region 1 kept all three candidates
        for candidate in ["    a + b", "    3", "    33"] {
            assert!(out.contains(candidate), "missing {candidate:?} in:\n{out}");
        }
    }

    #[test]
    fn a_multi_region_session_round_trips_at_every_stage_of_resolution() {
        // Whatever is still open must survive a write/parse cycle, so an
        // unfinished merge can be handed to git or an editor and come back.
        let mut s = three_regions();
        for step in 0..=2 {
            let reparsed = parse_diff3(&s.to_output(), DEFAULT_MARKER_SIZE);
            let open = reparsed
                .iter()
                .filter(|c| matches!(c, MergedChunk::Conflict { .. }))
                .count();
            assert_eq!(open, 3 - step, "after {step} resolutions");
            s.set_resolution(step, Resolution::Left);
        }
        assert!(!s.to_output().contains("<<<<<<<"));
    }

    #[test]
    fn a_session_without_conflicts_writes_its_content_back() {
        let s = MergeSession::new(
            vec![MergedChunk::Resolved {
                lines: own(&["a", "b"]),
            }],
            MarkerLabels::default(),
        );
        assert_eq!(s.conflict_count(), 0);
        assert!(
            s.is_fully_resolved(),
            "nothing to resolve is fully resolved"
        );
        assert_eq!(s.to_output(), "a\nb\n");
    }

    #[test]
    fn out_of_range_indexes_are_inert() {
        let mut s = sample();
        s.set_resolution(99, Resolution::Left);
        assert_eq!(s.resolved_count(), 0);
        assert_eq!(s.resolution(99), Resolution::Unresolved);
        assert_eq!(s.resolved_lines(99), None);
    }
}
