//! The set of conflicted files being resolved in one sitting.
//!
//! In directory mode bigyo is launched once for a whole conflicted merge rather
//! than per file, so it has to hold every file's content and — crucially — its
//! resolutions, which must survive moving to another file and back.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::external::git::{self, Repo, UnmergedEntry};
use crate::merge::session::MergeSession;
use crate::render::panes::SectionNames;

/// Whether an entry is a merge to resolve or a diff to read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum EntryKind {
    #[default]
    Merge,
    /// The pre-image is in `left` and the post-image in `right`; `base` is
    /// unused, which is why LOCAL/REMOTE map onto left/right exactly.
    Diff,
}

/// The only thing the file list says about a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileState {
    /// Some region is still undecided — or the file has not been opened yet, in
    /// which case this is simply what git said about it.
    Conflict,
    /// Every region has been decided.
    Resolved,
    /// A side was not valid UTF-8, so there is nothing to show.
    Binary,
    /// Diff: the post-image exists and the pre-image does not.
    Added,
    /// Diff: both sides exist.
    Modified,
    /// Diff: the pre-image exists and the post-image does not.
    Deleted,
}

impl FileState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Conflict => "conflict",
            Self::Resolved => "resolved",
            Self::Binary => "binary",
            Self::Added => "added",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
        }
    }
}

/// One conflicted file: its three stages, and the choices made about them.
#[derive(Clone, Debug)]
pub struct FileEntry {
    /// Repo-relative — what to show, and what to hand `git add`.
    pub path: PathBuf,
    /// Absolute — where to write the resolved content.
    pub abs: PathBuf,
    pub base: String,
    pub left: String,
    pub right: String,
    /// What the four title bars call this file's sections. Built where the
    /// sources are known — the three revisions come from different places in
    /// single-file mode and from one path's index stages in directory mode.
    pub names: SectionNames,
    pub kind: EntryKind,
    /// Built on first open. Running mergiraf for every file up front would
    /// stall the launch on a large merge.
    pub session: Option<MergeSession>,
    /// Set when a stage was not valid UTF-8.
    pub binary: bool,
    /// Whether the resolved content has been written and staged.
    pub saved: bool,
}

impl FileEntry {
    /// Derived rather than stored, so a file that mergiraf turns out to merge
    /// cleanly reads as resolved the moment it is opened, with nothing to
    /// update.
    pub fn state(&self) -> FileState {
        if self.binary {
            return FileState::Binary;
        }
        match self.kind {
            // A diff says what happened to the file; there is nothing to decide.
            EntryKind::Diff => match (self.left.is_empty(), self.right.is_empty()) {
                (true, false) => FileState::Added,
                (false, true) => FileState::Deleted,
                _ => FileState::Modified,
            },
            EntryKind::Merge => match &self.session {
                Some(session) if session.is_fully_resolved() => FileState::Resolved,
                _ => FileState::Conflict,
            },
        }
    }

    pub fn is_diff(&self) -> bool {
        self.kind == EntryKind::Diff
    }

    /// True when the file can be opened at all.
    pub fn is_openable(&self) -> bool {
        !self.binary
    }
}

/// Every conflicted file in a repository, and which one is being worked on.
#[derive(Debug)]
pub struct Workspace {
    files: Vec<FileEntry>,
    current: usize,
}

impl Workspace {
    pub fn new(files: Vec<FileEntry>) -> Self {
        // Start on the first file that can actually be opened.
        let current = files
            .iter()
            .position(FileEntry::is_openable)
            .unwrap_or(0);
        Self { files, current }
    }

