//! Column-aware text helpers for the title bars and gutters.
//!
//! Everything here measures in terminal columns rather than bytes or chars, so
//! CJK and other double-width text stays aligned.

use unicode_width::UnicodeWidthChar;

/// Width of `s` in terminal columns.
pub fn display_width(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Fit a path into `width` columns, truncating from the *left* with a leading
/// `…` so the filename — the part that identifies it — always survives.
pub fn fit_name(name: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if display_width(name) <= width {
        return name.to_string();
    }
    if width == 1 {
        return "…".to_string();
    }
    // Take characters from the end until they fill the space left by the `…`.
    let mut tail: Vec<char> = Vec::new();
    let mut used = 0usize;
    for ch in name.chars().rev() {
        let w = ch.width().unwrap_or(0);
        if used + w > width - 1 {
            break;
        }
        tail.push(ch);
        used += w;
    }
    tail.reverse();
    format!("…{}", tail.into_iter().collect::<String>())
}

/// How many leading `/`-separated components every one of `paths` shares.
///
/// The final component of each path is never counted, so the file name always
/// survives — the point is to drop the directories the paths agree on, which in
/// a merge are usually identical and carry no information.
pub fn common_dir_prefix(paths: &[&str]) -> usize {
    let dirs: Vec<Vec<&str>> = paths
        .iter()
        .map(|p| {
            let mut parts: Vec<&str> = p.split('/').collect();
            parts.pop(); // the file name
            parts
        })
        .collect();

    let Some((first, rest)) = dirs.split_first() else {
        return 0;
    };
    (0..first.len())
        .take_while(|&i| rest.iter().all(|d| d.get(i) == Some(&first[i])))
        .count()
}

/// Drop the first `n` `/`-separated components of `path`.
///
/// Paths shorter than `n` components are returned unchanged rather than
/// emptied.
pub fn strip_dir_prefix(path: &str, n: usize) -> &str {
    if n == 0 {
        return path;
    }
    let mut rest = path;
    for _ in 0..n {
        match rest.split_once('/') {
            Some((_, tail)) if !tail.is_empty() => rest = tail,
            _ => return rest,
        }
    }
    rest
}

/// A title-bar label like `── LEFT · left.rs `, fitted to `width` columns.
///
/// Matches the shape of the conflict headers in the merged panel, so the two
/// kinds of rule read as one family. The name is dropped entirely rather than
/// shown as a stub when the column is too narrow to say anything useful.
pub fn section_label(section: &str, name: &str, width: usize) -> String {
    let bare = format!("── {section} ");
    let overhead = display_width(&bare) + 2; // the `· ` joining name to section
    match width.checked_sub(overhead) {
        Some(room) if room >= 3 && !name.is_empty() => {
            format!("── {section} · {} ", fit_name(name, room - 1))
        }
        _ => bare,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_names_are_left_alone() {
        assert_eq!(fit_name("left.rs", 20), "left.rs");
        assert_eq!(fit_name("left.rs", 7), "left.rs");
    }

    #[test]
    fn long_names_keep_their_tail() {
        // the filename is the informative part, so it must survive
        let fitted = fit_name("tests/fixtures/chunks/left.rs", 12);
        assert_eq!(fitted, "…nks/left.rs");
        assert!(fitted.starts_with('…'));
        assert!(fitted.ends_with("left.rs"), "got {fitted:?}");
        assert_eq!(display_width(&fitted), 12);
    }

    #[test]
    fn fitting_never_exceeds_the_width() {
        let name = "a/very/long/path/to/some/file.rs";
        for width in 0..40 {
            assert!(
                display_width(&fit_name(name, width)) <= width,
                "width {width} overflowed"
            );
        }
    }

    #[test]
    fn degenerate_widths_do_not_panic() {
        assert_eq!(fit_name("left.rs", 0), "");
        assert_eq!(fit_name("left.rs", 1), "…");
        assert_eq!(fit_name("", 10), "");
    }

    #[test]
    fn double_width_names_are_measured_in_columns() {
        // each CJK char is two columns wide: 3*2 + 1 + 2*2 + 3
        let name = "테스트/파일.rs";
        assert_eq!(display_width(name), 14);
        for width in 0..20 {
            assert!(
                display_width(&fit_name(name, width)) <= width,
                "width {width}"
            );
        }
        // and truncation never splits a character
        let fitted = fit_name(name, 8);
        assert!(
            name.ends_with(fitted.trim_start_matches('…')),
            "got {fitted:?}"
        );
    }

    #[test]
    fn shared_directories_are_counted_but_never_the_file_name() {
        let paths = [
            "tests/fixtures/chunks/base.rs",
            "tests/fixtures/chunks/left.rs",
            "tests/fixtures/chunks/right.rs",
        ];
        assert_eq!(common_dir_prefix(&paths), 3);
        for p in paths {
            assert!(!strip_dir_prefix(p, 3).contains('/'));
        }
        assert_eq!(strip_dir_prefix(paths[0], 3), "base.rs");
    }

    #[test]
    fn paths_that_diverge_share_only_what_they_agree_on() {
        assert_eq!(common_dir_prefix(&["a/b/x.rs", "a/c/y.rs"]), 1);
        assert_eq!(strip_dir_prefix("a/b/x.rs", 1), "b/x.rs");
        assert_eq!(common_dir_prefix(&["a/x.rs", "b/y.rs"]), 0);
        assert_eq!(common_dir_prefix(&["x.rs", "y.rs"]), 0);
    }

    #[test]
    fn identical_paths_still_keep_their_file_name() {
        assert_eq!(common_dir_prefix(&["a/b/x.rs", "a/b/x.rs"]), 2);
        assert_eq!(strip_dir_prefix("a/b/x.rs", 2), "x.rs");
    }

    #[test]
    fn absolute_paths_share_their_leading_slash() {
        // splitting "/tmp/w/base.rs" yields a leading empty component
        assert_eq!(common_dir_prefix(&["/tmp/w/base.rs", "/tmp/w/left.rs"]), 3);
        assert_eq!(strip_dir_prefix("/tmp/w/base.rs", 3), "base.rs");
    }

    #[test]
    fn stripping_more_than_a_path_has_leaves_it_alone() {
        assert_eq!(strip_dir_prefix("x.rs", 3), "x.rs");
        assert_eq!(strip_dir_prefix("a/x.rs", 9), "x.rs");
        assert_eq!(strip_dir_prefix("", 2), "");
        assert_eq!(strip_dir_prefix("a/x.rs", 0), "a/x.rs");
    }

    #[test]
    fn an_empty_path_list_shares_nothing() {
        assert_eq!(common_dir_prefix(&[]), 0);
    }

    #[test]
    fn a_label_carries_the_section_and_the_name() {
        assert_eq!(section_label("LEFT", "left.rs", 40), "── LEFT · left.rs ");
    }

    #[test]
    fn a_label_truncates_the_name_before_the_section() {
        let label = section_label("LEFT", "tests/fixtures/chunks/left.rs", 24);
        assert!(label.starts_with("── LEFT · "), "got {label:?}");
        assert!(label.contains("left.rs"), "got {label:?}");
        assert!(display_width(&label) <= 24, "got {label:?}");
    }

    #[test]
    fn a_label_drops_the_name_when_there_is_no_room_for_it() {
        assert_eq!(section_label("LEFT", "left.rs", 10), "── LEFT ");
        assert_eq!(section_label("MERGED", "out.rs", 4), "── MERGED ");
        assert_eq!(section_label("LEFT", "", 40), "── LEFT ");
    }

    #[test]
    fn a_label_fits_its_width_whenever_it_can() {
        for width in 0..50 {
            let label = section_label("LEFT", "some/path/left.rs", width);
            // the bare label is the floor: it is never truncated mid-section
            assert!(label.starts_with("── LEFT "), "width {width}: {label:?}");
            assert!(
                display_width(&label) <= width.max(display_width("── LEFT ")),
                "width {width}: {label:?}"
            );
        }
    }
}
