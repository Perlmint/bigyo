//! Parser for the diff3-style conflict output that `mergiraf merge` writes to
//! stdout.
//!
//! ```text
//! <<<<<<< LEFT
//! left content
//! ||||||| BASE
//! base content
//! =======
//! right content
//! >>>>>>> RIGHT
//! ```
//!
//! Mergiraf falls back to `git merge-file` in a few situations, which can emit
//! diff2 conflicts (no `|||||||` section), so those are accepted too — with an
//! empty base side.

pub const DEFAULT_MARKER_SIZE: usize = 7;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergedChunk {
    /// A run of lines mergiraf merged successfully.
    Resolved { lines: Vec<String> },
    /// A conflict hunk, one line list per side.
    Conflict {
        left: Vec<String>,
        base: Vec<String>,
        right: Vec<String>,
    },
}

/// True if `line` opens with exactly `size` copies of `c`, followed by end of
/// line or a space. The "followed by" check is what keeps a line of `=========`
/// in the source from being mistaken for a marker.
fn is_marker(line: &str, c: char, size: usize) -> bool {
    let mut chars = line.chars();
    for _ in 0..size {
        if chars.next() != Some(c) {
            return false;
        }
    }
    match chars.next() {
        None => true,
        Some(next) => next == ' ',
    }
}

#[derive(Clone, Copy)]
enum State {
    Outside,
    Left,
    Base,
    Right,
}