    /// Read every unmerged path out of `repo`, optionally narrowed to a
    /// pathspec.
    ///
    /// A missing stage becomes an empty string: mergiraf already handles an
    /// empty side, and an add/add or delete/modify conflict is exactly that.
    pub fn from_repo(repo: &Repo, pathspec: Option<&Path>) -> Result<Self> {
        let entries = repo.unmerged(pathspec)?;
        let mut files = Vec::with_capacity(entries.len());

        for UnmergedEntry { path, stages } in entries {
            let mut text = [String::new(), String::new(), String::new()];
            let mut binary = false;
            for (i, sha) in stages.iter().enumerate() {
                let Some(sha) = sha else { continue };
                let bytes = repo
                    .blob(sha)
                    .with_context(|| format!("reading stage {} of {}", i + 1, path.display()))?;
                match String::from_utf8(bytes) {
                    Ok(s) => text[i] = s,
                    Err(_) => binary = true,
                }
            }
            let [base, left, right] = text;
            files.push(FileEntry {
                abs: repo.root().join(&path),
                // All three columns are stages of one path, so naming them by
                // that path three times says nothing. Git's own words for the
                // stages do.
                names: SectionNames {
                    left: "ours".into(),
                    base: "base".into(),
                    right: "theirs".into(),
                    merged: path.display().to_string(),
                },
                path,
                base,
                left,
                right,
                kind: EntryKind::Merge,
                session: None,
                binary,
                saved: false,
            });
        }

        Ok(Self::new(files))
    }

    /// Pair the files of two directory trees by relative path.
    ///
    /// This is what `git difftool --dir-diff` hands over: two temp trees, one
    /// per side. On non-Windows the right-hand tree may be symlinks into the
    /// working tree, so reads follow links; `.git` is skipped.
    pub fn from_dirs(old_root: &Path, new_root: &Path) -> Result<Self> {
        let mut paths: Vec<PathBuf> = walk(old_root)?;
        paths.extend(walk(new_root)?);
        paths.sort();
        paths.dedup();

        let files = paths
            .into_iter()
            .map(|rel| {
                // A path present on only one side reads as added or deleted.
                let (old, old_binary) = read_side(&old_root.join(&rel));
                let (new, new_binary) = read_side(&new_root.join(&rel));
                diff_entry(rel.clone(), new_root.join(&rel), old, new, old_binary || new_binary)
            })
            .collect();
        Ok(Self::new(files))
    }

    /// One pair of files, as `difftool.<tool>.cmd` passes $LOCAL and $REMOTE.
    ///
    /// `display` is $MERGED — the real path being compared, which is what
    /// should name the file and pick its language, not the temp file names.
    pub fn from_pair(old: &Path, new: &Path, display: &Path) -> Result<Self> {
        let (old_text, old_binary) = read_side(old);
        let (new_text, new_binary) = read_side(new);
        Ok(Self::new(vec![diff_entry(
            display.to_path_buf(),
            new.to_path_buf(),
            old_text,
            new_text,
            old_binary || new_binary,
        )]))
    }

    /// Everything `git diff <rev>` reports, against the working tree.
    pub fn from_diff(repo: &Repo, rev: &str, pathspec: Option<&Path>) -> Result<Self> {
        let mut files = Vec::new();
        for entry in repo.changed(rev, pathspec)? {
            let source = entry.old_path.as_ref().unwrap_or(&entry.path);
            let mut binary = false;

            // The pre-image comes out of the revision, the post-image off disk.
            let old = match repo
                .show(rev, source)
                .with_context(|| format!("reading {} from {rev}", source.display()))?
            {
                Some(bytes) => match String::from_utf8(bytes) {
                    Ok(text) => text,
                    Err(_) => {
                        binary = true;
                        String::new()
                    }
                },
                None => String::new(),
            };
            let abs = repo.root().join(&entry.path);
            let (new, new_binary) = read_side(&abs);
            files.push(diff_entry(
                entry.path,
                abs,
                old,
                new,
                binary || new_binary,
            ));
        }
        Ok(Self::new(files))
    }

