//! Tests that actually invoke `difft` and `mergiraf`.
//!
//! Marked `#[ignore]` so the ordinary `cargo test` run needs neither binary;
//! run them with `cargo test -- --ignored` to catch upstream CLI or output
//! format changes.

use std::path::Path;

use bigyo::external::difft::{DiffResult, Differ, DifftCli};
use bigyo::external::mergiraf;
use bigyo::merge::diff3::{DEFAULT_MARKER_SIZE, MergedChunk, parse_diff3};
use bigyo::merge::locate::conflict_base_ranges;
use bigyo::merge::session::{MarkerLabels, MergeSession, Resolution};
use bigyo::render::align::{AlignedRow, align3};
use bigyo::render::document::{RowKind, build_document};
use bigyo::render::highlight::{Assets, DEFAULT_THEME};
use bigyo::render::panes::build_panes;
use bigyo::render::theme::{DiffTheme, Side};

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn read(name: &str) -> String {
    std::fs::read_to_string(fixture(name)).expect("fixture")
}

fn rust_differ() -> DifftCli {
    DifftCli::new(Some("rust".into()), Some("rs".into()))
}

#[test]
#[ignore = "requires the `mergiraf` binary"]
fn mergiraf_produces_a_parseable_conflict() {
    let out = mergiraf::merge(
        &fixture("single/base.rs"),
        &fixture("single/left.rs"),
        &fixture("single/right.rs"),
        Some("rust"),
        Some(Path::new("sample.rs")),
    )
    .expect("mergiraf run");

    assert!(out.has_conflicts, "the fixtures are designed to conflict");
    assert!(
        out.text.contains("<<<<<<< LEFT") && out.text.contains("||||||| BASE"),
        "expected diff3 markers with our labels, got:\n{}",
        out.text
    );

    let conflicts: Vec<_> = out
        .chunks
        .iter()
        .filter(|c| matches!(c, MergedChunk::Conflict { .. }))
        .collect();
    assert!(
        !conflicts.is_empty(),
        "parser found no conflict in mergiraf's output"
    );
    assert!(
        out.chunks
            .iter()
            .any(|c| matches!(c, MergedChunk::Resolved { .. })),
        "expected some resolved context too"
    );
}

#[test]
#[ignore = "requires the `difft` binary"]
#[allow(clippy::single_range_in_vec_init)]
fn difft_reports_byte_ranges_for_changed_tokens() {
    let differ = rust_differ();
    let DiffResult { lhs, rhs, .. } = differ
        .try_diff("let a = 1;\nlet b = 2;\n", "let a = 1;\nlet b = 22;\n")
        .expect("difft run");

    // line 0 is identical; line 1 changed
    assert!(lhs.get(0).is_empty() && rhs.get(0).is_empty());
    assert!(
        !lhs.get(1).is_empty(),
        "expected a novel range on the changed line"
    );
    assert!(!rhs.get(1).is_empty());
    // `2` -> `22`, at byte 8 of `let b = 2;`
    assert_eq!(lhs.get(1), &[8..9]);
    assert_eq!(rhs.get(1), &[8..10]);
}

#[test]
#[ignore = "requires the `difft` binary"]
fn difft_offsets_are_byte_offsets_over_cjk() {
    let differ = rust_differ();
    let lhs_src = "let x = \"안녕하세요\";\n";
    let DiffResult { lhs, .. } = differ
        .try_diff(lhs_src, "let x = \"안녕\";\n")
        .expect("difft run");

    let line = lhs_src.lines().next().unwrap();
    for r in lhs.get(0) {
        assert!(
            line.is_char_boundary(r.start) && line.is_char_boundary(r.end) && r.end <= line.len(),
            "range {r:?} is not a valid byte range of {line:?}"
        );
    }
    // the Korean literal itself must be among what changed
    let covered: String = lhs.get(0).iter().map(|r| &line[r.clone()]).collect();
    assert!(covered.contains("안녕하세요"), "got {covered:?}");
}

