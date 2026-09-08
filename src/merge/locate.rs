//! Finds where each conflict region sits in the base file.
//!
//! Mergiraf's CLI reports no positions — only merged text with markers — so the
//! regions have to be located by matching each conflict's base side back
//! against the base file. The search is **monotonic**: a cursor advances past
//! each match, because the same base content can legitimately appear in more
//! than one conflict (base `x y x y` against two changed sides produces two
//! conflicts whose base sides are both `x`), and a plain "find first
//! occurrence" would map them onto the same line.

use std::ops::Range;

use crate::merge::diff3::MergedChunk;

/// The base-file line range each conflict covers, in file order.
///
/// An empty range means the conflict adds lines at that point without consuming
/// any base line — either both sides inserted there, or the base side could not
/// be found because mergiraf transformed it. Either way the region keeps its
/// place in the sequence rather than being dropped.
pub fn conflict_base_ranges(chunks: &[MergedChunk], base_lines: &[&str]) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut cursor = 0usize;

    for chunk in chunks {
        let MergedChunk::Conflict { base, .. } = chunk else {
            continue;
        };
        if base.is_empty() {
            ranges.push(cursor..cursor);
            continue;
        }
        match find_run(base_lines, base, cursor) {
            Some(start) => {
                let end = start + base.len();
                ranges.push(start..end);
                cursor = end;
            }
            None => ranges.push(cursor..cursor),
        }
    }

    ranges
}

