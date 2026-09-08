//! Bridge to the `git` binary, for discovering and staging conflicted files.
//!
//! Driven as a subprocess like `difft` and `mergiraf`, rather than linked.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// Index stages of an unmerged path, in bigyo's own order.
///
/// Git numbers them 1 = the common ancestor, 2 = ours, 3 = theirs; those are
/// exactly base, left and right.
pub const BASE: usize = 0;
pub const LEFT: usize = 1;
pub const RIGHT: usize = 2;

/// One unmerged path and the object id of each stage it has.
///
/// A stage is absent for an add/add conflict (no base) or a delete/modify one
/// (no ours, or no theirs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnmergedEntry {
    pub path: PathBuf,
    pub stages: [Option<String>; 3],
}

pub struct Repo {
    root: PathBuf,
}

impl Repo {
    /// Locate the repository containing `cwd`, or `None` if there is not one.
    pub fn discover(cwd: &Path) -> Result<Option<Self>> {
        let out = Command::new("git")
            .current_dir(cwd)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .context("running `git` (is it installed and on PATH?)")?;
        if !out.status.success() {
            return Ok(None);
        }
        let root = String::from_utf8(out.stdout)
            .context("git printed a non-UTF-8 repository path")?
            .trim_end()
            .to_owned();
        if root.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self {
            root: PathBuf::from(root),
        }))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Every unmerged path, optionally narrowed to a pathspec.
    pub fn unmerged(&self, pathspec: Option<&Path>) -> Result<Vec<UnmergedEntry>> {
        let mut cmd = Command::new("git");
        cmd.current_dir(&self.root).args(["ls-files", "-u", "-z"]);
        if let Some(spec) = pathspec {
            cmd.arg("--").arg(spec);
        }
        let out = cmd.output().context("running `git ls-files`")?;
        if !out.status.success() {
            bail!(
                "git ls-files failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        parse_unmerged(&out.stdout)
    }

    /// Read one stage's content by object id.
    ///
    /// Going through the object id rather than `git show :1:<path>` sidesteps
    /// pathspec quoting entirely.
    pub fn blob(&self, sha: &str) -> Result<Vec<u8>> {
        let out = Command::new("git")
            .current_dir(&self.root)
            .args(["cat-file", "blob", sha])
            .output()
            .context("running `git cat-file`")?;
        if !out.status.success() {
            bail!(
                "git cat-file {sha} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(out.stdout)
    }

    /// Stage `path`, which is what marks a conflict resolved for git.
    pub fn add(&self, path: &Path) -> Result<()> {
        let out = Command::new("git")
            .current_dir(&self.root)
            .arg("add")
            .arg("--")
            .arg(path)
            .output()
            .context("running `git add`")?;
        if !out.status.success() {
            bail!(
                "git add {} failed: {}",
                path.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }
}

/// Parse `git ls-files -u -z` output, grouping the stages of each path.
///
/// Each NUL-terminated record is `<mode> <sha> <stage>\t<path>`. Paths come out
/// verbatim under `-z`, so no unquoting is needed — which is the whole reason
/// for using it.
pub fn parse_unmerged(bytes: &[u8]) -> Result<Vec<UnmergedEntry>> {
    let mut entries: Vec<UnmergedEntry> = Vec::new();

    for record in bytes.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        let record =
            std::str::from_utf8(record).context("git reported a path that is not valid UTF-8")?;
        let (meta, path) = record
            .split_once('\t')
            .with_context(|| format!("no tab in ls-files record {record:?}"))?;

        let mut fields = meta.split_whitespace();
        let (_mode, sha, stage) = (fields.next(), fields.next(), fields.next());
        let (Some(sha), Some(stage)) = (sha, stage) else {
            bail!("malformed ls-files record {record:?}");
        };
        let stage: usize = stage
            .parse()
            .with_context(|| format!("unreadable stage in {record:?}"))?;
        if !(1..=3).contains(&stage) {
            bail!("stage {stage} out of range in {record:?}");
        }

        let path = PathBuf::from(path);
        // Records for one path arrive together and in stage order, but grouping
        // by path rather than assuming that costs nothing.
        let entry = match entries.iter_mut().find(|e| e.path == path) {
            Some(existing) => existing,
            None => {
                entries.push(UnmergedEntry {
                    path,
                    stages: [None, None, None],
                });
                entries.last_mut().expect("just pushed")
            }
        };
        entry.stages[stage - 1] = Some(sha.to_owned());
    }

    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build `ls-files -u -z` output from `(stage, path)` pairs.
    fn output(records: &[(u8, &str)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (stage, path) in records {
            out.extend_from_slice(
                format!("100644 {:040x} {stage}\t{path}\0", *stage as u32).as_bytes(),
            );
        }
        out
    }

    #[test]
    fn three_stages_of_one_path_become_one_entry() {
        let entries = parse_unmerged(&output(&[
            (1, "src/main.rs"),
            (2, "src/main.rs"),
            (3, "src/main.rs"),
        ]))
        .unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, PathBuf::from("src/main.rs"));
        assert!(entries[0].stages.iter().all(Option::is_some));
        // stage 1 is the common ancestor, i.e. bigyo's base
        assert_ne!(entries[0].stages[BASE], entries[0].stages[LEFT]);
        assert_ne!(entries[0].stages[LEFT], entries[0].stages[RIGHT]);
    }

    #[test]
    fn an_add_add_conflict_has_no_base_stage() {
        let entries = parse_unmerged(&output(&[(2, "new.rs"), (3, "new.rs")])).unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].stages[BASE].is_none());
        assert!(entries[0].stages[LEFT].is_some());
        assert!(entries[0].stages[RIGHT].is_some());
    }

    #[test]
    fn a_delete_modify_conflict_is_missing_the_deleted_side() {
        let entries = parse_unmerged(&output(&[(1, "gone.rs"), (3, "gone.rs")])).unwrap();
        assert!(entries[0].stages[BASE].is_some());
        assert!(entries[0].stages[LEFT].is_none(), "ours deleted it");
        assert!(entries[0].stages[RIGHT].is_some());
    }

    #[test]
    fn several_paths_keep_their_order_and_their_own_stages() {
        let entries = parse_unmerged(&output(&[
            (1, "a.rs"),
            (2, "a.rs"),
            (3, "a.rs"),
            (2, "b.rs"),
            (3, "b.rs"),
        ]))
        .unwrap();

        assert_eq!(
            entries.iter().map(|e| e.path.clone()).collect::<Vec<_>>(),
            [PathBuf::from("a.rs"), PathBuf::from("b.rs")]
        );
        assert!(entries[0].stages[BASE].is_some());
        assert!(entries[1].stages[BASE].is_none());
    }

    #[test]
    fn interleaved_records_still_group_by_path() {
        let entries = parse_unmerged(&output(&[
            (1, "a.rs"),
            (1, "b.rs"),
            (2, "a.rs"),
            (2, "b.rs"),
        ]))
        .unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].stages[BASE].is_some() && entries[0].stages[LEFT].is_some());
        assert!(entries[1].stages[BASE].is_some() && entries[1].stages[LEFT].is_some());
    }

    #[test]
    fn paths_with_spaces_and_non_ascii_survive_verbatim() {
        // `-z` is what makes this safe: no quoting to undo
        let entries = parse_unmerged(&output(&[
            (2, "dir with spaces/my file.rs"),
            (2, "테스트/파일.rs"),
        ]))
        .unwrap();
        assert_eq!(
            entries.iter().map(|e| e.path.clone()).collect::<Vec<_>>(),
            [
                PathBuf::from("dir with spaces/my file.rs"),
                PathBuf::from("테스트/파일.rs"),
            ]
        );
    }

    /// Captured verbatim from `git ls-files -u -z` on a repo whose merge
    /// produced a content conflict, an add/add, a modify/delete, and paths with
    /// a space and with Hangul in them.
    const REAL: &[u8] = include_bytes!("../../tests/fixtures/git_ls_files_unmerged.bin");

    #[test]
    fn parses_real_git_output() {
        let entries = parse_unmerged(REAL).unwrap();
        let paths: Vec<String> = entries
            .iter()
            .map(|e| e.path.display().to_string())
            .collect();
        assert_eq!(
            paths,
            [
                "added.rs",
                "dir with spaces/my file.rs",
                "src/a.rs",
                "테스트.rs",
            ]
        );

        let stages = |path: &str| {
            entries
                .iter()
                .find(|e| e.path.to_str() == Some(path))
                .unwrap()
                .stages
                .clone()
                .map(|s| s.is_some())
        };
        // add/add: both sides created it, so there is no common ancestor
        assert_eq!(stages("added.rs"), [false, true, true]);
        // an ordinary content conflict has all three
        assert_eq!(stages("src/a.rs"), [true, true, true]);
        // modify/delete: ours deleted it, theirs changed it
        assert_eq!(stages("테스트.rs"), [true, false, true]);
        assert_eq!(stages("dir with spaces/my file.rs"), [true, true, true]);
    }

    #[test]
    fn real_object_ids_are_full_length_hex() {
        for entry in parse_unmerged(REAL).unwrap() {
            for sha in entry.stages.iter().flatten() {
                assert_eq!(sha.len(), 40, "{sha:?}");
                assert!(sha.chars().all(|c| c.is_ascii_hexdigit()), "{sha:?}");
            }
        }
    }

    #[test]
    fn empty_output_yields_no_entries() {
        assert!(parse_unmerged(b"").unwrap().is_empty());
        assert!(parse_unmerged(b"\0").unwrap().is_empty());
    }

    #[test]
    fn malformed_records_are_errors_rather_than_panics() {
        // no tab
        assert!(parse_unmerged(b"100644 abc 1 src/main.rs\0").is_err());
        // no stage field
        assert!(parse_unmerged(b"100644 abc\tsrc/main.rs\0").is_err());
        // unparseable stage
        assert!(parse_unmerged(b"100644 abc x\tsrc/main.rs\0").is_err());
        // stage out of range
        assert!(parse_unmerged(b"100644 abc 4\tsrc/main.rs\0").is_err());
    }
}