#[test]
#[ignore = "requires both `mergiraf` and `difft`"]
fn end_to_end_document_has_all_three_conflict_sides() {
    let merged = mergiraf::merge(
        &fixture("single/base.rs"),
        &fixture("single/left.rs"),
        &fixture("single/right.rs"),
        Some("rust"),
        Some(Path::new("sample.rs")),
    )
    .expect("mergiraf run");

    let assets = Assets::new();
    let hl = assets
        .highlighter(Some("rust"), Some(Path::new("sample.rs")), DEFAULT_THEME)
        .unwrap();
    let theme = DiffTheme::default();
    let differ = rust_differ();
    let session = MergeSession::new(merged.chunks, MarkerLabels::default());
    let doc = build_document(&session, &hl, &differ, &theme);

    assert!(!doc.conflicts.is_empty());
    for side in [Side::Left, Side::Base, Side::Right] {
        assert!(
            doc.rows.iter().any(|r| r.kind == RowKind::Side(side)),
            "no rows for {side:?}"
        );
    }

    // difftastic should have marked *something* inside the conflict, otherwise
    // the whole point of the tool is missing.
    let emphasized = doc.rows.iter().any(|r| {
        let Some(row_bg) = r.row_bg else { return false };
        r.spans.iter().any(|s| s.bg.is_some_and(|b| b != row_bg))
    });
    assert!(
        emphasized,
        "expected intra-line emphasis inside a conflict block"
    );
}

#[test]
#[ignore = "requires the `difft` binary"]
fn side_by_side_gaps_where_a_file_has_no_line() {
    let assets = Assets::new();
    let hl = assets
        .highlighter(Some("rust"), Some(Path::new("sample.rs")), DEFAULT_THEME)
        .unwrap();
    let theme = DiffTheme::default();
    let doc = build_panes(
        &MergeSession::new(Vec::new(), MarkerLabels::default()),
        &read("single/base.rs"),
        &read("single/left.rs"),
        &read("single/right.rs"),
        &hl,
        &rust_differ(),
        &theme,
    );

    // left's extra `let c = 3;` exists in no other revision
    let row = doc
        .rows
        .iter()
        .find(|r| r.cell(Side::Left).text().contains("let c = 3"))
        .expect("left's extra line should have a row");
    assert!(
        row.cell(Side::Base).is_gap() && row.cell(Side::Right).is_gap(),
        "the other two panes should be gaps here"
    );
    assert_eq!(row.cell(Side::Base).bg, Some(theme.gap_bg));
    assert!(row.changed);

    // and difftastic should mark the digits that actually differ
    let row = doc
        .rows
        .iter()
        .find(|r| r.cell(Side::Base).text().contains("let a ="))
        .expect("the `let a` row");
    let emphasized: String = row
        .cell(Side::Left)
        .spans
        .iter()
        .filter(|s| s.bg == Some(theme.left_emph_bg))
        .map(|s| s.text.as_str())
        .collect();
    assert_eq!(emphasized, "100");
}

#[test]
#[ignore = "requires the `mergiraf` binary"]
fn a_multi_line_conflict_keeps_each_sides_own_length() {
    let out = mergiraf::merge(
        &fixture("multi/base.rs"),
        &fixture("multi/left.rs"),
        &fixture("multi/right.rs"),
        Some("rust"),
        Some(Path::new("sample.rs")),
    )
    .expect("mergiraf run");
    assert!(out.has_conflicts);

    let conflict = out
        .chunks
        .iter()
        .find_map(|c| match c {
            MergedChunk::Conflict { left, base, right } => Some((left, base, right)),
            _ => None,
        })
        .expect("a conflict");
    assert_eq!(
        (conflict.0.len(), conflict.1.len(), conflict.2.len()),
        (4, 3, 1),
        "left/base/right should keep their own lengths"
    );

    let assets = Assets::new();
    let hl = assets
        .highlighter(Some("rust"), Some(Path::new("sample.rs")), DEFAULT_THEME)
        .unwrap();
    let theme = DiffTheme::default();
    let session = MergeSession::new(out.chunks.clone(), MarkerLabels::default());
    let doc = build_document(&session, &hl, &rust_differ(), &theme);
    let count = |kind| doc.rows.iter().filter(|r| r.kind == kind).count();
    assert_eq!(count(RowKind::Side(Side::Left)), 4);
    assert_eq!(count(RowKind::Side(Side::Base)), 3);
    assert_eq!(count(RowKind::Side(Side::Right)), 1);
}