/// First index at or after `from` where `needle` occurs in `haystack`.
fn find_run(haystack: &[&str], needle: &[String], from: usize) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    (from..=haystack.len() - needle.len()).find(|&i| {
        haystack[i..i + needle.len()]
            .iter()
            .zip(needle)
            .all(|(h, n)| *h == n)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn own(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn conflict(left: &[&str], base: &[&str], right: &[&str]) -> MergedChunk {
        MergedChunk::Conflict {
            left: own(left),
            base: own(base),
            right: own(right),
        }
    }

    #[test]
    fn locates_a_multi_line_conflict() {
        // the `tests/fixtures/multi` shape
        let base_lines = [
            "fn compute(x: i32) -> i32 {",
            "    let y = x + 1;",
            "    let z = y * 2;",
            "    z",
            "}",
        ];
        let chunks = vec![
            MergedChunk::Resolved {
                lines: own(&["fn compute(x: i32) -> i32 {"]),
            },
            conflict(
                &["    let y = x + 10;", "    w"],
                &["    let y = x + 1;", "    let z = y * 2;", "    z"],
                &["    x * 3"],
            ),
            MergedChunk::Resolved { lines: own(&["}"]) },
        ];
        assert_eq!(conflict_base_ranges(&chunks, &base_lines), vec![1..4]);
    }

    #[test]
    fn repeated_base_content_maps_to_distinct_regions() {
        // Verified against mergiraf: base `x y x y` with both sides changing the
        // `x` lines yields two conflicts whose base sides are both `x`.
        let base_lines = ["x", "y", "x", "y"];
        let chunks = vec![
            conflict(&["L1"], &["x"], &["R1"]),
            MergedChunk::Resolved { lines: own(&["y"]) },
            conflict(&["L2"], &["x"], &["R2"]),
            MergedChunk::Resolved { lines: own(&["y"]) },
        ];
        assert_eq!(
            conflict_base_ranges(&chunks, &base_lines),
            vec![0..1, 2..3],
            "the cursor must stop the second conflict re-matching the first `x`"
        );
    }

    #[test]
    fn an_empty_base_side_yields_an_empty_range_at_the_cursor() {
        let base_lines = ["a", "b", "c"];
        let chunks = vec![
            conflict(&["L"], &["a", "b"], &["R"]),
            conflict(&["ins_l"], &[], &["ins_r"]),
        ];
        assert_eq!(conflict_base_ranges(&chunks, &base_lines), vec![0..2, 2..2]);
    }

    #[test]
    fn an_unfindable_base_side_keeps_its_place_instead_of_being_dropped() {
        let base_lines = ["a", "b"];
        let chunks = vec![
            conflict(&["L"], &["a"], &["R"]),
            conflict(&["L2"], &["mergiraf rewrote this"], &["R2"]),
            conflict(&["L3"], &["b"], &["R3"]),
        ];
        let ranges = conflict_base_ranges(&chunks, &base_lines);
        assert_eq!(ranges.len(), 3, "every conflict keeps a range");
        assert_eq!(ranges[0], 0..1);
        assert!(ranges[1].is_empty());
        assert_eq!(ranges[2], 1..2);
    }

    /// The `tests/fixtures/chunks` shape: three conflicts of different sizes
    /// with auto-merged content in between.
    #[test]
    fn several_conflicts_get_distinct_ordered_ranges() {
        let base_lines = [
            "fn one() -> i32 {",
            "    1",
            "}",
            "",
            "fn two() -> i32 {",
            "    2",
            "}",
            "",
            "fn three() -> i32 {",
            "    3",
            "}",
            "",
            "fn four() -> i32 {",
            "    4",
            "}",
            "",
        ];
        let chunks = vec![
            MergedChunk::Resolved {
                lines: own(&["fn one() -> i32 {"]),
            },
            conflict(&["    10"], &["    1"], &["    11"]),
            // the auto-merged `fn helper` that only left added, plus `fn two`
            MergedChunk::Resolved {
                lines: own(&[
                    "}",
                    "",
                    "fn helper() -> i32 {",
                    "    0",
                    "}",
                    "",
                    "fn two() -> i32 {",
                    "    2",
                    "}",
                    "",
                    "fn three() -> i32 {",
                ]),
            },
            conflict(
                &["    let a = 30;", "    let b = 0;", "    a + b"],
                &["    3"],
                &["    33"],
            ),
            MergedChunk::Resolved {
                lines: own(&["}", "", "fn four() -> i32 {"]),
            },
            conflict(&["    40"], &["    4"], &["    44", "    // note"]),
            MergedChunk::Resolved { lines: own(&["}"]) },
        ];

        let ranges = conflict_base_ranges(&chunks, &base_lines);
        assert_eq!(ranges, vec![1..2, 9..10, 13..14]);
        for pair in ranges.windows(2) {
            assert!(pair[0].end <= pair[1].start, "{pair:?} overlap");
        }
    }

    #[test]
    fn no_conflicts_yields_no_ranges() {
        let chunks = vec![MergedChunk::Resolved {
            lines: own(&["a", "b"]),
        }];
        assert!(conflict_base_ranges(&chunks, &["a", "b"]).is_empty());
    }

    #[test]
    fn a_conflict_covering_the_whole_base_file() {
        let base_lines = ["a", "b"];
        let chunks = vec![conflict(&["L"], &["a", "b"], &["R"])];
        assert_eq!(conflict_base_ranges(&chunks, &base_lines), vec![0..2]);
    }

    #[test]
    fn an_empty_base_file_gives_every_conflict_an_empty_range() {
        let chunks = vec![conflict(&["L"], &["a"], &["R"])];
        assert_eq!(conflict_base_ranges(&chunks, &[]), vec![0..0]);
    }

    #[test]
    fn ranges_come_out_in_order_and_never_overlap() {
        let base_lines = ["a", "b", "c", "d", "e"];
        let chunks = vec![
            conflict(&["L"], &["a"], &["R"]),
            conflict(&["L"], &["b", "c"], &["R"]),
            conflict(&["L"], &["e"], &["R"]),
        ];
        let ranges = conflict_base_ranges(&chunks, &base_lines);
        assert_eq!(ranges, vec![0..1, 1..3, 4..5]);
        for pair in ranges.windows(2) {
            assert!(pair[0].end <= pair[1].start, "{pair:?} overlap");
        }
    }
}
