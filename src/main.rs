//! `bigyo` — resolve syntax-aware three-way merges in the terminal.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Parser;

use bigyo::external::git::Repo;
use bigyo::external::mergiraf;
use bigyo::merge::session::{MarkerLabels, MergeSession};
use bigyo::merge::workspace::{EntryKind, FileEntry, Workspace};
use bigyo::render::highlight::{Assets, DEFAULT_THEME};
use bigyo::render::panes::SectionNames;
use bigyo::render::theme::DiffTheme;
use bigyo::tui::app::{App, Destination};

/// Shown by `--help`, where there is room for it; `-h` stays short.
const GIT_GUIDE: &str = "\
MODES:
  bigyo <base> <left> <right>   resolve one merge, as git mergetool passes it
  bigyo <old> <new>             diff two files, or two directories
  bigyo                         resolve every conflicted file in the repository
  bigyo <path>                  the same, narrowed to a path
  bigyo --diff [<rev>]          diff a revision against the working tree

GIT DIFFTOOL:
  git config difftool.bigyo.cmd 'bigyo \"$LOCAL\" \"$REMOTE\" --path-name \"$MERGED\"'
  git difftool -t bigyo [<rev>]            # one launch per changed file
  git difftool -t bigyo --dir-diff [<rev>] # one launch for all of them

  Make it the default:  git config diff.tool bigyo

GIT MERGETOOL:
  git config mergetool.bigyo.cmd 'bigyo \"$BASE\" \"$LOCAL\" \"$REMOTE\" -o \"$MERGED\"'
  git mergetool -t bigyo

  Make it the default:  git config merge.tool bigyo

  Note the argument order: bigyo takes <base> <left> <right>, so $BASE (the
  common ancestor) comes first, then $LOCAL (ours) and $REMOTE (theirs).

  Leave mergetool.bigyo.trustExitCode unset. bigyo exits 0 even if you quit
  without deciding every conflict, so let git ask whether the merge succeeded
  rather than have it assume so and stage a file that still has markers in it.

WITHOUT GIT'S WRAPPERS:
  bigyo's own repository modes do the same job in a single launch and know
  more about what they are doing:

    bigyo           every conflicted file; resolving one writes it and `git add`s it
    bigyo --diff    every change against HEAD

  Both open a file list on `f`, and `]`/`[` step between files.

  --path-name is where $MERGED goes: the real path of the file being compared.
  git's temp files keep the original extension, so detection usually survives
  without it — but mergetool mangles the basename of a file that has no
  extension (Makefile becomes Makefile_BASE_123), which defeats detection for
  the highlighting and for the structural merge alike. bigyo falls back to -o
  for the name, which is why the mergetool line above needs nothing extra.

LANGUAGE:
  Resolved per file: whatever you pick in-session with `L`, else the
  linguist-language gitattribute, else the file extension.
";

#[derive(Parser, Debug)]
#[command(name = "bigyo", version, about, long_about = None)]
#[command(after_help = "\
MODES:
  bigyo <base> <left> <right>   resolve one merge, as git mergetool passes it
  bigyo <old> <new>             diff two files, or two directories
  bigyo                         resolve every conflicted file in the repository
  bigyo <path>                  the same, narrowed to a path
  bigyo --diff [<rev>]          diff a revision against the working tree

Run `bigyo --help` for how to wire bigyo into git difftool and mergetool.")]
#[command(after_long_help = GIT_GUIDE)]
struct Args {
    /// `<base> <left> <right>` for one file, a path to narrow directory mode,
    /// or nothing for the whole repository
    #[arg(num_args = 0..=3)]
    paths: Vec<PathBuf>,

    /// Syntax theme, as named by bat
    #[arg(short, long, default_value = DEFAULT_THEME)]
    theme: String,

    /// Final path of the merged file; used for language detection
    #[arg(short, long)]
    path_name: Option<PathBuf>,

    /// Where to write the resolved file. Single-file mode only — in directory
    /// mode each file is written back to its own path.
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Diff a revision against the working tree instead of merging.
    /// Defaults to HEAD when given without a value.
    #[arg(long, num_args = 0..=1, default_missing_value = "HEAD")]
    diff: Option<String>,

    /// List the available syntax themes and exit
    #[arg(long)]
    list_themes: bool,
}

impl Args {
    /// `--path-name`, treating an empty value as absent.
    ///
    /// `git difftool --dir-diff` substitutes an empty `$MERGED`, so the
    /// documented config really does hand over `--path-name ""`.
    fn path_name(&self) -> Option<&Path> {
        self.path_name
            .as_deref()
            .filter(|p| !p.as_os_str().is_empty())
    }
}

fn main() -> Result<()> {
    let args = Args::parse();

    if args.list_themes {
        for name in Assets::theme_names() {
            println!("{name}");
        }
        return Ok(());
    }

    let assets = Assets::new();
    let theme = DiffTheme::default();

    let (workspace, destination) = match (&args.diff, args.paths.len()) {
        (Some(rev), _) => match git_diff(&args, rev)? {
            Some(pair) => pair,
            None => return Ok(()),
        },
        (None, 3) => single_file(&args)?,
        (None, 2) => match two_way(&args)? {
            Some(pair) => pair,
            None => return Ok(()),
        },
        (None, 0 | 1) => match repository(&args)? {
            Some(pair) => pair,
            None => return Ok(()),
        },
        (None, n) => bail!(
            "expected 3 paths (<base> <left> <right>), 2 (<old> <new>), or at \
             most 1 (a path to narrow the repository search), got {n}"
        ),
    };

    let app = App::new(&assets, workspace, theme, args.theme.clone(), destination)?;

    let mut terminal = ratatui::init();
    let result = app.run(&mut terminal);
    ratatui::restore();
    result
}