#[test]
#[ignore = "requires the `difft` binary"]
fn the_multi_fixture_still_joins_into_the_expected_three_way_alignment() {
    // Guards against difftastic changing how it aligns lines: this is the exact
    // join the side-by-side view depends on.
    let differ = rust_differ();
    let (base, left, right) = (
        read("multi/base.rs"),
        read("multi/left.rs"),
        read("multi/right.rs"),
    );
    let vs_left: DiffResult = differ.diff(&base, &left);
    let vs_right: DiffResult = differ.diff(&base, &right);

    let row = |left, base, right| AlignedRow { left, base, right };
    assert_eq!(
        align3(&vs_left.aligned, &vs_right.aligned),
        vec![
            row(Some(0), Some(0), Some(0)),
            row(Some(1), Some(1), Some(1)),
            row(Some(2), Some(2), None),
            row(Some(3), Some(3), None),
            row(Some(4), None, None),
            row(Some(5), Some(4), Some(2)),
            row(Some(6), Some(5), Some(3)),
        ]
    );
}

#[test]
#[ignore = "requires both `mergiraf` and `difft`"]
fn the_multi_fixture_panes_show_one_hunk_spanning_the_replaced_region() {
    let assets = Assets::new();
    let hl = assets
        .highlighter(Some("rust"), Some(Path::new("sample.rs")), DEFAULT_THEME)
        .unwrap();
    let theme = DiffTheme::default();
    let doc = build_panes(
        &MergeSession::new(Vec::new(), MarkerLabels::default()),
        &read("multi/base.rs"),
        &read("multi/left.rs"),
        &read("multi/right.rs"),
        &hl,
        &rust_differ(),
        &theme,
    );

    // the trailing empty row is dropped, leaving six
    assert_eq!(doc.len(), 6);
    // the whole replaced region reads as a single hunk
    assert_eq!(doc.hunks, vec![1]);
    // a two-row gap in the right column
    assert!(doc.rows[2].cell(Side::Right).is_gap());
    assert!(doc.rows[3].cell(Side::Right).is_gap());
    // and a left-only row gapped in both other panes
    assert_eq!(doc.rows[4].cell(Side::Left).text().trim(), "w");
    assert!(doc.rows[4].cell(Side::Base).is_gap());
    assert!(doc.rows[4].cell(Side::Right).is_gap());
}

#[test]
#[ignore = "requires the `mergiraf` binary"]
fn resolving_to_left_reproduces_the_left_file_with_no_markers() {
    let out = mergiraf::merge(
        &fixture("multi/base.rs"),
        &fixture("multi/left.rs"),
        &fixture("multi/right.rs"),
        Some("rust"),
        Some(Path::new("sample.rs")),
    )
    .expect("mergiraf run");

    let mut session = MergeSession::new(out.chunks, MarkerLabels::default());
    assert_eq!(session.conflict_count(), 1);

    // unresolved: the region goes out as diff3 markers and parses back
    let partial = session.to_output();
    assert!(partial.contains("<<<<<<<"), "in:\n{partial}");
    assert_eq!(
        parse_diff3(&partial, DEFAULT_MARKER_SIZE),
        session.chunks(),
        "an unfinished merge must round-trip"
    );

    // resolved: no markers at all, and the result is exactly the left revision
    session.set_resolution(0, Resolution::Left);
    let resolved = session.to_output();
    for marker in ["<<<<<<<", "|||||||", "=======", ">>>>>>>"] {
        assert!(!resolved.contains(marker), "found {marker} in:\n{resolved}");
    }
    assert_eq!(resolved, read("multi/left.rs"));

    session.set_resolution(0, Resolution::Right);
    assert_eq!(session.to_output(), read("multi/right.rs"));
}

#[test]
#[ignore = "requires the `mergiraf` binary"]
fn the_multi_conflict_is_located_at_its_real_base_lines() {
    let out = mergiraf::merge(
        &fixture("multi/base.rs"),
        &fixture("multi/left.rs"),
        &fixture("multi/right.rs"),
        Some("rust"),
        Some(Path::new("sample.rs")),
    )
    .expect("mergiraf run");

    let base = read("multi/base.rs");
    let base_lines: Vec<&str> = base.split('\n').collect();
    assert_eq!(
        conflict_base_ranges(&out.chunks, &base_lines),
        vec![1..4],
        "the conflict replaces the three-line body"
    );
}

