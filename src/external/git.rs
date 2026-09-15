//! Bridge to the `git` binary, for discovering and staging conflicted files.
//!
//! Driven as a subprocess like `difft` and `mergiraf`, rather than linked.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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

    /// What `git diff <rev>` reports against the working tree.
    pub fn changed(&self, rev: &str, pathspec: Option<&Path>) -> Result<Vec<ChangedEntry>> {
        let mut cmd = Command::new("git");
        cmd.current_dir(&self.root)
            .args(["diff", "--name-status", "-z", rev]);
        if let Some(spec) = pathspec {
            cmd.arg("--").arg(spec);
        }
        let out = cmd.output().context("running `git diff --name-status`")?;
        if !out.status.success() {
            bail!(
                "git diff failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        parse_name_status(&out.stdout)
    }

    /// `git show <rev>:<path>`, or `Ok(None)` when that revision has no such
    /// path — which is how an added file's pre-image reads.
    ///
    /// The path goes straight into argv, so there is nothing to quote.
    pub fn show(&self, rev: &str, path: &Path) -> Result<Option<Vec<u8>>> {
        let mut spec = std::ffi::OsString::from(rev);
        spec.push(":");
        spec.push(path.as_os_str());

        let out = Command::new("git")
            .current_dir(&self.root)
            .arg("show")
            .arg(&spec)
            .output()
            .context("running `git show`")?;
        Ok(out.status.success().then_some(out.stdout))
    }

    /// The `linguist-language` gitattribute of each path that sets one.
    ///
    /// Batched through `--stdin`: a whole workspace costs one subprocess rather
    /// than one per file, which matters because the result is read once at
    /// construction and then never again.
    pub fn linguist_languages(&self, paths: &[PathBuf]) -> Result<HashMap<PathBuf, String>> {
        if paths.is_empty() {
            return Ok(HashMap::new());
        }

        let mut input = Vec::new();
        for path in paths {
            input.extend_from_slice(path.as_os_str().as_encoded_bytes());
            input.push(0);
        }

        let mut child = Command::new("git")
            .current_dir(&self.root)
            .args(["check-attr", "--stdin", "-z", LINGUIST_LANGUAGE])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("running `git check-attr`")?;
        child
            .stdin
            .take()
            .context("git check-attr took no stdin")?
            .write_all(&input)
            .context("writing paths to git check-attr")?;
        let out = child
            .wait_with_output()
            .context("running `git check-attr`")?;
        if !out.status.success() {
            // Outside a repository, or any other failure: no attributes, which
            // is not an error — detection simply falls back to the extension.
            return Ok(HashMap::new());
        }

        Ok(parse_check_attr(&out.stdout)?
            .into_iter()
            .filter(|(_, attr, _)| attr == LINGUIST_LANGUAGE)
            .filter_map(|(path, _, value)| attribute_value(&value).map(|v| (path, v.to_owned())))
            .collect())
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

pub const LINGUIST_LANGUAGE: &str = "linguist-language";

/// Parse `git check-attr -z` output: `<path>\0<attribute>\0<value>\0` records.
pub fn parse_check_attr(bytes: &[u8]) -> Result<Vec<(PathBuf, String, String)>> {
    let mut fields = bytes.split(|&b| b == 0);
    let mut out = Vec::new();

    loop {
        // Trailing NUL leaves an empty final field; that is the end, not a record.
        let Some(path) = fields.next().filter(|f| !f.is_empty()) else {
            return Ok(out);
        };
        let (Some(attr), Some(value)) = (fields.next(), fields.next()) else {
            bail!("truncated check-attr record");
        };
        let text = |f: &[u8]| -> Result<String> {
            Ok(std::str::from_utf8(f)
                .context("git check-attr produced a field that is not valid UTF-8")?
                .to_owned())
        };
        out.push((PathBuf::from(text(path)?), text(attr)?, text(value)?));
    }
}

/// The value of a gitattribute, or `None` when it carries no useful one.
///
/// `unspecified` means the attribute is not set at all; `set` and `unset` are
/// the boolean forms, which say nothing about which language a file is.
pub fn attribute_value(value: &str) -> Option<&str> {
    match value {
        "" | "unspecified" | "unset" | "set" => None,
        other => Some(other),
    }
}

/// One path `git diff` reports as changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedEntry {
    /// The post-image path — the destination for a rename.
    pub path: PathBuf,
    /// The pre-image path, when a rename or copy moved it.
    pub old_path: Option<PathBuf>,
    /// `A`, `M`, `D`, `R`, `C`, `T` …
    pub status: char,
}

/// Parse `git diff --name-status -z` output.
///
/// Records are `<status>\0<path>\0`, except renames and copies, which are
/// `<status>\0<src>\0<dst>\0` — so the parser has to consume a second field
/// for those rather than assuming one path each.
pub fn parse_name_status(bytes: &[u8]) -> Result<Vec<ChangedEntry>> {
    let mut fields = bytes
        .split(|&b| b == 0)
        .filter(|f| !f.is_empty())
        .map(|f| std::str::from_utf8(f).context("git reported a field that is not valid UTF-8"));

    let mut out = Vec::new();
    while let Some(status) = fields.next().transpose()? {
        let letter = status
            .chars()
            .next()
            .with_context(|| format!("empty status field in {status:?}"))?;
        if !letter.is_ascii_alphabetic() {
            bail!("unreadable status {status:?}");
        }

        let first = fields
            .next()
            .transpose()?
            .with_context(|| format!("status {status:?} with no path"))?;

        // A rename or copy names where it came from as well as where it went.
        let (old_path, path) = if matches!(letter, 'R' | 'C') {
            let second = fields
                .next()
                .transpose()?
                .with_context(|| format!("status {status:?} with only one path"))?;
            (Some(PathBuf::from(first)), PathBuf::from(second))
        } else {
            (None, PathBuf::from(first))
        };

        out.push(ChangedEntry {
            path,
            old_path,
            status: letter,
        });
    }

    Ok(out)
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

    // ---- git check-attr -z -----------------------------------------------

    /// Build `check-attr -z` output from `(path, value)` pairs.
    fn check_attr(records: &[(&str, &str)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (path, value) in records {
            for field in [*path, LINGUIST_LANGUAGE, *value] {
                out.extend_from_slice(field.as_bytes());
                out.push(0);
            }
        }
        out
    }

    #[test]
    fn a_set_attribute_carries_its_language() {
        let parsed = parse_check_attr(&check_attr(&[("a.weird", "Rust")])).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].0, PathBuf::from("a.weird"));
        assert_eq!(parsed[0].1, LINGUIST_LANGUAGE);
        assert_eq!(attribute_value(&parsed[0].2), Some("Rust"));
    }

    #[test]
    fn the_boolean_and_absent_forms_name_no_language() {
        // `unspecified` is "not set"; `set`/`unset` are the boolean forms, which
        // say nothing about which language a file is.
        for value in ["unspecified", "set", "unset", ""] {
            let parsed = parse_check_attr(&check_attr(&[("a.rs", value)])).unwrap();
            assert_eq!(attribute_value(&parsed[0].2), None, "{value}");
        }
    }

    #[test]
    fn several_paths_come_back_in_order() {
        let parsed = parse_check_attr(&check_attr(&[
            ("a.weird", "Rust"),
            ("b.txt", "unspecified"),
            ("c.tmpl", "HTML"),
        ]))
        .unwrap();
        assert_eq!(
            parsed.iter().map(|r| r.0.clone()).collect::<Vec<_>>(),
            [
                PathBuf::from("a.weird"),
                PathBuf::from("b.txt"),
                PathBuf::from("c.tmpl"),
            ]
        );
        assert_eq!(attribute_value(&parsed[2].2), Some("HTML"));
    }

    #[test]
    fn attribute_paths_with_spaces_and_non_ascii_survive_verbatim() {
        let parsed = parse_check_attr(&check_attr(&[
            ("dir with spaces/my file.weird", "Rust"),
            ("테스트/파일.weird", "Python"),
        ]))
        .unwrap();
        assert_eq!(parsed[0].0, PathBuf::from("dir with spaces/my file.weird"));
        assert_eq!(parsed[1].0, PathBuf::from("테스트/파일.weird"));
        assert_eq!(attribute_value(&parsed[1].2), Some("Python"));
    }

    #[test]
    fn empty_check_attr_output_yields_no_records() {
        assert!(parse_check_attr(b"").unwrap().is_empty());
        assert!(parse_check_attr(b"\0").unwrap().is_empty());
    }

    #[test]
    fn a_truncated_check_attr_record_is_an_error() {
        // a path with no attribute or value after it
        assert!(parse_check_attr(b"a.rs\0").is_err());
        assert!(parse_check_attr(b"a.rs\0linguist-language").is_err());
    }

    #[test]
    fn an_empty_attribute_value_names_no_language() {
        // the trailing NUL leaves an empty value field rather than a short record
        let parsed = parse_check_attr(b"a.rs\0linguist-language\0\0").unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(attribute_value(&parsed[0].2), None);
    }

    // ---- git diff --name-status -z ---------------------------------------

    /// Build `--name-status -z` output from `(status, paths)` records.
    fn name_status(records: &[(&str, &[&str])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (status, paths) in records {
            out.extend_from_slice(status.as_bytes());
            out.push(0);
            for path in *paths {
                out.extend_from_slice(path.as_bytes());
                out.push(0);
            }
        }
        out
    }

    #[test]
    fn one_path_per_ordinary_status() {
        let entries = parse_name_status(&name_status(&[
            ("M", &["src/a.rs"]),
            ("A", &["new.rs"]),
            ("D", &["gone.rs"]),
        ]))
        .unwrap();

        assert_eq!(
            entries.iter().map(|e| e.status).collect::<Vec<_>>(),
            ['M', 'A', 'D']
        );
        assert_eq!(entries[0].path, PathBuf::from("src/a.rs"));
        assert!(entries.iter().all(|e| e.old_path.is_none()));
    }

    #[test]
    fn a_rename_consumes_two_paths() {
        // The record that would desynchronise a parser assuming one path each.
        let entries = parse_name_status(&name_status(&[
            ("R100", &["old/name.rs", "new/name.rs"]),
            ("M", &["after.rs"]),
        ]))
        .unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].status, 'R');
        assert_eq!(entries[0].old_path, Some(PathBuf::from("old/name.rs")));
        assert_eq!(entries[0].path, PathBuf::from("new/name.rs"));
        // and the record after it is still read correctly
        assert_eq!(entries[1].status, 'M');
        assert_eq!(entries[1].path, PathBuf::from("after.rs"));
    }

    #[test]
    fn a_copy_also_carries_its_source() {
        let entries = parse_name_status(&name_status(&[("C75", &["from.rs", "to.rs"])])).unwrap();
        assert_eq!(entries[0].status, 'C');
        assert_eq!(entries[0].old_path, Some(PathBuf::from("from.rs")));
        assert_eq!(entries[0].path, PathBuf::from("to.rs"));
    }

    #[test]
    fn changed_paths_with_spaces_and_non_ascii_survive_verbatim() {
        let entries = parse_name_status(&name_status(&[
            ("M", &["dir with spaces/my file.rs"]),
            ("A", &["테스트/파일.rs"]),
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

    #[test]
    fn empty_name_status_output_yields_no_entries() {
        assert!(parse_name_status(b"").unwrap().is_empty());
        assert!(parse_name_status(b"\0\0").unwrap().is_empty());
    }

    #[test]
    fn malformed_name_status_records_are_errors() {
        // a status with no path at all
        assert!(parse_name_status(b"M\0").is_err());
        // a rename with only one path
        assert!(parse_name_status(b"R100\0only.rs\0").is_err());
        // a status that is not a letter
        assert!(parse_name_status(b"7\0a.rs\0").is_err());
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