/// Split mergiraf's merged output into resolved runs and conflict hunks.
///
/// Adjacent resolved lines always land in a single [`MergedChunk::Resolved`].
pub fn parse_diff3(text: &str, marker_size: usize) -> Vec<MergedChunk> {
    let mut chunks = Vec::new();
    let mut state = State::Outside;

    let mut resolved: Vec<String> = Vec::new();
    let (mut left, mut base, mut right) = (Vec::new(), Vec::new(), Vec::new());

    let flush_conflict = |chunks: &mut Vec<MergedChunk>,
                          left: &mut Vec<String>,
                          base: &mut Vec<String>,
                          right: &mut Vec<String>| {
        chunks.push(MergedChunk::Conflict {
            left: std::mem::take(left),
            base: std::mem::take(base),
            right: std::mem::take(right),
        });
    };

    for line in text.lines() {
        match state {
            State::Outside if is_marker(line, '<', marker_size) => {
                if !resolved.is_empty() {
                    chunks.push(MergedChunk::Resolved {
                        lines: std::mem::take(&mut resolved),
                    });
                }
                state = State::Left;
            }
            State::Outside => resolved.push(line.to_owned()),

            State::Left if is_marker(line, '|', marker_size) => state = State::Base,
            // diff2: no base section at all
            State::Left if is_marker(line, '=', marker_size) => state = State::Right,
            State::Left => left.push(line.to_owned()),

            State::Base if is_marker(line, '=', marker_size) => state = State::Right,
            State::Base => base.push(line.to_owned()),

            State::Right if is_marker(line, '>', marker_size) => {
                flush_conflict(&mut chunks, &mut left, &mut base, &mut right);
                state = State::Outside;
            }
            State::Right => right.push(line.to_owned()),
        }
    }

    // Truncated input: keep whatever the conflict had rather than dropping it.
    if !matches!(state, State::Outside) {
        flush_conflict(&mut chunks, &mut left, &mut base, &mut right);
    }
    if !resolved.is_empty() {
        chunks.push(MergedChunk::Resolved { lines: resolved });
    }

    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved(lines: &[&str]) -> MergedChunk {
        MergedChunk::Resolved {
            lines: lines.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn conflict(left: &[&str], base: &[&str], right: &[&str]) -> MergedChunk {
        let own = |v: &[&str]| v.iter().map(|s| s.to_string()).collect();
        MergedChunk::Conflict {
            left: own(left),
            base: own(base),
            right: own(right),
        }
    }

    #[test]
    fn parses_a_real_mergiraf_conflict() {
        // captured verbatim from `mergiraf merge base.rs left.rs right.rs`
        let text = "\
fn main() {
<<<<<<< left.rs
    let a = 100;
||||||| base.rs
    let a = 1;
=======
    let a = 999;
>>>>>>> right.rs
    let b = 20;
}
";
        assert_eq!(
            parse_diff3(text, DEFAULT_MARKER_SIZE),
            vec![
                resolved(&["fn main() {"]),
                conflict(
                    &["    let a = 100;"],
                    &["    let a = 1;"],
                    &["    let a = 999;"]
                ),
                resolved(&["    let b = 20;", "}"]),
            ]
        );
    }

    #[test]
    fn no_conflicts_is_one_resolved_chunk() {
        let text = "a\nb\nc\n";
        assert_eq!(
            parse_diff3(text, DEFAULT_MARKER_SIZE),
            vec![resolved(&["a", "b", "c"])]
        );
    }

    #[test]
    fn conflict_at_start_and_end_of_file() {
        let text = "\
<<<<<<< LEFT
l
||||||| BASE
b
=======
r
>>>>>>> RIGHT
middle
<<<<<<< LEFT
l2
||||||| BASE
b2
=======
r2
>>>>>>> RIGHT";
        assert_eq!(
            parse_diff3(text, DEFAULT_MARKER_SIZE),
            vec![
                conflict(&["l"], &["b"], &["r"]),
                resolved(&["middle"]),
                conflict(&["l2"], &["b2"], &["r2"]),
            ]
        );
    }

    #[test]
    fn diff2_conflict_has_empty_base() {
        let text = "<<<<<<< LEFT\nl\n=======\nr\n>>>>>>> RIGHT\n";
        assert_eq!(
            parse_diff3(text, DEFAULT_MARKER_SIZE),
            vec![conflict(&["l"], &[], &["r"])]
        );
    }

    #[test]
    fn empty_sides_are_preserved() {
        let text = "<<<<<<< LEFT\n||||||| BASE\nb\n=======\n>>>>>>> RIGHT\n";
        assert_eq!(
            parse_diff3(text, DEFAULT_MARKER_SIZE),
            vec![conflict(&[], &["b"], &[])]
        );
    }

    #[test]
    fn marker_lookalikes_in_content_are_not_markers() {
        // longer runs, and runs with non-space follow-ons, are ordinary content
        let text = "\
========================
<<<<<<<<
banner ======= inline
";
        assert_eq!(
            parse_diff3(text, DEFAULT_MARKER_SIZE),
            vec![resolved(&[
                "========================",
                "<<<<<<<<",
                "banner ======= inline",
            ])]
        );
    }

    #[test]
    fn bare_markers_without_labels_are_accepted() {
        let text = "<<<<<<<\nl\n|||||||\nb\n=======\nr\n>>>>>>>\n";
        assert_eq!(
            parse_diff3(text, DEFAULT_MARKER_SIZE),
            vec![conflict(&["l"], &["b"], &["r"])]
        );
    }

    #[test]
    fn custom_marker_size() {
        let text = "<<<<\nl\n||||\nb\n====\nr\n>>>>\n";
        assert_eq!(parse_diff3(text, 4), vec![conflict(&["l"], &["b"], &["r"])]);
        // with the default size those lines are just content
        assert_eq!(parse_diff3(text, DEFAULT_MARKER_SIZE).len(), 1);
    }

    #[test]
    fn missing_trailing_newline() {
        let text = "a\nb";
        assert_eq!(
            parse_diff3(text, DEFAULT_MARKER_SIZE),
            vec![resolved(&["a", "b"])]
        );
    }

    #[test]
    fn unterminated_conflict_is_still_emitted() {
        let text = "a\n<<<<<<< LEFT\nl\n||||||| BASE\nb\n=======\nr\n";
        assert_eq!(
            parse_diff3(text, DEFAULT_MARKER_SIZE),
            vec![resolved(&["a"]), conflict(&["l"], &["b"], &["r"])]
        );
    }

    #[test]
    fn empty_input() {
        assert!(parse_diff3("", DEFAULT_MARKER_SIZE).is_empty());
    }
}