#[test]
#[ignore = "requires both `mergiraf` and `difft`"]
fn the_panes_tint_the_conflict_region_and_follow_a_resolution() {
    let out = mergiraf::merge(
        &fixture("multi/base.rs"),
        &fixture("multi/left.rs"),
        &fixture("multi/right.rs"),
        Some("rust"),
        Some(Path::new("sample.rs")),
    )
    .expect("mergiraf run");
    let mut session = MergeSession::new(out.chunks, MarkerLabels::default());

    let assets = Assets::new();
    let hl = assets
        .highlighter(Some("rust"), Some(Path::new("sample.rs")), DEFAULT_THEME)
        .unwrap();
    let theme = DiffTheme::default();
    let build = |session: &MergeSession| {
        build_panes(
            session,
            &read("multi/base.rs"),
            &read("multi/left.rs"),
            &read("multi/right.rs"),
            &hl,
            &rust_differ(),
            &theme,
        )
    };

    let doc = build(&session);
    assert_eq!(doc.conflicts.len(), 1);
    let region = doc.conflicts[0].clone();
    assert!(!region.is_empty(), "the region should own some rows");
    for row in &doc.rows[region.clone()] {
        let cell = row.cell(Side::Left);
        if !cell.is_gap() {
            assert_eq!(cell.bg, Some(theme.conflict_bg));
        }
    }

    session.set_resolution(0, Resolution::Left);
    let doc = build(&session);
    for row in &doc.rows[region] {
        let cell = row.cell(Side::Left);
        if !cell.is_gap() {
            assert_eq!(cell.bg, Some(theme.resolved_bg));
        }
    }
}

/// `tests/fixtures/chunks` is the multi-region case: three conflicts of
/// different shapes (1/1/1, 3/1/1 and 1/1/2 lines) with auto-merged content
/// between them, including a function only the left side added.
fn chunks_merge() -> mergiraf::MergeOutput {
    mergiraf::merge(
        &fixture("chunks/base.rs"),
        &fixture("chunks/left.rs"),
        &fixture("chunks/right.rs"),
        Some("rust"),
        Some(Path::new("sample.rs")),
    )
    .expect("mergiraf run")
}

