//! Joins two base-anchored line alignments into one three-way alignment.
//!
//! Difftastic gives us `base → left` and `base → right` separately. Both
//! enumerate every base line exactly once and in order, so the two can be
//! merge-joined on the base line number to get rows that line up all three
//! revisions at once — which is what the side-by-side view draws.

/// One row of the three-way alignment. Each field is a 0-based line number in
/// that file, or `None` where that file has no line at this row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AlignedRow {
    pub left: Option<u32>,
    pub base: Option<u32>,
    pub right: Option<u32>,
}

impl AlignedRow {
    /// True when some file has no line here, i.e. the row is a gap on at least
    /// one side.
    pub fn has_gap(&self) -> bool {
        self.left.is_none() || self.base.is_none() || self.right.is_none()
    }
}

/// Merge-join `base → left` and `base → right` into one three-way alignment.
///
/// Insertions (rows where the base side is `None`) belong to only one of the
/// two inputs, so they are emitted on their own with gaps in the other two
/// columns. Left insertions are emitted before right ones, which stacks
/// simultaneous insertions as a left block then a right block rather than
/// interleaving them line by line.
pub fn align3(
    base_left: &[(Option<u32>, Option<u32>)],
    base_right: &[(Option<u32>, Option<u32>)],
) -> Vec<AlignedRow> {
    let mut rows = Vec::with_capacity(base_left.len().max(base_right.len()));
    let (mut i, mut j) = (0usize, 0usize);

    while i < base_left.len() || j < base_right.len() {
        let bl = base_left.get(i).copied();
        let br = base_right.get(j).copied();

        match (bl, br) {
            // A line only the left file has.
            (Some((None, left)), _) => {
                rows.push(AlignedRow {
                    left,
                    base: None,
                    right: None,
                });
                i += 1;
            }
            // A line only the right file has.
            (_, Some((None, right))) => {
                rows.push(AlignedRow {
                    left: None,
                    base: None,
                    right,
                });
                j += 1;
            }
            (Some((Some(b_left), left)), Some((Some(b_right), right))) => {
                // Both alignments walk the base in order, so equal is the
                // normal case; the others only guard against difftastic
                // reporting something we did not expect.
                match b_left.cmp(&b_right) {
                    std::cmp::Ordering::Equal => {
                        rows.push(AlignedRow {
                            left,
                            base: Some(b_left),
                            right,
                        });
                        i += 1;
                        j += 1;
                    }
                    std::cmp::Ordering::Less => {
                        rows.push(AlignedRow {
                            left,
                            base: Some(b_left),
                            right: None,
                        });
                        i += 1;
                    }
                    std::cmp::Ordering::Greater => {
                        rows.push(AlignedRow {
                            left: None,
                            base: Some(b_right),
                            right,
                        });
                        j += 1;
                    }
                }
            }
            // One input is exhausted; drain the other.
            (Some((base, left)), None) => {
                rows.push(AlignedRow {
                    left,
                    base,
                    right: None,
                });
                i += 1;
            }
            (None, Some((base, right))) => {
                rows.push(AlignedRow {
                    left: None,
                    base,
                    right,
                });
                j += 1;
            }
            (None, None) => break,
        }
    }

    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(left: Option<u32>, base: Option<u32>, right: Option<u32>) -> AlignedRow {
        AlignedRow { left, base, right }
    }

    /// `base_left` and `base_right` for two files identical to the base.
    fn identity(n: u32) -> Vec<(Option<u32>, Option<u32>)> {
        (0..n).map(|i| (Some(i), Some(i))).collect()
    }

    #[test]
    fn identical_files_align_one_to_one() {
        let rows = align3(&identity(4), &identity(4));
        assert_eq!(
            rows,
            (0..4)
                .map(|i| row(Some(i), Some(i), Some(i)))
                .collect::<Vec<_>>()
        );
        assert!(rows.iter().all(|r| !r.has_gap()));
    }

    #[test]
    fn a_left_only_insertion_gaps_the_other_two_panes() {
        // left inserts a line after base line 0
        let bl = vec![(Some(0), Some(0)), (None, Some(1)), (Some(1), Some(2))];
        assert_eq!(
            align3(&bl, &identity(2)),
            vec![
                row(Some(0), Some(0), Some(0)),
                row(Some(1), None, None),
                row(Some(2), Some(1), Some(1)),
            ]
        );
    }

    #[test]
    fn a_right_only_insertion_gaps_the_other_two_panes() {
        let br = vec![(Some(0), Some(0)), (None, Some(1)), (Some(1), Some(2))];
        assert_eq!(
            align3(&identity(2), &br),
            vec![
                row(Some(0), Some(0), Some(0)),
                row(None, None, Some(1)),
                row(Some(1), Some(1), Some(2)),
            ]
        );
    }

    #[test]
    fn a_deletion_keeps_the_base_line_with_a_gap_on_that_side() {
        // right deletes base line 1
        let br = vec![(Some(0), Some(0)), (Some(1), None), (Some(2), Some(1))];
        assert_eq!(
            align3(&identity(3), &br),
            vec![
                row(Some(0), Some(0), Some(0)),
                row(Some(1), Some(1), None),
                row(Some(2), Some(2), Some(1)),
            ]
        );
    }

    #[test]
    fn simultaneous_insertions_stack_left_block_then_right_block() {
        let bl = vec![(Some(0), Some(0)), (None, Some(1)), (Some(1), Some(2))];
        let br = vec![(Some(0), Some(0)), (None, Some(1)), (Some(1), Some(2))];
        assert_eq!(
            align3(&bl, &br),
            vec![
                row(Some(0), Some(0), Some(0)),
                row(Some(1), None, None),
                row(None, None, Some(1)),
                row(Some(2), Some(1), Some(2)),
            ]
        );
    }

    #[test]
    fn an_empty_side_yields_gaps_throughout() {
        let br: Vec<(Option<u32>, Option<u32>)> = (0..3).map(|i| (Some(i), None)).collect();
        let rows = align3(&identity(3), &br);
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| r.right.is_none() && r.base.is_some()));
    }

    #[test]
    fn both_inputs_empty_yields_no_rows() {
        assert!(align3(&[], &[]).is_empty());
    }

    #[test]
    fn one_input_empty_drains_the_other() {
        assert_eq!(
            align3(&identity(2), &[]),
            vec![row(Some(0), Some(0), None), row(Some(1), Some(1), None)]
        );
        assert_eq!(
            align3(&[], &identity(2)),
            vec![row(None, Some(0), Some(0)), row(None, Some(1), Some(1))]
        );
    }

    #[test]
    fn every_base_line_appears_once_in_increasing_order() {
        let bl = vec![
            (Some(0), Some(0)),
            (None, Some(1)),
            (Some(1), Some(2)),
            (Some(2), None),
            (Some(3), Some(3)),
        ];
        let br = vec![
            (Some(0), Some(0)),
            (Some(1), None),
            (None, Some(1)),
            (Some(2), Some(2)),
            (Some(3), Some(3)),
        ];
        let bases: Vec<u32> = align3(&bl, &br).iter().filter_map(|r| r.base).collect();
        assert_eq!(bases, vec![0, 1, 2, 3]);
    }

    #[test]
    fn a_base_line_missing_from_one_input_neither_loops_nor_drops_rows() {
        // base line 1 is absent from `base_right`, which difftastic should never
        // do — the join must still terminate and keep every line it was given.
        let bl = identity(3);
        let br = vec![(Some(0), Some(0)), (Some(2), Some(1))];
        let rows = align3(&bl, &br);
        assert_eq!(
            rows,
            vec![
                row(Some(0), Some(0), Some(0)),
                row(Some(1), Some(1), None),
                row(Some(2), Some(2), Some(1)),
            ]
        );
    }

    /// The real alignments difftastic reports for `tests/fixtures/multi`, where
    /// left and right replace the same region with 4 and 1 lines respectively.
    #[test]
    fn multi_line_conflict_with_differing_side_lengths() {
        let base_left = vec![
            (Some(0), Some(0)),
            (Some(1), Some(1)),
            (Some(2), Some(2)),
            (Some(3), Some(3)),
            (None, Some(4)),
            (Some(4), Some(5)),
            (Some(5), Some(6)),
        ];
        let base_right = vec![
            (Some(0), Some(0)),
            (Some(1), Some(1)),
            (Some(2), None),
            (Some(3), None),
            (Some(4), Some(2)),
            (Some(5), Some(3)),
        ];

        assert_eq!(
            align3(&base_left, &base_right),
            vec![
                row(Some(0), Some(0), Some(0)),
                // three genuinely different lines paired as one change
                row(Some(1), Some(1), Some(1)),
                // a two-row gap in the right column
                row(Some(2), Some(2), None),
                row(Some(3), Some(3), None),
                // a left-only insertion: gaps in *both* other panes
                row(Some(4), None, None),
                row(Some(5), Some(4), Some(2)),
                row(Some(6), Some(5), Some(3)),
            ]
        );
    }
}
