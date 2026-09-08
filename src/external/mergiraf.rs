//! Bridge to the `mergiraf` binary for syntax-aware three-way merging.
//!
//! Mergiraf is invoked as a subprocess rather than linked: it is GPL-3.0 and
//! its own docs say it is not designed to be used as a library.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::merge::diff3::{DEFAULT_MARKER_SIZE, MergedChunk, parse_diff3};

/// Labels written into conflict markers. Fixed values keep absolute paths out
/// of the output the parser sees.
const LEFT_NAME: &str = "LEFT";
const BASE_NAME: &str = "BASE";
const RIGHT_NAME: &str = "RIGHT";

pub struct MergeOutput {
    /// The merged file, conflict markers included.
    pub text: String,
    /// Whether mergiraf left any conflicts behind (its exit code 1).
    pub has_conflicts: bool,
    pub chunks: Vec<MergedChunk>,
}

/// Merge three revisions held in memory, as directory mode has them.
///
/// The temp files are named after `path_name` so mergiraf's own language
/// detection still works on the extension.
pub fn merge_texts(
    base: &str,
    left: &str,
    right: &str,
    language: Option<&str>,
    path_name: &Path,
) -> Result<MergeOutput> {
    let dir = tempfile::tempdir().context("creating temp dir for mergiraf")?;
    let name = path_name
        .file_name()
        .map_or_else(|| std::ffi::OsString::from("file"), |n| n.to_owned());

    let write = |side: &str, text: &str| -> Result<std::path::PathBuf> {
        let path = dir.path().join(side);
        std::fs::create_dir_all(&path)?;
        let path = path.join(&name);
        std::fs::write(&path, text)?;
        Ok(path)
    };
    let base_path = write("base", base)?;
    let left_path = write("left", left)?;
    let right_path = write("right", right)?;

    merge(
        &base_path,
        &left_path,
        &right_path,
        language,
        Some(path_name),
    )
}

/// Run `mergiraf merge <base> <left> <right>` and parse its stdout.
pub fn merge(
    base: &Path,
    left: &Path,
    right: &Path,
    language: Option<&str>,
    path_name: Option<&Path>,
) -> Result<MergeOutput> {
    let mut cmd = Command::new("mergiraf");
    cmd.arg("merge")
        .arg(base)
        .arg(left)
        .arg(right)
        .args(["--base-name", BASE_NAME])
        .args(["--left-name", LEFT_NAME])
        .args(["--right-name", RIGHT_NAME]);
    if let Some(lang) = language {
        cmd.args(["--language", lang]);
    }
    if let Some(path_name) = path_name {
        cmd.arg("--path-name").arg(path_name);
    }

    let out = cmd
        .output()
        .context("running `mergiraf` (is it installed and on PATH?)")?;

    // 0 = clean merge, 1 = conflicts remain; anything else is a real failure.
    let has_conflicts = match out.status.code() {
        Some(0) => false,
        Some(1) => true,
        other => bail!(
            "mergiraf exited with {:?}: {}",
            other,
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    };

    let text = String::from_utf8(out.stdout).context("mergiraf produced non-UTF-8 output")?;
    let chunks = parse_diff3(&text, DEFAULT_MARKER_SIZE);
    Ok(MergeOutput {
        text,
        has_conflicts,
        chunks,
    })
}