#[test]
#[ignore = "requires the `mergiraf` binary"]
fn the_chunks_fixture_really_produces_three_separate_conflicts() {
    let out = chunks_merge();
    assert!(out.has_conflicts);

    let shapes: Vec<(usize, usize, usize)> = out
        .chunks
        .iter()
        .filter_map(|c| match c {
            MergedChunk::Conflict { left, base, right } => {
                Some((left.len(), base.len(), right.len()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        shapes,
        vec![(1, 1, 1), (3, 1, 1), (1, 1, 2)],
        "three regions, each a different shape"
    );

    // the region only left touched was auto-merged, not turned into a conflict
    let resolved: String = out
        .chunks
        .iter()
        .filter_map(|c| match c {
            MergedChunk::Resolved { lines } => Some(lines.join("\n")),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        resolved.contains("fn helper()"),
        "left's new function should have merged cleanly:\n{resolved}"
    );
}

#[test]
#[ignore = "requires the `mergiraf` binary"]
fn the_three_chunks_are_located_at_distinct_base_lines() {
    let out = chunks_merge();
    let base = read("chunks/base.rs");
    let base_lines: Vec<&str> = base.split('\n').collect();

    let ranges = conflict_base_ranges(&out.chunks, &base_lines);
    assert_eq!(ranges, vec![1..2, 9..10, 13..14]);
    for pair in ranges.windows(2) {
        assert!(pair[0].end <= pair[1].start, "{pair:?} overlap");
    }
}

#[test]
#[ignore = "requires the `mergiraf` binary"]
fn each_chunk_resolves_independently_and_writes_without_markers() {
    let mut session = MergeSession::new(chunks_merge().chunks, MarkerLabels::default());
    assert_eq!(session.conflict_count(), 3);

    // With nothing decided yet, the output is the merge we started from.
    assert_eq!(
        parse_diff3(&session.to_output(), DEFAULT_MARKER_SIZE),
        session.chunks(),
        "an untouched session must round-trip exactly"
    );

    // Resolve one region at a time. Each write keeps the undecided regions as
    // markers, verbatim — that is what lets an unfinished merge be handed to
    // git or an editor and picked back up.
    let conflicts = |chunks: &[MergedChunk]| -> Vec<MergedChunk> {
        chunks
            .iter()
            .filter(|c| matches!(c, MergedChunk::Conflict { .. }))
            .cloned()
            .collect()
    };
    for step in 0..3 {
        let out = session.to_output();
        assert_eq!(
            out.matches("<<<<<<<").count(),
            3 - step,
            "after {step} resolutions:\n{out}"
        );

        let still_open: Vec<MergedChunk> = conflicts(session.chunks())
            .into_iter()
            .enumerate()
            .filter(|(i, _)| !session.resolution(*i).is_resolved())
            .map(|(_, c)| c)
            .collect();
        assert_eq!(
            conflicts(&parse_diff3(&out, DEFAULT_MARKER_SIZE)),
            still_open,
            "the undecided regions must survive a write/parse cycle unchanged"
        );

        session.set_resolution(step, Resolution::Left);
    }

    // all left is exactly the left revision
    let all_left = session.to_output();
    for marker in ["<<<<<<<", "|||||||", "=======", ">>>>>>>"] {
        assert!(!all_left.contains(marker), "found {marker} in:\n{all_left}");
    }
    assert_eq!(all_left, read("chunks/left.rs"));

    // and mixing choices takes each region's own side
    session.set_resolution(1, Resolution::Right);
    session.set_resolution(2, Resolution::Base);
    let mixed = session.to_output();
    assert!(mixed.contains("    10"), "region 0 kept left:\n{mixed}");
    assert!(
        mixed.contains("    33") && !mixed.contains("    a + b"),
        "region 1 took right"
    );
    assert!(
        mixed.contains("    4\n") && !mixed.contains("    40"),
        "region 2 took base"
    );
}

#[test]
#[ignore = "requires both `mergiraf` and `difft`"]
fn the_panes_give_each_chunk_its_own_row_range() {
    let session = MergeSession::new(chunks_merge().chunks, MarkerLabels::default());

    let assets = Assets::new();
    let hl = assets
        .highlighter(Some("rust"), Some(Path::new("sample.rs")), DEFAULT_THEME)
        .unwrap();
    let theme = DiffTheme::default();
    let doc = build_panes(
        &session,
        &read("chunks/base.rs"),
        &read("chunks/left.rs"),
        &read("chunks/right.rs"),
        &hl,
        &rust_differ(),
        &theme,
    );

    assert_eq!(doc.conflicts.len(), 3);
    for range in &doc.conflicts {
        assert!(!range.is_empty(), "every region should own rows: {range:?}");
    }
    for pair in doc.conflicts.windows(2) {
        assert!(pair[0].end <= pair[1].start, "{pair:?} overlap");
    }
    // rows outside every region are untouched by the conflict tint
    for (i, row) in doc.rows.iter().enumerate() {
        let owned = doc.conflicts.iter().any(|r| r.contains(&i));
        assert_eq!(row.conflict.is_some(), owned, "row {i}");
    }
}

#[test]
#[ignore = "requires both `mergiraf` and `difft`"]
fn two_regions_can_display_different_choices_at_the_same_time() {
    // The bug this guards: a resolved region used to paint all three sides
    // alike, so "took left" and "took right" looked identical.
    let mut session = MergeSession::new(chunks_merge().chunks, MarkerLabels::default());
    session.set_resolution(0, Resolution::Left);
    session.set_resolution(1, Resolution::Right);

    let assets = Assets::new();
    let hl = assets
        .highlighter(Some("rust"), Some(Path::new("sample.rs")), DEFAULT_THEME)
        .unwrap();
    let theme = DiffTheme::default();
    let doc = build_panes(
        &session,
        &read("chunks/base.rs"),
        &read("chunks/left.rs"),
        &read("chunks/right.rs"),
        &hl,
        &rust_differ(),
        &theme,
    );

    let check = |region: usize, chosen: Side| {
        for row in &doc.rows[doc.conflicts[region].clone()] {
            for side in Side::MERGE {
                let cell = row.cell(side);
                if cell.is_gap() {
                    continue;
                }
                if side == chosen {
                    assert_eq!(
                        cell.bg,
                        Some(theme.resolved_bg),
                        "region {region}: {side:?} was chosen and should be lit"
                    );
                } else {
                    assert_eq!(
                        cell.bg,
                        Some(theme.dimmed_bg),
                        "region {region}: {side:?} was not chosen and should be dimmed"
                    );
                    assert!(
                        cell.spans.iter().all(|s| s.fg == Some(theme.dimmed_fg)),
                        "region {region}: {side:?} should be flattened to grey"
                    );
                }
            }
        }
    };
    check(0, Side::Left);
    check(1, Side::Right);

    // and the region left undecided still offers all three
    for row in &doc.rows[doc.conflicts[2].clone()] {
        for side in Side::MERGE {
            let cell = row.cell(side);
            if !cell.is_gap() {
                assert_eq!(cell.bg, Some(theme.conflict_bg), "region 2: {side:?}");
            }
        }
    }
}

// ---- directory mode ------------------------------------------------------

mod repo_mode {
    use std::path::Path;
    use std::process::Command;

    use bigyo::external::git::Repo;
    use bigyo::merge::session::{MarkerLabels, MergeSession, Resolution};
    use bigyo::merge::workspace::{FileState, Workspace};

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A repo whose merge leaves a content conflict, an add/add, and a
    /// modify/delete — the three shapes that differ in which stages exist.
    fn conflicted_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();

        git(p, &["init", "-q", "--initial-branch=main", "."]);
        git(p, &["config", "user.email", "t@example.com"]);
        git(p, &["config", "user.name", "T"]);

        std::fs::create_dir_all(p.join("src")).unwrap();
        std::fs::write(p.join("src/a.rs"), "fn a() -> i32 {\n    1\n}\n").unwrap();
        std::fs::write(p.join("gone.rs"), "fn gone() {}\n").unwrap();
        git(p, &["add", "-A"]);
        git(p, &["commit", "-qm", "base"]);

        git(p, &["checkout", "-qb", "feature"]);
        std::fs::write(p.join("src/a.rs"), "fn a() -> i32 {\n    3\n}\n").unwrap();
        std::fs::write(p.join("gone.rs"), "fn gone() { changed(); }\n").unwrap();
        std::fs::write(p.join("added.rs"), "fn theirs() {}\n").unwrap();
        git(p, &["add", "-A"]);
        git(p, &["commit", "-qm", "theirs"]);

        git(p, &["checkout", "-q", "main"]);
        std::fs::write(p.join("src/a.rs"), "fn a() -> i32 {\n    2\n}\n").unwrap();
        std::fs::write(p.join("added.rs"), "fn ours() {}\n").unwrap();
        git(p, &["rm", "-q", "gone.rs"]);
        git(p, &["add", "-A"]);
        git(p, &["commit", "-qm", "ours"]);

        // expected to fail — that is the whole point
        let _ = Command::new("git")
            .current_dir(p)
            .args(["merge", "feature"])
            .output()
            .expect("git merge");
        dir
    }

    fn unmerged_paths(dir: &Path) -> Vec<String> {
        let out = Command::new("git")
            .current_dir(dir)
            .args(["ls-files", "-u", "--format=%(path)"])
            .output()
            .expect("git ls-files");
        let mut paths: Vec<String> = String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        paths.sort();
        paths.dedup();
        paths
    }

    #[test]
    #[ignore = "requires the `git` binary"]
    fn a_workspace_reads_every_stage_out_of_the_index() {
        let dir = conflicted_repo();
        let repo = Repo::discover(dir.path()).unwrap().expect("a repo");
        let workspace = Workspace::from_repo(&repo, None).unwrap();

        let paths: Vec<String> = workspace
            .files()
            .iter()
            .map(|f| f.path.display().to_string())
            .collect();
        assert_eq!(paths, ["added.rs", "gone.rs", "src/a.rs"]);

        let file = |name: &str| {
            workspace
                .files()
                .iter()
                .find(|f| f.path.to_str() == Some(name))
                .unwrap()
        };

        // an ordinary content conflict has all three stages
        let a = file("src/a.rs");
        assert!(a.base.contains("    1"));
        assert!(a.left.contains("    2"), "stage 2 is ours");
        assert!(a.right.contains("    3"), "stage 3 is theirs");

        // add/add: no common ancestor, so base is empty
        let added = file("added.rs");
        assert!(added.base.is_empty());
        assert!(added.left.contains("ours") && added.right.contains("theirs"));

        // modify/delete: ours deleted it, so the left side is empty
        let gone = file("gone.rs");
        assert!(!gone.base.is_empty());
        assert!(gone.left.is_empty(), "ours deleted it");
        assert!(gone.right.contains("changed"));
    }

    #[test]
    #[ignore = "requires the `git` binary"]
    fn a_pathspec_narrows_the_workspace() {
        let dir = conflicted_repo();
        let repo = Repo::discover(dir.path()).unwrap().expect("a repo");
        let workspace = Workspace::from_repo(&repo, Some(Path::new("src"))).unwrap();

        assert_eq!(workspace.len(), 1);
        assert_eq!(workspace.files()[0].path.display().to_string(), "src/a.rs");
    }

    #[test]
    #[ignore = "requires the `git` binary"]
    fn writing_and_staging_marks_the_conflict_resolved() {
        let dir = conflicted_repo();
        let repo = Repo::discover(dir.path()).unwrap().expect("a repo");
        let mut workspace = Workspace::from_repo(&repo, None).unwrap();

        assert!(unmerged_paths(dir.path()).contains(&"src/a.rs".to_string()));

        // stand in for what the app does: decide every region, write, stage
        while workspace.current().unwrap().path != Path::new("src/a.rs") {
            assert!(workspace.advance(true), "src/a.rs should be in the set");
        }
        let file = workspace.current_mut().unwrap();
        let merged = bigyo::external::mergiraf::merge_texts(
            &file.base,
            &file.left,
            &file.right,
            Some("rust"),
            &file.path,
        )
        .expect("mergiraf");
        let mut session = MergeSession::new(merged.chunks, MarkerLabels::default());
        assert!(session.conflict_count() > 0, "the fixture must conflict");
        for i in 0..session.conflict_count() {
            session.set_resolution(i, Resolution::Left);
        }
        file.session = Some(session);
        assert_eq!(file.state(), FileState::Resolved);

        let (abs, rel, content) = (
            file.abs.clone(),
            file.path.clone(),
            file.session.as_ref().unwrap().to_output(),
        );
        std::fs::write(&abs, &content).unwrap();
        repo.add(&rel).unwrap();

        // the working tree holds our choice, with no markers left behind
        let written = std::fs::read_to_string(&abs).unwrap();
        assert!(!written.contains("<<<<<<<"), "in:\n{written}");
        assert!(written.contains("    2"), "took ours:\n{written}");

        // and git no longer considers the path unmerged
        assert!(
            !unmerged_paths(dir.path()).contains(&"src/a.rs".to_string()),
            "git add should have resolved it; still unmerged: {:?}",
            unmerged_paths(dir.path())
        );
    }

    #[test]
    #[ignore = "requires the `git` binary"]
    fn discover_finds_the_root_from_a_subdirectory() {
        let dir = conflicted_repo();
        let repo = Repo::discover(&dir.path().join("src"))
            .unwrap()
            .expect("a repo");
        // macOS hands out /var symlinks for temp dirs, so compare by suffix
        assert!(
            repo.root().ends_with(dir.path().file_name().unwrap()),
            "{:?} vs {:?}",
            repo.root(),
            dir.path()
        );
        assert_eq!(Workspace::from_repo(&repo, None).unwrap().len(), 3);
    }

    #[test]
    #[ignore = "requires the `git` binary"]
    fn a_directory_without_a_repository_is_reported_as_such() {
        let dir = tempfile::tempdir().unwrap();
        // a bare temp dir is not a repo — unless one encloses it, which the
        // system temp directory never is
        assert!(Repo::discover(dir.path()).unwrap().is_none());
    }
}

// ---- diff mode -----------------------------------------------------------

mod diff_mode {
    use std::path::Path;
    use std::process::Command;

    use bigyo::external::git::Repo;
    use bigyo::merge::workspace::{FileState, Workspace};

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A repo with one commit, then a modification, an addition and a deletion
    /// left in the working tree — the three statuses `--name-status` reports.
    fn changed_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path();

        git(p, &["init", "-q", "--initial-branch=main", "."]);
        git(p, &["config", "user.email", "t@example.com"]);
        git(p, &["config", "user.name", "T"]);

        std::fs::create_dir_all(p.join("src")).unwrap();
        std::fs::write(p.join("src/a.rs"), "fn a() -> i32 {\n    1\n}\n").unwrap();
        std::fs::write(p.join("gone.rs"), "fn gone() {}\n").unwrap();
        git(p, &["add", "-A"]);
        git(p, &["commit", "-qm", "base"]);

        std::fs::write(p.join("src/a.rs"), "fn a() -> i32 {\n    2\n}\n").unwrap();
        std::fs::write(p.join("added.rs"), "fn added() {}\n").unwrap();
        std::fs::remove_file(p.join("gone.rs")).unwrap();
        dir
    }

    #[test]
    #[ignore = "requires the `git` binary"]
    fn changed_reports_every_status_against_the_working_tree() {
        let dir = changed_repo();
        let repo = Repo::discover(dir.path()).unwrap().expect("a repo");

        let mut entries = repo.changed("HEAD", None).unwrap();
        entries.sort_by_key(|e| e.path.clone());
        let summary: Vec<(String, char)> = entries
            .iter()
            .map(|e| (e.path.display().to_string(), e.status))
            .collect();
        assert_eq!(
            summary,
            [
                ("added.rs".to_string(), 'A'),
                ("gone.rs".to_string(), 'D'),
                ("src/a.rs".to_string(), 'M'),
            ]
        );
    }

    #[test]
    #[ignore = "requires the `git` binary"]
    fn show_reads_the_pre_image_and_reports_paths_a_revision_lacks() {
        let dir = changed_repo();
        let repo = Repo::discover(dir.path()).unwrap().expect("a repo");

        let committed = repo.show("HEAD", Path::new("src/a.rs")).unwrap().unwrap();
        assert_eq!(String::from_utf8(committed).unwrap(), "fn a() -> i32 {\n    1\n}\n");

        // an added file has no pre-image, which is not an error
        assert!(repo.show("HEAD", Path::new("added.rs")).unwrap().is_none());
    }

    #[test]
    #[ignore = "requires the `git` binary"]
    fn a_workspace_built_from_a_diff_pairs_each_side() {
        let dir = changed_repo();
        let repo = Repo::discover(dir.path()).unwrap().expect("a repo");
        let workspace = Workspace::from_diff(&repo, "HEAD", None).unwrap();

        let file = |name: &str| {
            workspace
                .files()
                .iter()
                .find(|f| f.path.to_str() == Some(name))
                .unwrap_or_else(|| panic!("{name} missing from {:?}", workspace.files().len()))
        };

        let modified = file("src/a.rs");
        assert_eq!(modified.state(), FileState::Modified);
        assert!(modified.left.contains("    1"), "pre-image from the commit");
        assert!(modified.right.contains("    2"), "post-image from the tree");
        assert!(modified.is_diff() && modified.session.is_none());

        let added = file("added.rs");
        assert_eq!(added.state(), FileState::Added);
        assert!(added.left.is_empty());
        assert!(added.right.contains("fn added"));

        let deleted = file("gone.rs");
        assert_eq!(deleted.state(), FileState::Deleted);
        assert!(deleted.left.contains("fn gone"));
        assert!(deleted.right.is_empty());
    }

    #[test]
    #[ignore = "requires the `git` binary"]
    fn a_pathspec_narrows_a_diff_workspace() {
        let dir = changed_repo();
        let repo = Repo::discover(dir.path()).unwrap().expect("a repo");
        let workspace = Workspace::from_diff(&repo, "HEAD", Some(Path::new("src"))).unwrap();
        assert_eq!(workspace.len(), 1);
        assert_eq!(workspace.files()[0].path.display().to_string(), "src/a.rs");
    }
}
