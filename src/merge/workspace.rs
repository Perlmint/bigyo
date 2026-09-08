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

/// The only thing the file list says about a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileState {
    /// Some region is still undecided — or the file has not been opened yet, in
    /// which case this is simply what git said about it.
    Conflict,
    /// Every region has been decided.
    Resolved,
    /// A stage was not valid UTF-8, so there is nothing to show.
    Binary,
}

impl FileState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Conflict => "conflict",
            Self::Resolved => "resolved",
            Self::Binary => "binary",
        }
    }
}

/// One conflicted file: its three stages, and the choices made about them.
#[derive(Clone, Debug)]
pub struct ConflictFile {
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
    /// Built on first open. Running mergiraf for every file up front would
    /// stall the launch on a large merge.
    pub session: Option<MergeSession>,
    /// Set when a stage was not valid UTF-8.
    pub binary: bool,
    /// Whether the resolved content has been written and staged.
    pub saved: bool,
}

impl ConflictFile {
    /// Derived rather than stored, so a file that mergiraf turns out to merge
    /// cleanly reads as resolved the moment it is opened, with nothing to
    /// update.
    pub fn state(&self) -> FileState {
        if self.binary {
            return FileState::Binary;
        }
        match &self.session {
            Some(session) if session.is_fully_resolved() => FileState::Resolved,
            _ => FileState::Conflict,
        }
    }

    /// True when the file can be opened in the merge screen at all.
    pub fn is_openable(&self) -> bool {
        !self.binary
    }
}

/// Every conflicted file in a repository, and which one is being worked on.
#[derive(Debug)]
pub struct Workspace {
    files: Vec<ConflictFile>,
    current: usize,
}

impl Workspace {
    pub fn new(files: Vec<ConflictFile>) -> Self {
        // Start on the first file that can actually be opened.
        let current = files
            .iter()
            .position(ConflictFile::is_openable)
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
            files.push(ConflictFile {
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
                session: None,
                binary,
                saved: false,
            });
        }

        Ok(Self::new(files))
    }

    pub fn files(&self) -> &[ConflictFile] {
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

    pub fn current(&self) -> Option<&ConflictFile> {
        self.files.get(self.current)
    }

    pub fn current_mut(&mut self) -> Option<&mut ConflictFile> {
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

    fn file(path: &str) -> ConflictFile {
        ConflictFile {
            path: PathBuf::from(path),
            abs: PathBuf::from("/repo").join(path),
            base: "b\n".into(),
            left: "l\n".into(),
            right: "r\n".into(),
            names: SectionNames::default(),
            session: None,
            binary: false,
            saved: false,
        }
    }

    fn binary(path: &str) -> ConflictFile {
        ConflictFile {
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

    #[test]
    fn marker_labels_name_the_sides_the_way_git_does() {
        let labels = repo_marker_labels(Path::new("src/main.rs"));
        assert_eq!(labels.left, "ours:src/main.rs");
        assert_eq!(labels.base, "base:src/main.rs");
        assert_eq!(labels.right, "theirs:src/main.rs");
    }
}