    pub fn files(&self) -> &[FileEntry] {
        &self.files
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn current_index(&self) -> usize {
        self.current
    }

    pub fn current(&self) -> Option<&FileEntry> {
        self.files.get(self.current)
    }

    pub fn current_mut(&mut self) -> Option<&mut FileEntry> {
        self.files.get_mut(self.current)
    }

    /// How many files have every region decided.
    pub fn resolved_count(&self) -> usize {
        self.files
            .iter()
            .filter(|f| f.state() == FileState::Resolved)
            .count()
    }

    /// Move to `index` if it names a file that can be opened.
    pub fn select(&mut self, index: usize) -> bool {
        match self.files.get(index) {
            Some(file) if file.is_openable() => {
                self.current = index;
                true
            }
            _ => false,
        }
    }

    /// The next openable file after the current one, skipping binaries.
    ///
    /// Stops at the last file rather than wrapping, so repeated presses do not
    /// silently cycle you past work you have not looked at.
    pub fn advance(&mut self, forward: bool) -> bool {
        let found = if forward {
            (self.current + 1..self.files.len()).find(|&i| self.files[i].is_openable())
        } else {
            (0..self.current)
                .rev()
                .find(|&i| self.files[i].is_openable())
        };
        match found {
            Some(i) => {
                self.current = i;
                true
            }
            None => false,
        }
    }
}

/// Read one side of a diff: `(text, was_binary)`. A path that is not there at
/// all is simply empty, which is how an added or deleted file reads.
fn read_side(path: &Path) -> (String, bool) {
    match std::fs::read(path) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => (text, false),
            Err(_) => (String::new(), true),
        },
        Err(_) => (String::new(), false),
    }
}

fn diff_entry(path: PathBuf, abs: PathBuf, old: String, new: String, binary: bool) -> FileEntry {
    let name = path.display().to_string();
    FileEntry {
        // Both columns show the same file at two points in time, so naming
        // them by that path twice would say nothing; "old"/"new" does.
        names: SectionNames {
            left: "old".into(),
            base: String::new(),
            right: "new".into(),
            merged: name,
        },
        path,
        abs,
        base: String::new(),
        left: old,
        right: new,
        kind: EntryKind::Diff,
        session: None,
        binary,
        saved: false,
    }
}

/// Every file under `root`, as paths relative to it. Skips `.git`.
fn walk(root: &Path) -> Result<Vec<PathBuf>> {
    fn recurse(dir: &Path, root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            // A side may simply not exist; that is an empty tree, not an error.
            Err(_) => return Ok(()),
        };
        for entry in entries {
            let entry = entry.context("reading a directory entry")?;
            let path = entry.path();
            if path.file_name().is_some_and(|n| n == ".git") {
                continue;
            }
            // `metadata` follows symlinks, which is what --dir-diff needs.
            match std::fs::metadata(&path) {
                Ok(meta) if meta.is_dir() => recurse(&path, root, out)?,
                Ok(_) => {
                    if let Ok(rel) = path.strip_prefix(root) {
                        out.push(rel.to_path_buf());
                    }
                }
                Err(_) => {}
            }
        }
        Ok(())
    }

    let mut out = Vec::new();
    recurse(root, root, &mut out)?;
    Ok(out)
}

/// Names for the conflict markers of a file resolved out of a repository.
pub fn repo_marker_labels(path: &Path) -> crate::merge::session::MarkerLabels {
    use crate::merge::session::MarkerLabels;
    let name = path.display();
    MarkerLabels {
        left: format!("ours:{name}"),
        base: format!("base:{name}"),
        right: format!("theirs:{name}"),
        ..MarkerLabels::default()
    }
}

