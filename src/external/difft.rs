//! Bridge to the `difft` binary for structural, intra-line diffs.
//!
//! Difftastic publishes no library target, so it is driven as a subprocess.
//! Its JSON output is gated behind `DFT_UNSTABLE=yes` and reports novel token
//! positions as **byte offsets** on **0-based** line numbers.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::render::span::coalesce;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    #[default]
    Unchanged,
    Changed,
    Created,
    Deleted,
}

/// Difftastic omits `aligned_lines` and `chunks` entirely when a file is
/// unchanged, so every field past the always-present three needs a default.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct DiffFile {
    #[serde(default)]
    pub aligned_lines: Vec<(Option<u32>, Option<u32>)>,
    #[serde(default)]
    pub chunks: Vec<Vec<DiffLine>>,
    #[serde(default)]
    pub language: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub status: Status,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DiffLine {
    #[serde(default)]
    pub lhs: Option<DiffSide>,
    #[serde(default)]
    pub rhs: Option<DiffSide>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DiffSide {
    pub line_number: u32,
    #[serde(default)]
    pub changes: Vec<DiffChange>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct DiffChange {
    /// Byte offset of the change within its line.
    pub start: u32,
    /// Byte offset one past the end of the change.
    pub end: u32,
    #[serde(default)]
    pub content: String,
    /// Difftastic's own token classification. Unused: foregrounds come from
    /// syntect, so difftastic only tells us *where* a change is.
    #[serde(default)]
    pub highlight: String,
}

/// Coalesced novel byte ranges, keyed by 0-based line number.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SideRanges(pub HashMap<u32, Vec<Range<usize>>>);

impl SideRanges {
    pub fn get(&self, line: u32) -> &[Range<usize>] {
        self.0.get(&line).map_or(&[], Vec::as_slice)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Merge another side's ranges into this one, coalescing per line.
    ///
    /// Used for the base side of a conflict, which needs the union of what
    /// left and right each changed relative to it.
    pub fn union(mut self, other: &Self) -> Self {
        for (line, ranges) in &other.0 {
            let entry = self.0.entry(*line).or_default();
            entry.extend(ranges.iter().cloned());
            *entry = coalesce(entry);
        }
        self
    }
}

/// Extract per-line novel ranges for both sides of a difftastic result.
pub fn novel_ranges(file: &DiffFile) -> (SideRanges, SideRanges) {
    let mut lhs: HashMap<u32, Vec<Range<usize>>> = HashMap::new();
    let mut rhs: HashMap<u32, Vec<Range<usize>>> = HashMap::new();

    for line in file.chunks.iter().flatten() {
        for (side, out) in [(&line.lhs, &mut lhs), (&line.rhs, &mut rhs)] {
            let Some(side) = side else { continue };
            let entry = out.entry(side.line_number).or_default();
            entry.extend(
                side.changes
                    .iter()
                    .map(|c| c.start as usize..c.end as usize),
            );
        }
    }

    let finish = |m: HashMap<u32, Vec<Range<usize>>>| {
        SideRanges(
            m.into_iter()
                .map(|(k, v)| (k, coalesce(&v)))
                .filter(|(_, v)| !v.is_empty())
                .collect(),
        )
    };
    (finish(lhs), finish(rhs))
}

/// What one structural diff tells us about a pair of texts.
#[derive(Clone, Debug, Default)]
pub struct DiffResult {
    /// Whole-file row alignment, `(lhs line, rhs line)`, 0-based. Every line of
    /// both files appears exactly once, in order; `None` marks a pure insertion
    /// or deletion. A *changed* line is paired with its counterpart rather than
    /// split into a delete plus an insert.
    pub aligned: Vec<(Option<u32>, Option<u32>)>,
    pub lhs: SideRanges,
    pub rhs: SideRanges,
}

/// Produces the alignment and per-line novel ranges for a pair of texts.
///
/// Exists as a trait so the document and pane builders can be tested without
/// `difft` on `PATH`.
pub trait Differ {
    fn diff(&self, lhs: &str, rhs: &str) -> DiffResult;
}

/// Difftastic counts lines by splitting on `\n`, so a file ending in a newline
/// has a final empty line. Matching that keeps our line numbers aligned with
/// the ones it reports.
pub fn line_count(text: &str) -> usize {
    text.split('\n').count()
}

/// Difftastic omits `aligned_lines` when a side is empty or the files match, so
/// those cases get an alignment built from the line counts instead.
fn alignment_or_synthesized(
    file: &DiffFile,
    lhs: &str,
    rhs: &str,
) -> Vec<(Option<u32>, Option<u32>)> {
    if !file.aligned_lines.is_empty() {
        return file.aligned_lines.clone();
    }
    match file.status {
        Status::Created => (0..line_count(rhs) as u32)
            .map(|i| (None, Some(i)))
            .collect(),
        Status::Deleted => (0..line_count(lhs) as u32)
            .map(|i| (Some(i), None))
            .collect(),
        // Unchanged, or a `changed` result with nothing to align.
        Status::Unchanged | Status::Changed => (0..line_count(lhs) as u32)
            .map(|i| (Some(i), Some(i)))
            .collect(),
    }
}

/// A naive index-for-index alignment, padding the shorter side with `None`.
///
/// Used when `difft` cannot be run at all, so the side-by-side view degrades to
/// a plain line-by-line comparison rather than going blank.
fn positional_alignment(lhs: &str, rhs: &str) -> Vec<(Option<u32>, Option<u32>)> {
    let (n, m) = (line_count(lhs) as u32, line_count(rhs) as u32);
    (0..n.max(m))
        .map(|i| ((i < n).then_some(i), (i < m).then_some(i)))
        .collect()
}

/// Drives the real `difft` binary over a pair of temporary files.
pub struct DifftCli {
    /// Passed to `--override='*:<lang>'` so difftastic doesn't have to guess a
    /// language from the temp file names.
    pub language: Option<String>,
    /// The compared file's own name, given to the temp files so difftastic can
    /// detect from it. A name is used rather than just an extension because
    /// difftastic matches whole names too — `Makefile` has no extension to
    /// match on, and `CMakeLists.txt`'s would be misleading.
    pub file_name: Option<String>,
}

impl DifftCli {
    pub fn new(language: Option<String>, file_name: Option<String>) -> Self {
        Self {
            language,
            file_name,
        }
    }

    /// Like [`Differ::diff`] but surfaces the failure instead of degrading.
    pub fn try_diff(&self, lhs: &str, rhs: &str) -> Result<DiffResult> {
        let file = self.run(lhs, rhs)?;
        let (lhs_ranges, rhs_ranges) = novel_ranges(&file);
        Ok(DiffResult {
            aligned: alignment_or_synthesized(&file, lhs, rhs),
            lhs: lhs_ranges,
            rhs: rhs_ranges,
        })
    }

    fn run(&self, lhs: &str, rhs: &str) -> Result<DiffFile> {
        let dir = tempfile::tempdir().context("creating temp dir for difft")?;
        // Both sides keep the real file name, in their own subdirectory, so
        // difftastic sees the name it would have seen on disk.
        let name = self.file_name.as_deref().unwrap_or("file.txt");
        let write = |side: &str, text: &str| -> Result<std::path::PathBuf> {
            let dir = dir.path().join(side);
            std::fs::create_dir_all(&dir)?;
            let path = dir.join(name);
            std::fs::write(&path, text)?;
            Ok(path)
        };
        let lhs_path = write("lhs", lhs)?;
        let rhs_path = write("rhs", rhs)?;

        let mut cmd = Command::new("difft");
        cmd.env("DFT_UNSTABLE", "yes").arg("--display").arg("json");
        if let Some(lang) = &self.language {
            cmd.arg(format!("--override=*:{lang}"));
        }
        cmd.arg(&lhs_path).arg(&rhs_path);

        let out = cmd
            .output()
            .context("running `difft` (is difftastic installed and on PATH?)")?;
        // difft exits 0 whether or not the files differ, so a non-zero status
        // is a genuine failure.
        if !out.status.success() {
            bail!(
                "difft exited with {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        serde_json::from_slice(&out.stdout).context("parsing difft JSON output")
    }
}

impl Differ for DifftCli {
    fn diff(&self, lhs: &str, rhs: &str) -> DiffResult {
        // A missing or misbehaving difft costs inline emphasis, not the view:
        // conflict blocks still render with their block backgrounds, and the
        // panes fall back to a line-for-line alignment.
        self.try_diff(lhs, rhs).unwrap_or_else(|_| DiffResult {
            aligned: positional_alignment(lhs, rhs),
            ..DiffResult::default()
        })
    }
}

/// Memoises another [`Differ`] by text pair.
///
/// Both documents are rebuilt whenever a resolution changes, and building the
/// merged panel runs the differ once per conflict — without this, every
/// keypress would spawn a `difft` process per conflict.
pub struct CachingDiffer<D: Differ> {
    inner: D,
    cache: RefCell<HashMap<(String, String), DiffResult>>,
}

impl<D: Differ> CachingDiffer<D> {
    pub fn new(inner: D) -> Self {
        Self {
            inner,
            cache: RefCell::new(HashMap::new()),
        }
    }
}

impl<D: Differ> Differ for CachingDiffer<D> {
    fn diff(&self, lhs: &str, rhs: &str) -> DiffResult {
        let key = (lhs.to_owned(), rhs.to_owned());
        if let Some(hit) = self.cache.borrow().get(&key) {
            return hit.clone();
        }
        let result = self.inner.diff(lhs, rhs);
        self.cache.borrow_mut().insert(key, result.clone());
        result
    }
}

/// A [`Differ`] that returns canned results, for tests.
#[derive(Default)]
pub struct StaticDiffer {
    pub aligned: Vec<(Option<u32>, Option<u32>)>,
    pub lhs: SideRanges,
    pub rhs: SideRanges,
}

impl Differ for StaticDiffer {
    fn diff(&self, lhs: &str, rhs: &str) -> DiffResult {
        DiffResult {
            // An unset alignment means "line for line", which is what most
            // tests want without having to spell it out.
            aligned: if self.aligned.is_empty() {
                positional_alignment(lhs, rhs)
            } else {
                self.aligned.clone()
            },
            lhs: self.lhs.clone(),
            rhs: self.rhs.clone(),
        }
    }
}

#[cfg(test)]
// These compare against one-element slices of ranges, which clippy mistakes
// for an attempt to build a Vec *from* a range.
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;

    /// Captured from `DFT_UNSTABLE=yes difft --display json` on a Rust file
    /// containing Korean and Chinese text.
    const CHANGED_JSON: &str = include_str!("../../tests/fixtures/difft_changed.json");
    const UNCHANGED_JSON: &str = include_str!("../../tests/fixtures/difft_unchanged.json");

    #[test]
    fn deserializes_unchanged_output_without_optional_keys() {
        let file: DiffFile = serde_json::from_str(UNCHANGED_JSON).unwrap();
        assert_eq!(file.status, Status::Unchanged);
        assert_eq!(file.language, "Rust");
        assert!(file.chunks.is_empty());
        assert!(file.aligned_lines.is_empty());
    }

    #[test]
    fn deserializes_changed_output() {
        let file: DiffFile = serde_json::from_str(CHANGED_JSON).unwrap();
        assert_eq!(file.status, Status::Changed);
        assert_eq!(file.aligned_lines.len(), 6);
        assert_eq!(file.chunks.len(), 1);
        assert_eq!(file.chunks[0].len(), 2);
    }

    #[test]
    fn novel_ranges_coalesces_per_token_changes() {
        let file: DiffFile = serde_json::from_str(CHANGED_JSON).unwrap();
        let (lhs, rhs) = novel_ranges(&file);

        // lhs line 1 arrives as 17..18, 18..33, 33..34, 34..40, 40..41
        assert_eq!(lhs.get(1), &[17..41]);
        // rhs line 1 arrives as seven touching tokens
        assert_eq!(rhs.get(1), &[17..34]);
        // the `1` -> `2` change on line 3
        assert_eq!(lhs.get(3), &[12..13]);
        assert_eq!(rhs.get(3), &[12..13]);
        // unchanged lines carry no ranges
        assert_eq!(lhs.get(0), &[] as &[Range<usize>]);
        assert_eq!(lhs.get(999), &[] as &[Range<usize>]);
    }

    #[test]
    fn union_merges_ranges_per_line() {
        let a = SideRanges(HashMap::from([(1, vec![2..5]), (2, vec![0..1])]));
        let b = SideRanges(HashMap::from([(1, vec![4..9]), (3, vec![7..8])]));
        let u = a.union(&b);
        assert_eq!(u.get(1), &[2..9]);
        assert_eq!(u.get(2), &[0..1]);
        assert_eq!(u.get(3), &[7..8]);
    }

    #[test]
    fn line_count_matches_difftastics_split_on_newline() {
        // a trailing newline leaves a final empty line, which difftastic numbers
        assert_eq!(line_count("a\nb\n"), 3);
        assert_eq!(line_count("a\nb"), 2);
        assert_eq!(line_count(""), 1);
    }

    #[test]
    fn alignment_is_taken_from_the_json_when_present() {
        let file: DiffFile = serde_json::from_str(CHANGED_JSON).unwrap();
        let aligned = alignment_or_synthesized(&file, "", "");
        assert_eq!(aligned.len(), 6);
        assert_eq!(aligned[0], (Some(0), Some(0)));
    }

    #[test]
    fn unchanged_output_synthesizes_an_identity_alignment() {
        let file: DiffFile = serde_json::from_str(UNCHANGED_JSON).unwrap();
        let text = "a\nb\nc\n";
        assert_eq!(
            alignment_or_synthesized(&file, text, text),
            vec![
                (Some(0), Some(0)),
                (Some(1), Some(1)),
                (Some(2), Some(2)),
                (Some(3), Some(3)),
            ]
        );
    }

    #[test]
    fn an_empty_side_synthesizes_a_one_sided_alignment() {
        let created = DiffFile {
            status: Status::Created,
            ..DiffFile::default()
        };
        assert_eq!(
            alignment_or_synthesized(&created, "", "a\nb"),
            vec![(None, Some(0)), (None, Some(1))]
        );
        let deleted = DiffFile {
            status: Status::Deleted,
            ..DiffFile::default()
        };
        assert_eq!(
            alignment_or_synthesized(&deleted, "a\nb", ""),
            vec![(Some(0), None), (Some(1), None)]
        );
    }

    #[test]
    fn positional_alignment_pads_the_shorter_side() {
        assert_eq!(
            positional_alignment("a\nb\nc", "a\nb"),
            vec![(Some(0), Some(0)), (Some(1), Some(1)), (Some(2), None)]
        );
        assert_eq!(
            positional_alignment("a", "a\nb"),
            vec![(Some(0), Some(0)), (None, Some(1))]
        );
    }

    #[test]
    fn a_static_differ_without_an_alignment_falls_back_to_line_for_line() {
        let result = StaticDiffer::default().diff("a\nb", "a\nb");
        assert_eq!(result.aligned, vec![(Some(0), Some(0)), (Some(1), Some(1))]);
    }

    #[test]
    fn the_caching_differ_calls_through_once_per_distinct_pair() {
        use std::cell::Cell;

        struct Counting(Cell<usize>);
        impl Differ for Counting {
            fn diff(&self, _lhs: &str, _rhs: &str) -> DiffResult {
                self.0.set(self.0.get() + 1);
                DiffResult::default()
            }
        }

        let caching = CachingDiffer::new(Counting(Cell::new(0)));
        caching.diff("a", "b");
        caching.diff("a", "b");
        caching.diff("a", "b");
        assert_eq!(
            caching.inner.0.get(),
            1,
            "repeats should come from the cache"
        );

        caching.diff("a", "c");
        assert_eq!(caching.inner.0.get(), 2, "a new pair goes through");
    }

    #[test]
    fn empty_diff_file_yields_empty_ranges() {
        let (lhs, rhs) = novel_ranges(&DiffFile::default());
        assert!(lhs.is_empty() && rhs.is_empty());
    }
}