/// `bigyo <base> <left> <right>` — the `mergetool.<tool>.cmd` shape.
fn single_file(args: &Args) -> Result<(Workspace, Destination)> {
    let [base, left, right] = [&args.paths[0], &args.paths[1], &args.paths[2]];
    for path in [base, left, right] {
        if !path.is_file() {
            bail!("no such file: {}", path.display());
        }
    }

    // git mergetool mangles the basename of an extensionless file
    // (`Makefile` becomes `Makefile_BASE_123`), which defeats detection for
    // syntect *and* mergiraf. `-o` is `$MERGED` there — the real name — so it
    // stands in when --path-name was not given.
    let display: &Path = args.path_name().or(args.output.as_deref()).unwrap_or(left);
    // No language is passed: mergiraf reads `mergiraf.language` and
    // `linguist-language` itself, with a precedence of its own.
    let merged = mergiraf::merge(base, left, right, None, Some(display))?;

    let read =
        |p: &Path| std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()));
    let name = |p: &Path| p.display().to_string();
    let output = args.output.clone().unwrap_or_else(|| display.to_path_buf());
    let file = FileEntry {
        path: display.to_path_buf(),
        abs: output.clone(),
        base: read(base)?,
        left: read(left)?,
        right: read(right)?,
        // Three genuinely different files here, so name each column after the
        // one it shows; the shared directories are dropped.
        names: SectionNames::new(&name(left), &name(base), &name(right), &name(&output)),
        kind: EntryKind::Merge,
        attr_language: None,
        language: None,
        session: Some(MergeSession::new(
            merged.chunks,
            MarkerLabels {
                left: name(left),
                base: name(base),
                right: name(right),
                ..MarkerLabels::default()
            },
        )),
        binary: false,
        saved: false,
    };

    Ok((
        Workspace::new(vec![file]),
        Destination::Path(args.output.clone()),
    ))
}

/// `bigyo <old> <new>` — two files, or the two directory trees `git difftool
/// --dir-diff` hands over.
fn two_way(args: &Args) -> Result<Option<(Workspace, Destination)>> {
    let (old, new) = (&args.paths[0], &args.paths[1]);

    let workspace = if old.is_dir() || new.is_dir() {
        if !(old.is_dir() && new.is_dir()) {
            bail!(
                "both sides must be files or both directories: {} and {}",
                old.display(),
                new.display()
            );
        }
        Workspace::from_dirs(old, new)?
    } else {
        // `--path-name` is where $MERGED goes: the real path of the file being
        // compared, which is what should drive language detection and the title.
        let display = args.path_name().unwrap_or(new);
        Workspace::from_pair(old, new, display)?
    };

    if workspace.is_empty() {
        println!("no files to compare");
        return Ok(None);
    }
    if !workspace.files().iter().any(|f| f.is_openable()) {
        println!("{} file(s), but none can be shown as text", workspace.len());
        return Ok(None);
    }
    Ok(Some((workspace, Destination::Path(None))))
}

/// `bigyo --diff [<rev>] [<path>…]` — what git itself says changed.
fn git_diff(args: &Args, rev: &str) -> Result<Option<(Workspace, Destination)>> {
    let cwd = std::env::current_dir().context("reading the current directory")?;
    let Some(repo) = Repo::discover(&cwd)? else {
        bail!("--diff needs a git repository; pass <old> <new> to compare two paths instead");
    };

    let workspace = Workspace::from_diff(&repo, rev, args.paths.first().map(PathBuf::as_path))?;
    if workspace.is_empty() {
        println!("no changes against {rev}");
        return Ok(None);
    }
    Ok(Some((workspace, Destination::Path(None))))
}

/// `bigyo` / `bigyo <path>` — every unmerged file git knows about.
///
/// `Ok(None)` means there was nothing to do and the reason has been printed.
fn repository(args: &Args) -> Result<Option<(Workspace, Destination)>> {
    if args.output.is_some() {
        bail!(
            "--output is for single-file mode; in a repository each file is written back to its own path"
        );
    }

    let cwd = std::env::current_dir().context("reading the current directory")?;
    let Some(repo) = Repo::discover(&cwd)? else {
        bail!(
            "not inside a git repository — pass <base> <left> <right> to resolve a single file \
             instead"
        );
    };

    let pathspec = args.paths.first().map(PathBuf::as_path);
    let workspace = Workspace::from_repo(&repo, pathspec)?;
    if workspace.is_empty() {
        println!("no conflicted files");
        return Ok(None);
    }
    if !workspace.files().iter().any(|f| f.is_openable()) {
        println!(
            "{} conflicted file(s), but none can be shown as text",
            workspace.len()
        );
        return Ok(None);
    }

    Ok(Some((workspace, Destination::Repo(repo))))
}