/// Re-export so callers do not need `external::git` for the stage order.
pub use git::{BASE, LEFT, RIGHT};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::merge::diff3::MergedChunk;
    use crate::merge::session::{MarkerLabels, Resolution};

    fn file(path: &str) -> FileEntry {
        FileEntry {
            path: PathBuf::from(path),
            abs: PathBuf::from("/repo").join(path),
            base: "b\n".into(),
            left: "l\n".into(),
            right: "r\n".into(),
            names: SectionNames::default(),
            kind: EntryKind::Merge,
            session: None,
            binary: false,
            saved: false,
        }
    }

    fn binary(path: &str) -> FileEntry {
        FileEntry {
            binary: true,
            ..file(path)
        }
    }

    fn session_with(conflicts: usize) -> MergeSession {
        let own = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let mut chunks = vec![MergedChunk::Resolved {
            lines: own(&["ctx"]),
        }];
        for _ in 0..conflicts {
            chunks.push(MergedChunk::Conflict {
                left: own(&["l"]),
                base: own(&["b"]),
                right: own(&["r"]),
            });
        }
        MergeSession::new(chunks, MarkerLabels::default())
    }

    #[test]
    fn an_unopened_file_reads_as_conflicted() {
        assert_eq!(file("a.rs").state(), FileState::Conflict);
    }

    #[test]
    fn state_follows_the_sessions_resolutions() {
        let mut f = file("a.rs");
        f.session = Some(session_with(2));
        assert_eq!(f.state(), FileState::Conflict);

        f.session
            .as_mut()
            .unwrap()
            .set_resolution(0, Resolution::Left);
        assert_eq!(f.state(), FileState::Conflict, "one region is still open");

        f.session
            .as_mut()
            .unwrap()
            .set_resolution(1, Resolution::Right);
        assert_eq!(f.state(), FileState::Resolved);
    }

    #[test]
    fn a_file_mergiraf_merges_cleanly_is_resolved_on_open() {
        let mut f = file("a.rs");
        f.session = Some(session_with(0));
        assert_eq!(f.state(), FileState::Resolved);
    }

    #[test]
    fn binary_outranks_everything_else() {
        let mut f = binary("logo.png");
        f.session = Some(session_with(0));
        assert_eq!(f.state(), FileState::Binary);
        assert!(!f.is_openable());
    }

    #[test]
    fn resolutions_survive_leaving_a_file_and_coming_back() {
        let mut w = Workspace::new(vec![file("a.rs"), file("b.rs")]);
        w.current_mut().unwrap().session = Some(session_with(1));
        w.current_mut()
            .unwrap()
            .session
            .as_mut()
            .unwrap()
            .set_resolution(0, Resolution::Right);

        assert!(w.advance(true));
        assert_eq!(w.current().unwrap().path, PathBuf::from("b.rs"));
        assert!(w.advance(false));

        let session = w.current().unwrap().session.as_ref().unwrap();
        assert_eq!(session.resolution(0), Resolution::Right);
        assert_eq!(w.current().unwrap().state(), FileState::Resolved);
    }

    #[test]
    fn advancing_skips_binaries_and_stops_at_the_ends() {
        let mut w = Workspace::new(vec![
            file("a.rs"),
            binary("logo.png"),
            binary("data.bin"),
            file("b.rs"),
        ]);
        assert_eq!(w.current_index(), 0);

        assert!(w.advance(true));
        assert_eq!(w.current().unwrap().path, PathBuf::from("b.rs"));
        assert!(!w.advance(true), "there is nothing after the last file");
        assert_eq!(w.current().unwrap().path, PathBuf::from("b.rs"));

        assert!(w.advance(false));
        assert_eq!(w.current().unwrap().path, PathBuf::from("a.rs"));
        assert!(!w.advance(false));
    }

    #[test]
    fn a_workspace_opening_on_a_binary_moves_past_it() {
        let w = Workspace::new(vec![binary("logo.png"), file("a.rs")]);
        assert_eq!(w.current_index(), 1);
    }

    #[test]
    fn selecting_a_binary_is_refused() {
        let mut w = Workspace::new(vec![file("a.rs"), binary("logo.png")]);
        assert!(!w.select(1));
        assert_eq!(w.current_index(), 0);
        assert!(!w.select(99));
        assert!(w.select(0));
    }

    #[test]
    fn resolved_count_tracks_the_whole_set() {
        let mut w = Workspace::new(vec![file("a.rs"), file("b.rs"), binary("c.png")]);
        assert_eq!(w.resolved_count(), 0);

        w.current_mut().unwrap().session = Some(session_with(0));
        assert_eq!(w.resolved_count(), 1);

        w.select(1);
        w.current_mut().unwrap().session = Some(session_with(1));
        assert_eq!(w.resolved_count(), 1, "b.rs still has an open region");
    }

    #[test]
    fn an_all_binary_workspace_has_no_openable_file() {
        let w = Workspace::new(vec![binary("a.png"), binary("b.png")]);
        assert_eq!(w.len(), 2);
        assert!(!w.current().unwrap().is_openable());
    }

    // ---- diff entries -----------------------------------------------------

    fn diff_file(path: &str, old: &str, new: &str) -> FileEntry {
        diff_entry(
            PathBuf::from(path),
            PathBuf::from("/repo").join(path),
            old.into(),
            new.into(),
            false,
        )
    }

    #[test]
    fn a_diff_entry_reports_what_happened_to_the_file() {
        assert_eq!(diff_file("a.rs", "old\n", "new\n").state(), FileState::Modified);
        assert_eq!(diff_file("a.rs", "", "new\n").state(), FileState::Added);
        assert_eq!(diff_file("a.rs", "old\n", "").state(), FileState::Deleted);
    }

    #[test]
    fn a_diff_entry_has_no_session_to_resolve() {
        let f = diff_file("a.rs", "old\n", "new\n");
        assert!(f.is_diff());
        assert!(f.session.is_none());
        assert_eq!(f.kind, EntryKind::Diff);
        // and its columns are named for the two points in time, not the path
        assert_eq!(f.names.left, "old");
        assert_eq!(f.names.right, "new");
        assert_eq!(f.names.merged, "a.rs");
    }

    #[test]
    fn a_binary_diff_entry_outranks_its_status() {
        let mut f = diff_file("logo.png", "", "x\n");
        f.binary = true;
        assert_eq!(f.state(), FileState::Binary);
        assert!(!f.is_openable());
    }

    #[test]
    fn every_state_has_its_own_word() {
        use FileState::*;
        let words: Vec<&str> = [Conflict, Resolved, Binary, Added, Modified, Deleted]
            .iter()
            .map(|s| s.label())
            .collect();
        assert_eq!(
            words,
            ["conflict", "resolved", "binary", "added", "modified", "deleted"]
        );
        // one word each, so a list row stays one column wide
        assert!(words.iter().all(|w| !w.contains(' ')));
    }

    #[test]
    fn pairing_two_trees_unions_and_sorts_their_paths() {
        let old = tempfile::tempdir().unwrap();
        let new = tempfile::tempdir().unwrap();
        let write = |root: &Path, rel: &str, text: &str| {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };

        write(old.path(), "src/a.rs", "old a\n");
        write(old.path(), "gone.rs", "bye\n");
        write(new.path(), "src/a.rs", "new a\n");
        write(new.path(), "added.rs", "hi\n");
        // a .git directory in either tree is not content
        write(old.path(), ".git/config", "[core]\n");
        write(new.path(), ".git/config", "[core]\n");

        let w = Workspace::from_dirs(old.path(), new.path()).unwrap();
        let paths: Vec<String> = w
            .files()
            .iter()
            .map(|f| f.path.display().to_string())
            .collect();
        assert_eq!(paths, ["added.rs", "gone.rs", "src/a.rs"]);

        let state = |name: &str| {
            w.files()
                .iter()
                .find(|f| f.path.to_str() == Some(name))
                .unwrap()
                .state()
        };
        assert_eq!(state("added.rs"), FileState::Added);
        assert_eq!(state("gone.rs"), FileState::Deleted);
        assert_eq!(state("src/a.rs"), FileState::Modified);
    }

    #[test]
    fn pairing_a_tree_against_a_missing_one_reads_as_all_added() {
        let new = tempfile::tempdir().unwrap();
        std::fs::write(new.path().join("a.rs"), "hi\n").unwrap();
        let w = Workspace::from_dirs(Path::new("/no/such/tree"), new.path()).unwrap();
        assert_eq!(w.len(), 1);
        assert_eq!(w.files()[0].state(), FileState::Added);
    }

    #[test]
    fn a_pair_of_files_is_named_after_the_compared_path() {
        let dir = tempfile::tempdir().unwrap();
        let (old, new) = (dir.path().join("tmp1"), dir.path().join("tmp2"));
        std::fs::write(&old, "a\n").unwrap();
        std::fs::write(&new, "b\n").unwrap();

        // git difftool hands over temp files; $MERGED is the real path
        let w = Workspace::from_pair(&old, &new, Path::new("src/real.rs")).unwrap();
        assert_eq!(w.len(), 1);
        assert_eq!(w.files()[0].path, PathBuf::from("src/real.rs"));
        assert_eq!(w.files()[0].left, "a\n");
        assert_eq!(w.files()[0].right, "b\n");
        assert_eq!(w.files()[0].state(), FileState::Modified);
    }

    #[test]
    fn marker_labels_name_the_sides_the_way_git_does() {
        let labels = repo_marker_labels(Path::new("src/main.rs"));
        assert_eq!(labels.left, "ours:src/main.rs");
        assert_eq!(labels.base, "base:src/main.rs");
        assert_eq!(labels.right, "theirs:src/main.rs");
    }
}
