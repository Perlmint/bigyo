//! The ratatui viewer: LEFT | BASE | RIGHT over the merged result, with the
//! resolution keys that decide each conflict region.

use std::path::PathBuf;

use anyhow::{Context, Result};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Widget};
use ratatui::{DefaultTerminal, Frame};
use unicode_width::UnicodeWidthChar;

use crate::external::git::Repo;
use crate::external::mergiraf;
use crate::merge::session::{MergeSession, Resolution};
use crate::merge::workspace::{FileEntry, FileState, Workspace, repo_marker_labels};
use crate::render::document::{DisplayRow, RowKind};
use crate::render::highlight::Assets;
use crate::render::panes::Cell;
use crate::render::span::StyledSpan;
use crate::render::text::{display_width, fit_name, section_label};
use crate::render::theme::{DiffTheme, Side};
use crate::tui::file_view::FileView;

/// Which panel `j`/`k` and the horizontal scroll act on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Focus {
    /// The three revision panes across the top.
    #[default]
    Panes,
    /// The merged result being assembled below them.
    Merged,
}

impl Focus {
    fn index(self) -> usize {
        match self {
            Self::Panes => 0,
            Self::Merged => 1,
        }
    }

    fn toggled(self) -> Self {
        match self {
            Self::Panes => Self::Merged,
            Self::Merged => Self::Panes,
        }
    }
}

/// Percentage of the body height given to the top panes.
const DEFAULT_SPLIT: u16 = 60;
const MIN_SPLIT: u16 = 20;
const MAX_SPLIT: u16 = 85;

/// Which screen is on show.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Screen {
    /// The three revisions over the merged result.
    #[default]
    Merge,
    /// The list of conflicted files, in directory mode.
    Files,
    /// The list of languages to force on the open file.
    Language,
}

/// The picker's first row, which clears the override.
const AUTO_DETECT: &str = "(auto-detect)";

/// Where a resolved file is written, and whether git is told about it.
pub enum Destination {
    /// Single-file mode: `-o` names the file, or nothing is written.
    Path(Option<PathBuf>),
    /// Directory mode: each file writes to its own working-tree path, and a
    /// fully resolved one is staged.
    Repo(Repo),
}

pub struct App<'a> {
    assets: &'a Assets,
    workspace: Workspace,
    view: FileView<'a>,
    screen: Screen,
    /// Row highlighted in the file list, which is not yet the open file.
    file_cursor: usize,
    /// Row highlighted in the language list, and what is filtering it.
    language_cursor: usize,
    language_filter: String,
    /// A language change waiting on confirmation, because applying it would
    /// discard resolutions. `Some(None)` is a pending switch to auto-detect.
    pending_language: Option<Option<String>>,
    focus: Focus,
    theme: DiffTheme,
    theme_name: String,
    title: String,
    destination: Destination,
    /// Vertical offset per panel, indexed by [`Focus::index`].
    scroll: [usize; 2],
    hscroll: [usize; 2],
    /// The conflict region the resolution keys act on.
    selected: usize,
    split: u16,
    /// Rows visible in each panel, updated on draw.
    viewport: [usize; 2],
    status: Option<String>,
    quit: bool,
}

impl<'a> App<'a> {
    /// Open the workspace's current file and build the app around it.
    pub fn new(
        assets: &'a Assets,
        mut workspace: Workspace,
        theme: DiffTheme,
        theme_name: String,
        destination: Destination,
    ) -> Result<Self> {
        let view = open_current(assets, &mut workspace, &theme, &theme_name)?;
        let title = workspace
            .current()
            .map_or_else(String::new, |f| f.path.display().to_string());
        let file_cursor = workspace.current_index();

        Ok(Self {
            assets,
            workspace,
            view,
            screen: Screen::default(),
            file_cursor,
            language_cursor: 0,
            language_filter: String::new(),
            pending_language: None,
            focus: Focus::default(),
            theme,
            theme_name,
            title,
            destination,
            scroll: [0, 0],
            hscroll: [0, 0],
            selected: 0,
            split: DEFAULT_SPLIT,
            viewport: [1, 1],
            status: None,
            quit: false,
        })
    }

    pub fn run(mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        while !self.quit {
            terminal.draw(|frame| self.draw(frame))?;
            if let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                self.on_key(key);
            }
        }
        Ok(())
    }

    /// The session of the file currently open, if it is a merge.
    fn session(&self) -> Option<&MergeSession> {
        self.workspace.current().and_then(|f| f.session.as_ref())
    }

    /// True when the open file is a diff, which has no merged panel and
    /// nothing to resolve.
    fn is_diff(&self) -> bool {
        self.workspace.current().is_some_and(|f| f.is_diff())
    }

    /// How many conflict regions the open file has; zero for a diff.
    fn conflict_count(&self) -> usize {
        self.session().map_or(0, MergeSession::conflict_count)
    }

    fn resolved_count(&self) -> usize {
        self.session().map_or(0, MergeSession::resolved_count)
    }

    fn resolution(&self) -> Resolution {
        self.session()
            .map_or(Resolution::Unresolved, |s| s.resolution(self.selected))
    }

    /// Load the file the workspace is pointing at, replacing the view.
    fn reload(&mut self) {
        match open_current(
            self.assets,
            &mut self.workspace,
            &self.theme,
            &self.theme_name,
        ) {
            Ok(view) => {
                self.view = view;
                self.title = self
                    .workspace
                    .current()
                    .map_or_else(String::new, |f| f.path.display().to_string());
                self.selected = 0;
                self.scroll = [0, 0];
                self.hscroll = [0, 0];
            }
            Err(e) => self.status = Some(format!("could not open file: {e}")),
        }
    }

    /// Move to another file, if there is one that way.
    fn change_file(&mut self, forward: bool) {
        if !self.workspace.advance(forward) {
            self.status = Some(if forward { "last file" } else { "first file" }.to_string());
            return;
        }
        self.file_cursor = self.workspace.current_index();
        self.reload();
    }

    /// Gutter width: badge + right-aligned number + separator.
    fn gutter_width(&self) -> usize {
        1 + self.view.gutter_digits + 2
    }

    fn row_count(&self, focus: Focus) -> usize {
        match focus {
            Focus::Panes => self.view.panes.len(),
            Focus::Merged => self.view.doc.len(),
        }
    }

    fn max_scroll(&self, focus: Focus) -> usize {
        self.row_count(focus)
            .saturating_sub(self.viewport[focus.index()])
    }

    fn set_scroll(&mut self, focus: Focus, value: usize) {
        self.scroll[focus.index()] = value.min(self.max_scroll(focus));
    }

    fn scroll_by(&mut self, delta: isize) {
        let focus = self.focus;
        let target = self.scroll[focus.index()] as isize + delta;
        self.set_scroll(focus, target.max(0) as usize);
    }

    fn on_key(&mut self, key: KeyEvent) {
        self.status = None;
        match self.screen {
            Screen::Files => return self.on_files_key(key),
            Screen::Language => return self.on_language_key(key),
            Screen::Merge => {}
        }

        // A pending confirmation owns enter and esc, which would otherwise
        // do nothing and quit.
        if self.pending_language.is_some() {
            match key.code {
                KeyCode::Enter => {
                    let choice = self.pending_language.take().flatten();
                    self.apply_language(choice);
                }
                KeyCode::Esc => {
                    self.pending_language = None;
                    self.status = Some("kept the current merge".into());
                }
                _ => self.restate_pending(),
            }
            return;
        }

        let page = self.viewport[self.focus.index()].saturating_sub(2).max(1);
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,

            KeyCode::Char('f') if self.workspace.len() > 1 => {
                self.file_cursor = self.workspace.current_index();
                self.screen = Screen::Files;
            }
            KeyCode::Char('L') => {
                self.language_filter.clear();
                self.language_cursor = 0;
                self.screen = Screen::Language;
            }
            KeyCode::Char(']') => self.change_file(true),
            KeyCode::Char('[') => self.change_file(false),

            KeyCode::Tab | KeyCode::BackTab if !self.is_diff() => self.focus = self.focus.toggled(),

            KeyCode::Char('j') | KeyCode::Down => self.scroll_by(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_by(-1),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_by(page as isize),
            KeyCode::PageUp => self.scroll_by(-(page as isize)),
            KeyCode::Char('g') | KeyCode::Home => self.set_scroll(self.focus, 0),
            KeyCode::Char('G') | KeyCode::End => {
                self.set_scroll(self.focus, self.max_scroll(self.focus))
            }

            KeyCode::Char('l') | KeyCode::Right => self.hscroll[self.focus.index()] += 4,
            KeyCode::Char('h') | KeyCode::Left => {
                let i = self.focus.index();
                self.hscroll[i] = self.hscroll[i].saturating_sub(4);
            }
            KeyCode::Char('0') => self.hscroll[self.focus.index()] = 0,

            KeyCode::Char('n') => self.select_region(self.selected.saturating_add(1)),
            KeyCode::Char('p') => self.select_region(self.selected.saturating_sub(1)),

            KeyCode::Char('1') => self.resolve(Resolution::Left),
            KeyCode::Char('2') => self.resolve(Resolution::Base),
            KeyCode::Char('3') => self.resolve(Resolution::Right),
            KeyCode::Char('b') => self.resolve(Resolution::BothLeftFirst),
            KeyCode::Char('B') => self.resolve(Resolution::BothRightFirst),
            KeyCode::Char('u') => self.resolve(Resolution::Unresolved),

            KeyCode::Char('w') => self.write_output(),

            KeyCode::Char('+') | KeyCode::Char('=') => self.split = (self.split + 5).min(MAX_SPLIT),
            KeyCode::Char('-') => self.split = self.split.saturating_sub(5).max(MIN_SPLIT),
            _ => {}
        }
    }

    /// Keys for the file list.
    fn on_files_key(&mut self, key: KeyEvent) {
        let last = self.workspace.len().saturating_sub(1);
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,

            KeyCode::Esc | KeyCode::Char('f') => self.screen = Screen::Merge,

            KeyCode::Char('j') | KeyCode::Down => {
                self.file_cursor = (self.file_cursor + 1).min(last)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.file_cursor = self.file_cursor.saturating_sub(1)
            }
            KeyCode::Char('g') | KeyCode::Home => self.file_cursor = 0,
            KeyCode::Char('G') | KeyCode::End => self.file_cursor = last,

            KeyCode::Enter => {
                if self.file_cursor == self.workspace.current_index() {
                    self.screen = Screen::Merge;
                } else if self.workspace.select(self.file_cursor) {
                    self.reload();
                    self.screen = Screen::Merge;
                } else {
                    self.status = Some("that file cannot be opened".into());
                }
            }
            _ => {}
        }
    }

    /// Languages matching the filter, `(auto-detect)` always first.
    fn language_choices(&self) -> Vec<&str> {
        let needle = self.language_filter.to_lowercase();
        let mut out = vec![AUTO_DETECT];
        out.extend(
            self.assets
                .syntax_names()
                .into_iter()
                .filter(|name| needle.is_empty() || name.to_lowercase().contains(&needle)),
        );
        out
    }

    fn on_language_key(&mut self, key: KeyEvent) {
        let last = self.language_choices().len().saturating_sub(1);
        match key.code {
            KeyCode::Esc => self.screen = Screen::Merge,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => self.quit = true,

            KeyCode::Down => self.language_cursor = (self.language_cursor + 1).min(last),
            KeyCode::Up => self.language_cursor = self.language_cursor.saturating_sub(1),
            KeyCode::Home => self.language_cursor = 0,
            KeyCode::End => self.language_cursor = last,

            KeyCode::Backspace => {
                self.language_filter.pop();
                self.language_cursor = 0;
            }
            KeyCode::Char(c) => {
                self.language_filter.push(c);
                self.language_cursor = 0;
            }

            KeyCode::Enter => {
                let choice = self
                    .language_choices()
                    .get(self.language_cursor)
                    .map(|name| (*name != AUTO_DETECT).then(|| (*name).to_owned()));
                if let Some(choice) = choice {
                    self.screen = Screen::Merge;
                    self.request_language(choice);
                }
            }
            _ => {}
        }
    }

    /// Apply a language, or ask first when doing so would discard choices.
    fn request_language(&mut self, choice: Option<String>) {
        let resolved = self
            .workspace
            .current()
            .filter(|f| !f.is_diff())
            .and_then(|f| f.session.as_ref())
            .map_or(0, MergeSession::resolved_count);

        if resolved == 0 {
            self.apply_language(choice);
            return;
        }
        // Re-merging rebuilds the chunks, so the choices cannot survive it.
        self.pending_language = Some(choice);
        self.restate_pending();
    }

    fn restate_pending(&mut self) {
        let Some(choice) = &self.pending_language else {
            return;
        };
        let name = choice.clone().unwrap_or_else(|| "auto-detect".into());
        let resolved = self.resolved_count();
        self.status = Some(format!(
            "re-merging as {name} discards {resolved} choice{} — enter to confirm, esc to cancel",
            if resolved == 1 { "" } else { "s" }
        ));
    }

    fn apply_language(&mut self, choice: Option<String>) {
        let Some(file) = self.workspace.current_mut() else {
            return;
        };
        file.language = choice;
        let name = file
            .effective_language()
            .map_or_else(|| "auto-detect".to_string(), str::to_owned);
        // A merge has to be redone by mergiraf, not just recoloured.
        if !file.is_diff() {
            file.session = None;
            file.saved = false;
        }

        self.reload();
        if self.status.is_none() {
            self.status = Some(format!("language: {name}"));
        }
    }

    /// Move the region cursor and bring that region into view in *both* panels.
    ///
    /// A diff has hunks rather than conflict regions, so the same keys step
    /// through those instead.
    fn select_region(&mut self, index: usize) {
        if self.is_diff() {
            let hunks = self.view.panes.hunks.clone();
            if hunks.is_empty() {
                self.status = Some("no changes".into());
                return;
            }
            self.selected = index.min(hunks.len() - 1);
            self.set_scroll(Focus::Panes, hunks[self.selected]);
            return;
        }

        let total = self.conflict_count();
        if total == 0 {
            self.status = Some("no conflicts".into());
            return;
        }
        self.selected = index.min(total - 1);

        if let Some(range) = self.view.panes.conflicts.get(self.selected)
            && !range.is_empty()
        {
            self.set_scroll(Focus::Panes, range.start);
        }
        if let Some(&row) = self.view.doc.conflicts.get(self.selected) {
            self.set_scroll(Focus::Merged, row);
        }
    }

    fn resolve(&mut self, resolution: Resolution) {
        if self.conflict_count() == 0 {
            self.status = Some("no conflicts to resolve".into());
            return;
        }
        let selected = self.selected;
        let file = self.workspace.current_mut().expect("a file is open");
        file.saved = false;
        let Some(session) = file.session.as_mut() else {
            self.status = Some("a diff has nothing to resolve".into());
            return;
        };
        session.set_resolution(selected, resolution);

        // `view` and `workspace` are separate fields, so the session can be
        // borrowed to re-render without conflicting with the mutation above.
        let session = self.workspace.current().unwrap().session.clone().unwrap();
        self.view.rebuild(&session, &self.theme);
        // Keep the cursor on the same region, whose rows have just moved.
        self.select_region(selected);
    }

    fn write_output(&mut self) {
        let Some(session) = self.session() else {
            self.status = Some("a diff has nothing to write".into());
            return;
        };
        let unresolved = session.conflict_count() - session.resolved_count();
        let content = session.to_output();

        let (path, stage) = match &self.destination {
            Destination::Path(None) => {
                self.status = Some("no --output given; nothing written".into());
                return;
            }
            Destination::Path(Some(path)) => (path.clone(), None),
            Destination::Repo(repo) => {
                let file = self.workspace.current().expect("a file is open");
                // Only a fully decided file is staged: the index must never
                // claim a half-finished merge is done.
                let stage = (unresolved == 0).then(|| (repo, file.path.clone()));
                (file.abs.clone(), stage)
            }
        };

        if let Err(e) = std::fs::write(&path, content) {
            self.status = Some(format!("could not write {}: {e}", path.display()));
            return;
        }

        let name = path.display().to_string();
        self.status = Some(match stage {
            Some((repo, relative)) => match repo.add(&relative) {
                Ok(()) => {
                    if let Some(file) = self.workspace.current_mut() {
                        file.saved = true;
                    }
                    format!("wrote and staged {}", relative.display())
                }
                Err(e) => format!("wrote {name}, but git add failed: {e}"),
            },
            None if unresolved == 0 => format!("wrote {name}"),
            None => format!("wrote {name} ({unresolved} unresolved left as conflict markers)"),
        });
    }

    fn draw(&mut self, frame: &mut Frame) {
        if self.screen != Screen::Merge {
            // Paragraph leaves cells it does not write, so the merge screen
            // would otherwise show through around a shorter list.
            Clear.render(frame.area(), frame.buffer_mut());
            let [body, status] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());
            match self.screen {
                Screen::Files => self.draw_files(frame, body),
                Screen::Language => self.draw_languages(frame, body),
                Screen::Merge => unreachable!(),
            }
            self.draw_status(frame, status);
            return;
        }

        // A diff has only the two panes, so it gets the whole body and none of
        // the merge screen's split, merged panel or focus handling.
        if self.is_diff() {
            let [body, status] =
                Layout::vertical([Constraint::Min(2), Constraint::Length(1)]).areas(frame.area());
            let [pane_titles, panes] =
                Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(body);

            self.focus = Focus::Panes;
            self.viewport = [panes.height as usize, 0];
            self.set_scroll(Focus::Panes, self.scroll[0]);

            self.draw_pane_titles(frame, pane_titles);
            self.draw_panes(frame, panes);
            self.draw_status(frame, status);
            return;
        }

        let [body, status] =
            Layout::vertical([Constraint::Min(4), Constraint::Length(1)]).areas(frame.area());
        let [pane_titles, panes, merged_title, merged] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Percentage(self.split),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .areas(body);

        self.viewport = [panes.height as usize, merged.height as usize];
        self.set_scroll(Focus::Panes, self.scroll[0]);
        self.set_scroll(Focus::Merged, self.scroll[1]);

        self.draw_pane_titles(frame, pane_titles);
        self.draw_panes(frame, panes);
        self.draw_merged_title(frame, merged_title);
        self.draw_merged(frame, merged);
        self.draw_status(frame, status);
    }

    /// The conflicted-file list: one row per file, path and state, nothing else.
    fn draw_files(&self, frame: &mut Frame, area: Rect) {
        let width = area.width as usize;
        let header = section_label(
            "FILES",
            &format!(
                "{}/{} resolved",
                self.workspace.resolved_count(),
                self.workspace.len()
            ),
            width,
        );
        let mut lines = vec![Line::from(vec![
            styled(header.clone(), Some(self.theme.focus_fg), self.view.page_bg),
            styled(
                "─".repeat(width.saturating_sub(display_width(&header))),
                Some(self.theme.gutter_fg),
                self.view.page_bg,
            ),
        ])];

        // Keep the cursor on screen when there are more files than rows.
        let rows = (area.height as usize).saturating_sub(1);
        let first = self.file_cursor.saturating_sub(rows.saturating_sub(1));

        for (i, file) in self
            .workspace
            .files()
            .iter()
            .enumerate()
            .skip(first)
            .take(rows)
        {
            let state = file.state();
            let open = i == self.workspace.current_index();
            let marker = match (i == self.file_cursor, open) {
                (true, _) => "▸ ",
                (false, true) => "· ",
                (false, false) => "  ",
            };
            let label = state.label();
            // The state word is right-aligned, so the eye can scan one column.
            let room = width.saturating_sub(display_width(marker) + label.len() + 3);
            let path = fit_name(&file.path.display().to_string(), room);
            let gap = width
                .saturating_sub(display_width(marker) + display_width(&path) + label.len() + 1);

            lines.push(Line::from(vec![
                styled(
                    marker.to_string(),
                    Some(self.theme.focus_fg),
                    self.view.page_bg,
                ),
                styled(
                    path,
                    Some(if open {
                        self.theme.focus_fg
                    } else {
                        self.theme.status_fg
                    }),
                    self.view.page_bg,
                ),
                styled(" ".repeat(gap), None, self.view.page_bg),
                styled(
                    label.to_string(),
                    Some(self.state_color(state)),
                    self.view.page_bg,
                ),
            ]));
        }

        let mut para = Paragraph::new(lines);
        if let Some(bg) = self.view.page_bg {
            para = para.style(Style::default().bg(bg));
        }
        para.render(area, frame.buffer_mut());
    }

    /// The language list: what is in effect, and what you can force instead.
    fn draw_languages(&self, frame: &mut Frame, area: Rect) {
        let width = area.width as usize;
        // `current` is what a row is marked against — an explicit choice, or
        // `(auto-detect)`. The header says what is actually in use, which with
        // nothing chosen is whatever detection landed on.
        let current = self
            .workspace
            .current()
            .and_then(FileEntry::effective_language)
            .unwrap_or(AUTO_DETECT);
        let in_use = if current == AUTO_DETECT {
            format!("{} (detected)", self.view.syntax)
        } else {
            current.to_owned()
        };
        let header = section_label("LANGUAGE", &in_use, width);

        let mut lines = vec![Line::from(vec![
            styled(header.clone(), Some(self.theme.focus_fg), self.view.page_bg),
            styled(
                "─".repeat(width.saturating_sub(display_width(&header))),
                Some(self.theme.gutter_fg),
                self.view.page_bg,
            ),
        ])];

        // One row goes to the filter, so you can see what you have typed.
        let filter = format!(" / {}", self.language_filter);
        lines.push(Line::from(styled(
            fit_name(&filter, width),
            Some(self.theme.status_fg),
            self.view.page_bg,
        )));

        let choices = self.language_choices();
        let rows = (area.height as usize).saturating_sub(2);
        let first = self.language_cursor.saturating_sub(rows.saturating_sub(1));

        for (i, name) in choices.iter().enumerate().skip(first).take(rows) {
            let in_effect = *name == current;
            let marker = match (i == self.language_cursor, in_effect) {
                (true, _) => "▸ ",
                (false, true) => "· ",
                (false, false) => "  ",
            };
            lines.push(Line::from(vec![
                styled(
                    marker.to_string(),
                    Some(self.theme.focus_fg),
                    self.view.page_bg,
                ),
                styled(
                    fit_name(name, width.saturating_sub(2)),
                    Some(if in_effect {
                        self.theme.focus_fg
                    } else {
                        self.theme.status_fg
                    }),
                    self.view.page_bg,
                ),
            ]));
        }

        let mut para = Paragraph::new(lines);
        if let Some(bg) = self.view.page_bg {
            para = para.style(Style::default().bg(bg));
        }
        para.render(area, frame.buffer_mut());
    }

    fn state_color(&self, state: FileState) -> Color {
        match state {
            FileState::Conflict | FileState::Modified => self.theme.marker_fg,
            FileState::Resolved | FileState::Added => self.theme.resolved_fg,
            FileState::Deleted => self.theme.conflict_selected_bg,
            FileState::Binary => self.theme.dimmed_fg,
        }
    }

    /// Names the file behind each revision column.
    fn draw_pane_titles(&self, frame: &mut Frame, area: Rect) {
        let sides = self.view.panes.columns.clone();
        let columns = pane_columns(area, sides.len());
        let focused = self.focus == Focus::Panes;

        for (position, side) in sides.iter().copied().enumerate() {
            let column = columns[position];
            let is_last = position + 1 == sides.len();
            let width = (column.width as usize).saturating_sub(usize::from(!is_last));

            // The focus marker goes on the last column, matching the merged bar.
            let marker = if focused && is_last { "◂ " } else { "" };
            let label = section_label(
                side.label(self.is_diff()),
                self.view.names.side(side),
                width.saturating_sub(display_width(marker)),
            );

            let mut spans = vec![styled(
                label.clone(),
                Some(if focused {
                    self.theme.focus_fg
                } else {
                    self.theme.gutter_fg
                }),
                self.view.page_bg,
            )];
            let used = display_width(&label) + display_width(marker);
            if !marker.is_empty() {
                spans.push(styled(
                    marker.to_string(),
                    Some(self.theme.focus_fg),
                    self.view.page_bg,
                ));
            }
            if used < width {
                spans.push(styled(
                    "─".repeat(width - used),
                    Some(self.theme.gutter_fg),
                    self.view.page_bg,
                ));
            }
            if !is_last {
                spans.push(styled(
                    "┬".to_string(),
                    Some(self.theme.gutter_fg),
                    self.view.page_bg,
                ));
            }
            Paragraph::new(Line::from(spans)).render(column, frame.buffer_mut());
        }
    }

    /// The bar separating the revisions above from the result below.
    fn draw_merged_title(&self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Merged;
        let fg = if focused {
            self.theme.focus_fg
        } else {
            self.theme.gutter_fg
        };
        let marker = if focused { "◂ " } else { "" };
        let label = format!(
            "{}{marker}",
            section_label(
                "MERGED",
                &self.view.names.merged,
                (area.width as usize).saturating_sub(display_width(marker))
            )
        );
        let fill = (area.width as usize).saturating_sub(display_width(&label));
        Paragraph::new(Line::from(vec![
            styled(label, Some(fg), self.view.page_bg),
            styled(
                "─".repeat(fill),
                Some(self.theme.gutter_fg),
                self.view.page_bg,
            ),
        ]))
        .render(area, frame.buffer_mut());
    }

    fn draw_merged(&self, frame: &mut Frame, area: Rect) {
        let scroll = self.scroll[Focus::Merged.index()];
        let hscroll = self.hscroll[Focus::Merged.index()];
        let gutter = self.gutter_width();
        let text_width = (area.width as usize).saturating_sub(gutter);

        let lines: Vec<Line<'static>> = self
            .view
            .doc
            .rows
            .iter()
            .skip(scroll)
            .take(area.height as usize)
            .map(|row| match row.kind {
                RowKind::ConflictOpen | RowKind::ConflictClose => {
                    self.separator_line(row, area.width as usize)
                }
                _ => {
                    let mut spans = self.gutter_spans(row.kind, row.line_no);
                    spans.extend(clip_spans(&row.spans, hscroll, text_width));
                    let bg = row.row_bg.or(self.view.page_bg);
                    // Pad to the full width so a short or blank line inside a
                    // conflict block still reads as part of that block.
                    let used: usize = spans.iter().map(|s| display_width(&s.content)).sum();
                    if used < area.width as usize {
                        spans.push(styled(" ".repeat(area.width as usize - used), None, bg));
                    }
                    Line::from(spans)
                }
            })
            .collect();

        let mut para = Paragraph::new(lines);
        if let Some(bg) = self.view.page_bg {
            para = para.style(Style::default().bg(bg));
        }
        para.render(area, frame.buffer_mut());
    }

    fn gutter_spans(&self, kind: RowKind, line_no: Option<usize>) -> Vec<Span<'static>> {
        let (badge, badge_fg) = match kind {
            RowKind::Side(side) => (
                DiffTheme::side_badge(side),
                Some(match side {
                    Side::Left => self.theme.left_emph_bg,
                    Side::Base => self.theme.marker_fg,
                    Side::Right => self.theme.right_emph_bg,
                }),
            ),
            // A decided region is badged with the side it took.
            RowKind::Resolved(side) => (DiffTheme::side_badge(side), Some(self.theme.resolved_fg)),
            _ => (' ', None),
        };
        let num = line_no.map_or_else(String::new, |n| n.to_string());
        vec![
            styled(badge.to_string(), badge_fg, self.view.page_bg),
            styled(
                format!("{num:>width$} ", width = self.view.gutter_digits),
                Some(self.theme.gutter_fg),
                self.view.page_bg,
            ),
            styled(
                "│".to_string(),
                Some(self.theme.gutter_fg),
                self.view.page_bg,
            ),
        ]
    }

    fn separator_line(&self, row: &DisplayRow, width: usize) -> Line<'static> {
        let selected = row.conflict == Some(self.selected);
        let label = match &row.header {
            Some(header) => format!("{header}{} ", if selected { "◂" } else { "" }),
            None => "── END ".to_string(),
        };
        let fg = if selected {
            self.theme.focus_fg
        } else {
            self.theme.marker_fg
        };
        let fill = width.saturating_sub(display_width(&label));
        Line::from(vec![
            styled(label, Some(fg), self.view.page_bg),
            styled(
                "─".repeat(fill),
                Some(self.theme.gutter_fg),
                self.view.page_bg,
            ),
        ])
    }

    /// Draw LEFT | BASE | RIGHT as three equal columns, aligned row for row.
    ///
    /// Horizontal scroll is deliberately shared across the columns: scrolling
    /// them independently would destroy the row correspondence that is the
    /// whole point of the view. For the same reason lines are clipped rather
    /// than wrapped.
    fn draw_panes(&self, frame: &mut Frame, area: Rect) {
        let scroll = self.scroll[Focus::Panes.index()];
        let hscroll = self.hscroll[Focus::Panes.index()];
        let selected = self.view.panes.conflicts.get(self.selected).cloned();
        let sides = self.view.panes.columns.clone();
        let columns = pane_columns(area, sides.len());

        for (position, side) in sides.iter().copied().enumerate() {
            let column = columns[position];
            // Every column but the last gives up its rightmost cell to a divider.
            let is_last = position + 1 == sides.len();
            let content_width = (column.width as usize).saturating_sub(usize::from(!is_last));
            let gutter = (self.view.pane_digits + 2).min(content_width);
            let text_width = content_width.saturating_sub(gutter);

            let lines: Vec<Line<'static>> = self
                .view
                .panes
                .rows
                .iter()
                .enumerate()
                .skip(scroll)
                .take(column.height as usize)
                .map(|(index, row)| {
                    let cell = row.cell(side);
                    // The region under the cursor reads brighter than the rest.
                    let in_selection = selected.as_ref().is_some_and(|r| r.contains(&index));
                    let bg = match cell.bg {
                        Some(_)
                            if in_selection
                                && !cell.is_gap()
                                && !self.resolution().is_resolved() =>
                        {
                            Some(self.theme.conflict_selected_bg)
                        }
                        other => other.or(self.view.page_bg),
                    };
                    let mut spans = self.pane_gutter(cell, gutter);
                    let cells = if in_selection && !cell.is_gap() {
                        recolor(&cell.spans, bg)
                    } else {
                        cell.spans.clone()
                    };
                    spans.extend(clip_spans(&cells, hscroll, text_width));

                    let used: usize = spans.iter().map(|s| display_width(&s.content)).sum();
                    if used < content_width {
                        spans.push(styled(" ".repeat(content_width - used), None, bg));
                    }
                    if !is_last {
                        spans.push(styled(
                            "│".to_string(),
                            Some(self.theme.gutter_fg),
                            self.view.page_bg,
                        ));
                    }
                    Line::from(spans)
                })
                .collect();

            let mut para = Paragraph::new(lines);
            if let Some(bg) = self.view.page_bg {
                para = para.style(Style::default().bg(bg));
            }
            para.render(column, frame.buffer_mut());
        }
    }

    /// A pane's line number, or `~` where that file has no line at this row.
    fn pane_gutter(&self, cell: &Cell, width: usize) -> Vec<Span<'static>> {
        if width == 0 {
            return Vec::new();
        }
        let label = match cell.line_no {
            Some(n) => format!("{n:>digits$} ", digits = self.view.pane_digits),
            None => format!("{:>digits$} ", "~", digits = self.view.pane_digits),
        };
        vec![styled(label, Some(self.theme.gutter_fg), self.view.page_bg)]
    }

    fn draw_status(&self, frame: &mut Frame, area: Rect) {
        let total = self.conflict_count();
        // Only worth naming the file's place when there is more than one.
        let file = if self.workspace.len() > 1 {
            format!(
                "{} [{}/{}] [{}]",
                self.title,
                self.workspace.current_index() + 1,
                self.workspace.len(),
                self.view.syntax,
            )
        } else {
            format!("{} [{}]", self.title, self.view.syntax)
        };
        let left = match &self.status {
            Some(message) => format!(" {message} "),
            None if self.screen == Screen::Language => format!(
                " {} languages ",
                self.language_choices().len().saturating_sub(1)
            ),
            None if self.screen == Screen::Files => format!(
                " {} files  ·  {} resolved ",
                self.workspace.len(),
                self.workspace.resolved_count()
            ),
            None if self.is_diff() => {
                let hunks = self.view.panes.hunks.len();
                if hunks == 0 {
                    format!(" {file}  no changes ")
                } else {
                    format!(" {file}  hunk {}/{hunks} ", self.selected + 1)
                }
            }
            None if total == 0 => format!(" {file}  no conflicts "),
            None => format!(
                " {file}  resolved {}/{}  ·  conflict {}/{}: {} ",
                self.resolved_count(),
                total,
                self.selected + 1,
                total,
                self.resolution().label(),
            ),
        };
        let right = match self.screen {
            Screen::Language => " type to filter · enter esc ".to_string(),
            Screen::Files => " j/k enter esc q ".to_string(),
            Screen::Merge if self.is_diff() && self.workspace.len() > 1 => {
                " f L ]/[ n/p j/k q ".to_string()
            }
            Screen::Merge if self.is_diff() => " L n/p j/k h/l q ".to_string(),
            Screen::Merge if self.workspace.len() > 1 => {
                " f L ]/[ tab 1/2/3 b u n/p w q ".to_string()
            }
            Screen::Merge => " L tab 1/2/3 b u n/p w q ".to_string(),
        };
        let pad =
            (area.width as usize).saturating_sub(display_width(&left) + display_width(&right));

        let style = Style::default()
            .fg(self.theme.status_fg)
            .bg(self.theme.status_bg);
        Paragraph::new(Line::from(vec![
            Span::styled(left, style),
            Span::styled(" ".repeat(pad), style),
            Span::styled(right, style),
        ]))
        .render(area, frame.buffer_mut());
    }
}

/// Open the workspace's current file, running mergiraf for it if this is the
/// first time it has been looked at.
fn open_current<'a>(
    assets: &'a Assets,
    workspace: &mut Workspace,
    theme: &DiffTheme,
    theme_name: &str,
) -> Result<FileView<'a>> {
    let file = workspace
        .current_mut()
        .context("the workspace has no file to open")?;

    // syntect and difftastic get the in-session choice if there is one, else
    // whatever `linguist-language` said.
    let language = file.effective_language().map(str::to_owned);

    if file.is_diff() {
        return FileView::new_diff(
            assets,
            file.left.clone(),
            file.right.clone(),
            &file.path,
            file.names.clone(),
            language.as_deref(),
            theme_name,
            theme,
        );
    }

    if file.session.is_none() {
        // Mergiraf reads the gitattributes itself, so only an explicit choice
        // is worth overriding it with.
        let merged = mergiraf::merge_texts(
            &file.base,
            &file.left,
            &file.right,
            file.override_language(),
            &file.path,
        )?;
        file.session = Some(MergeSession::new(
            merged.chunks,
            repo_marker_labels(&file.path),
        ));
    }

    let session = file.session.as_ref().expect("just built");
    FileView::new(
        assets,
        session,
        file.base.clone(),
        file.left.clone(),
        file.right.clone(),
        &file.path,
        file.names.clone(),
        language.as_deref(),
        theme_name,
        theme,
    )
}

/// Repaint a row's spans onto a different background, keeping foregrounds.
fn recolor(spans: &[StyledSpan], bg: Option<Color>) -> Vec<StyledSpan> {
    spans
        .iter()
        .map(|s| StyledSpan { bg, ..s.clone() })
        .collect()
}

/// The three equal columns the panes and their title bar share, so labels sit
/// over the columns they name.
fn pane_columns(area: Rect, count: usize) -> Vec<Rect> {
    let n = count.max(1) as u32;
    Layout::horizontal(vec![Constraint::Ratio(1, n); count.max(1)])
        .split(area)
        .to_vec()
}

fn styled(content: String, fg: Option<Color>, bg: Option<Color>) -> Span<'static> {
    let mut style = Style::default();
    if let Some(fg) = fg {
        style = style.fg(fg);
    }
    if let Some(bg) = bg {
        style = style.bg(bg);
    }
    Span::styled(content, style)
}

/// Slice a row's spans to the horizontal window `skip..skip + width`, measured
/// in terminal columns.
///
/// A double-width character straddling either edge is replaced by spaces so the
/// columns stay aligned — CJK source is the whole reason this is column-aware
/// rather than byte- or char-indexed.
pub fn clip_spans(spans: &[StyledSpan], skip: usize, width: usize) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut col = 0usize;
    let mut emitted = 0usize;

    for span in spans {
        if emitted >= width {
            break;
        }
        let mut text = String::new();
        for ch in span.text.chars() {
            let cw = ch.width().unwrap_or(0);
            let next = col + cw;
            if next <= skip {
                col = next;
                continue;
            }
            if emitted >= width {
                break;
            }
            // Straddles the left edge, or would overflow the right edge.
            if col < skip || emitted + cw > width {
                let visible = if col < skip {
                    next - skip
                } else {
                    width - emitted
                };
                let visible = visible.min(width - emitted);
                text.push_str(&" ".repeat(visible));
                emitted += visible;
            } else {
                text.push(ch);
                emitted += cw;
            }
            col = next;
        }
        if !text.is_empty() {
            out.push(styled(text, span.fg, span.bg));
        }
    }
    out
}

#[cfg(test)]
// One-element slices/vecs of ranges here are intentional; clippy mistakes them
// for an attempt to build a collection *from* a range.
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;

    const RED: Option<Color> = Some(Color::Rgb(255, 0, 0));
    const BLU: Option<Color> = Some(Color::Rgb(0, 0, 255));

    fn spans() -> Vec<StyledSpan> {
        vec![
            StyledSpan::new("abc", RED, None),
            StyledSpan::new("defg", BLU, None),
        ]
    }

    fn rendered(v: &[Span<'static>]) -> String {
        v.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn clip_without_scroll_keeps_everything_that_fits() {
        assert_eq!(rendered(&clip_spans(&spans(), 0, 10)), "abcdefg");
        assert_eq!(rendered(&clip_spans(&spans(), 0, 5)), "abcde");
    }

    #[test]
    fn clip_skips_leading_columns() {
        assert_eq!(rendered(&clip_spans(&spans(), 3, 10)), "defg");
        assert_eq!(rendered(&clip_spans(&spans(), 2, 3)), "cde");
    }

    #[test]
    fn clip_preserves_per_span_styles() {
        let out = clip_spans(&spans(), 2, 3);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].content.as_ref(), "c");
        assert_eq!(out[1].content.as_ref(), "de");
    }

    #[test]
    fn double_width_characters_count_two_columns() {
        let s = vec![StyledSpan::new("世界ab", RED, None)];
        assert_eq!(rendered(&clip_spans(&s, 0, 4)), "世界");
        // width 5 cannot fit `世界a`'s neighbour cleanly plus one more? it can:
        assert_eq!(rendered(&clip_spans(&s, 0, 5)), "世界a");
    }

    #[test]
    fn a_wide_char_straddling_the_right_edge_becomes_a_space() {
        let s = vec![StyledSpan::new("世界", RED, None)];
        // only 3 columns available: 世 fits, 界 would overflow
        assert_eq!(rendered(&clip_spans(&s, 0, 3)), "世 ");
    }

    #[test]
    fn a_wide_char_straddling_the_left_edge_becomes_a_space() {
        let s = vec![StyledSpan::new("世界", RED, None)];
        // skipping 1 column lands mid-世
        assert_eq!(rendered(&clip_spans(&s, 1, 10)), " 界");
    }

    #[test]
    fn scrolling_past_the_end_yields_nothing() {
        assert!(clip_spans(&spans(), 99, 10).is_empty());
        assert!(clip_spans(&spans(), 0, 0).is_empty());
    }
    use crate::merge::diff3::MergedChunk;
    use crate::merge::session::MarkerLabels;
    use crate::merge::workspace::{EntryKind, FileEntry};
    use crate::render::highlight::DEFAULT_THEME;
    use crate::render::panes::SectionNames;
    use std::path::PathBuf;

    /// Three revisions and the session that describes them, generated together
    /// so the conflicts' base sides really do occur in the base text — the
    /// region locator matches on content, so a hand-written mismatch would map
    /// every region onto no rows at all.
    ///
    /// The session is pre-built, which is what keeps these tests from invoking
    /// mergiraf: `App::new` only shells out for a file that has none yet.
    fn revisions(conflicts: usize) -> (String, String, String, MergeSession) {
        let one = |s: &str| vec![s.to_string()];
        let (mut base, mut left, mut right) = (
            vec!["fn main() {".to_string()],
            vec!["fn main() {".to_string()],
            vec!["fn main() {".to_string()],
        );
        let mut chunks = vec![MergedChunk::Resolved {
            lines: one("fn main() {"),
        }];

        for i in 0..conflicts {
            let b = format!("    let a{i} = {i};");
            let l = format!("    let a{i} = 10{i};");
            let r = format!("    let a{i} = 99{i};");
            base.push(b.clone());
            left.push(l.clone());
            right.push(r.clone());
            chunks.push(MergedChunk::Conflict {
                left: one(&l),
                base: one(&b),
                right: one(&r),
            });

            // A line all three agree on, so the regions stay separate.
            let shared = format!("    let b{i} = 2;");
            base.push(shared.clone());
            left.push(shared.clone());
            right.push(shared.clone());
            chunks.push(MergedChunk::Resolved {
                lines: one(&shared),
            });
        }

        for side in [&mut base, &mut left, &mut right] {
            side.push("}".into());
        }
        chunks.push(MergedChunk::Resolved { lines: one("}") });

        let text = |v: Vec<String>| format!("{}\n", v.join("\n"));
        (
            text(base),
            text(left),
            text(right),
            MergeSession::new(chunks, MarkerLabels::default()),
        )
    }

    fn file(path: &str, conflicts: usize) -> FileEntry {
        let (base, left, right, session) = revisions(conflicts);
        FileEntry {
            path: PathBuf::from(path),
            abs: PathBuf::from("/repo").join(path),
            base,
            left,
            right,
            names: SectionNames::new("left.rs", "base.rs", "right.rs", path),
            kind: EntryKind::Merge,
            attr_language: None,
            language: None,
            session: Some(session),
            binary: false,
            saved: false,
        }
    }

    fn binary(path: &str) -> FileEntry {
        FileEntry {
            binary: true,
            session: None,
            ..file(path, 0)
        }
    }

    struct Harness {
        assets: Assets,
        theme: DiffTheme,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                assets: Assets::new(),
                theme: DiffTheme::default(),
            }
        }

        fn app(&self, files: Vec<FileEntry>) -> App<'_> {
            self.app_to(files, Destination::Path(None))
        }

        fn app_to(&self, files: Vec<FileEntry>, destination: Destination) -> App<'_> {
            App::new(
                &self.assets,
                Workspace::new(files),
                self.theme.clone(),
                DEFAULT_THEME.to_string(),
                destination,
            )
            .unwrap()
        }
    }

    fn render_to_string(app: &mut App<'_>, w: u16, h: u16) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::from(KeyCode::Char(c))
    }

    // ---- the merge screen -------------------------------------------------

    #[test]
    fn draws_the_panes_the_merged_bar_and_the_status_line() {
        let h = Harness::new();
        let mut app = h.app(vec![file("a.rs", 1)]);
        let out = render_to_string(&mut app, 96, 16);

        assert!(out.contains("── MERGED · "), "{out}");
        assert!(out.contains("CONFLICT 1/1 · unresolved"), "{out}");
        assert!(out.contains("a.rs"), "{out}");
        assert!(out.contains("resolved 0/1"), "{out}");
        let rows: Vec<&str> = out.lines().collect();
        assert_eq!(rows[1].matches('│').count(), 2, "in {:?}", rows[1]);
    }

    #[test]
    fn each_column_is_named_after_the_revision_it_shows() {
        // Regression: every column used to be named after the file's display
        // path, so all three read `…/left.rs` and said nothing about which
        // revision you were looking at.
        let h = Harness::new();
        let mut app = h.app(vec![file("src/a.rs", 1)]);
        let titles = render_to_string(&mut app, 96, 16)
            .lines()
            .next()
            .unwrap()
            .to_string();

        assert!(titles.contains("LEFT · left.rs"), "{titles:?}");
        assert!(titles.contains("BASE · base.rs"), "{titles:?}");
        assert!(titles.contains("RIGHT · right.rs"), "{titles:?}");
        assert_eq!(titles.matches('┬').count(), 2, "{titles:?}");
        assert_eq!(
            titles.matches("left.rs").count(),
            1,
            "one column names left, not all three: {titles:?}"
        );
    }

    #[test]
    fn a_files_names_come_from_the_file_itself() {
        // Two files in a workspace can name their sections differently, so the
        // names have to travel with the file rather than be derived from one
        // path at render time.
        let h = Harness::new();
        let mut a = file("a.rs", 1);
        a.names = SectionNames::new("ours", "base", "theirs", "a.rs");
        let app = h.app(vec![a, file("b.rs", 1)]);
        assert_eq!(app.view.names.left, "ours");
        assert_eq!(app.view.names.merged, "a.rs");
        let _ = &app;
    }

    #[test]
    fn resolution_keys_change_the_region_and_rebuild_both_documents() {
        let h = Harness::new();
        let mut app = h.app(vec![file("a.rs", 1)]);
        render_to_string(&mut app, 96, 16);

        app.on_key(key('1'));
        assert_eq!(app.session().unwrap().resolution(0), Resolution::Left);
        let out = render_to_string(&mut app, 96, 16);
        assert!(out.contains("CONFLICT 1/1 · left"), "{out}");
        assert!(out.contains("resolved 1/1"), "{out}");

        app.on_key(key('3'));
        assert_eq!(app.session().unwrap().resolution(0), Resolution::Right);
        app.on_key(key('u'));
        assert_eq!(app.session().unwrap().resolution(0), Resolution::Unresolved);
    }

    #[test]
    fn the_panes_show_the_choice_once_a_region_is_resolved() {
        let h = Harness::new();
        let mut app = h.app(vec![file("a.rs", 1)]);
        render_to_string(&mut app, 96, 16);

        let region = app.view.panes.conflicts[0].clone();
        let bgs = |app: &App<'_>| Side::MERGE.map(|s| app.view.panes.rows[region.start].cell(s).bg);
        assert_eq!(bgs(&app), [Some(h.theme.conflict_bg); 3]);

        app.on_key(key('1'));
        assert_eq!(
            bgs(&app),
            [
                Some(h.theme.resolved_bg),
                Some(h.theme.dimmed_bg),
                Some(h.theme.dimmed_bg)
            ]
        );
    }

    #[test]
    fn tab_moves_focus_and_scrolls_only_the_focused_panel() {
        let h = Harness::new();
        let mut app = h.app(vec![file("a.rs", 1)]);
        render_to_string(&mut app, 96, 16);

        app.on_key(key('j'));
        let (panes_at, merged_at) = (app.scroll[0], app.scroll[1]);
        assert_eq!(merged_at, 0);

        app.on_key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Merged);
        app.on_key(key('j'));
        assert_eq!(app.scroll[0], panes_at);
        assert!(app.scroll[1] > merged_at);
    }

    #[test]
    fn n_and_p_walk_the_regions_of_one_file() {
        let h = Harness::new();
        let mut app = h.app(vec![file("a.rs", 3)]);
        render_to_string(&mut app, 96, 14);

        assert_eq!(app.session().unwrap().conflict_count(), 3);
        app.on_key(key('n'));
        assert_eq!(app.selected, 1);
        app.on_key(key('n'));
        assert_eq!(app.selected, 2);
        app.on_key(key('n'));
        assert_eq!(app.selected, 2, "stops at the last region");
        app.on_key(key('p'));
        assert_eq!(app.selected, 1);
    }

    // ---- directory mode ---------------------------------------------------

    #[test]
    fn a_single_file_workspace_hides_the_file_machinery() {
        let h = Harness::new();
        let mut app = h.app(vec![file("a.rs", 1)]);
        let out = render_to_string(&mut app, 96, 16);

        // `f` does nothing with one file, and the status bar omits the counter
        app.on_key(key('f'));
        assert_eq!(app.screen, Screen::Merge);
        assert!(!out.contains("[1/1]"), "{out}");
    }

    #[test]
    fn f_opens_the_file_list_and_esc_closes_it() {
        let h = Harness::new();
        let mut app = h.app(vec![file("a.rs", 1), file("b.rs", 1)]);
        render_to_string(&mut app, 96, 16);

        app.on_key(key('f'));
        assert_eq!(app.screen, Screen::Files);
        let out = render_to_string(&mut app, 96, 16);
        assert!(out.contains("FILES"), "{out}");

        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Merge);
    }

    #[test]
    fn the_file_list_shows_one_state_word_per_file() {
        let h = Harness::new();
        let mut app = h.app(vec![file("a.rs", 1), file("b.rs", 0), binary("logo.png")]);
        app.on_key(key('f'));
        let out = render_to_string(&mut app, 60, 10);

        assert!(out.contains("a.rs"), "{out}");
        assert!(out.contains("conflict"), "{out}");
        assert!(out.contains("b.rs"), "{out}");
        assert!(out.contains("resolved"), "{out}");
        assert!(out.contains("logo.png"), "{out}");
        assert!(out.contains("binary"), "{out}");
        // exactly one state word per file row, and no counts
        assert_eq!(out.matches("conflict").count(), 1, "{out}");
        assert!(!out.contains("1 conflict"), "no counts, as asked:\n{out}");
    }

    #[test]
    fn the_file_list_header_counts_resolved_files() {
        let h = Harness::new();
        let mut app = h.app(vec![file("a.rs", 1), file("b.rs", 0)]);
        app.on_key(key('f'));
        assert!(render_to_string(&mut app, 60, 10).contains("1/2 resolved"));

        app.on_key(KeyEvent::from(KeyCode::Esc));
        app.on_key(key('1'));
        app.on_key(key('f'));
        assert!(render_to_string(&mut app, 60, 10).contains("2/2 resolved"));
    }

    #[test]
    fn j_and_k_move_the_file_cursor_and_clamp() {
        let h = Harness::new();
        let mut app = h.app(vec![file("a.rs", 1), file("b.rs", 1)]);
        app.on_key(key('f'));

        assert_eq!(app.file_cursor, 0);
        app.on_key(key('j'));
        assert_eq!(app.file_cursor, 1);
        app.on_key(key('j'));
        assert_eq!(app.file_cursor, 1, "clamps at the last file");
        app.on_key(key('k'));
        assert_eq!(app.file_cursor, 0);
        app.on_key(key('k'));
        assert_eq!(app.file_cursor, 0);
    }

    #[test]
    fn enter_opens_the_highlighted_file() {
        let h = Harness::new();
        let mut app = h.app(vec![file("a.rs", 1), file("b.rs", 2)]);
        app.on_key(key('f'));
        app.on_key(key('j'));
        app.on_key(KeyEvent::from(KeyCode::Enter));

        assert_eq!(app.screen, Screen::Merge);
        assert_eq!(app.workspace.current_index(), 1);
        assert_eq!(app.title, "b.rs");
        assert_eq!(
            app.session().unwrap().conflict_count(),
            2,
            "b.rs's own session"
        );
    }

    #[test]
    fn enter_on_a_binary_file_refuses_and_says_so() {
        let h = Harness::new();
        let mut app = h.app(vec![file("a.rs", 1), binary("logo.png")]);
        app.on_key(key('f'));
        app.on_key(key('j'));
        app.on_key(KeyEvent::from(KeyCode::Enter));

        assert_eq!(app.screen, Screen::Files, "stays on the list");
        assert_eq!(app.workspace.current_index(), 0);
        assert!(app.status.as_deref().unwrap().contains("cannot be opened"));
    }

    #[test]
    fn bracket_keys_step_files_without_the_list() {
        let h = Harness::new();
        let mut app = h.app(vec![file("a.rs", 1), file("b.rs", 1)]);
        render_to_string(&mut app, 96, 16);

        app.on_key(key(']'));
        assert_eq!(app.workspace.current_index(), 1);
        assert_eq!(app.screen, Screen::Merge, "the list never opened");

        app.on_key(key(']'));
        assert_eq!(app.workspace.current_index(), 1);
        assert_eq!(app.status.as_deref(), Some("last file"));

        app.on_key(key('['));
        assert_eq!(app.workspace.current_index(), 0);
        app.on_key(key('['));
        assert_eq!(app.status.as_deref(), Some("first file"));
    }

    #[test]
    fn stepping_files_skips_binaries() {
        let h = Harness::new();
        let mut app = h.app(vec![file("a.rs", 1), binary("logo.png"), file("b.rs", 1)]);
        app.on_key(key(']'));
        assert_eq!(app.workspace.current().unwrap().path, PathBuf::from("b.rs"));
    }

    #[test]
    fn resolutions_survive_switching_files_and_coming_back() {
        let h = Harness::new();
        let mut app = h.app(vec![file("a.rs", 2), file("b.rs", 1)]);
        render_to_string(&mut app, 96, 16);

        app.on_key(key('1'));
        app.on_key(key('n'));
        app.on_key(key('3'));
        assert_eq!(app.session().unwrap().resolution(0), Resolution::Left);
        assert_eq!(app.session().unwrap().resolution(1), Resolution::Right);

        app.on_key(key(']'));
        assert_eq!(app.title, "b.rs");
        assert_eq!(app.selected, 0, "the region cursor resets for the new file");
        assert_eq!(app.session().unwrap().resolution(0), Resolution::Unresolved);

        app.on_key(key('['));
        assert_eq!(app.title, "a.rs");
        assert_eq!(app.session().unwrap().resolution(0), Resolution::Left);
        assert_eq!(app.session().unwrap().resolution(1), Resolution::Right);
    }

    #[test]
    fn the_status_bar_names_the_files_place_in_the_set() {
        let h = Harness::new();
        let mut app = h.app(vec![file("a.rs", 1), file("b.rs", 1), file("c.rs", 1)]);
        assert!(render_to_string(&mut app, 96, 16).contains("a.rs [1/3]"));
        app.on_key(key(']'));
        assert!(render_to_string(&mut app, 96, 16).contains("b.rs [2/3]"));
    }

    // ---- diff mode --------------------------------------------------------

    const D_OLD: &str = "fn main() {\n    let a = 1;\n    let b = 2;\n}\n";
    const D_NEW: &str = "fn main() {\n    let a = 100;\n}\n";

    fn diff_file(path: &str, old: &str, new: &str) -> FileEntry {
        FileEntry {
            path: PathBuf::from(path),
            abs: PathBuf::from("/repo").join(path),
            base: String::new(),
            left: old.into(),
            right: new.into(),
            names: SectionNames {
                left: "old".into(),
                base: String::new(),
                right: "new".into(),
                merged: path.into(),
            },
            kind: EntryKind::Diff,
            attr_language: None,
            language: None,
            session: None,
            binary: false,
            saved: false,
        }
    }

    #[test]
    fn a_diff_draws_two_panes_and_no_merged_panel() {
        let h = Harness::new();
        let mut app = h.app(vec![diff_file("a.rs", D_OLD, D_NEW)]);
        let out = render_to_string(&mut app, 96, 14);

        assert!(
            !out.contains("MERGED"),
            "a diff has nothing to merge:\n{out}"
        );
        assert!(!out.contains("CONFLICT"), "{out}");
        let rows: Vec<&str> = out.lines().collect();
        assert!(rows[0].contains("OLD · old"), "{:?}", rows[0]);
        assert!(rows[0].contains("NEW · new"), "{:?}", rows[0]);
        assert_eq!(
            rows[0].matches('┬').count(),
            1,
            "two columns: {:?}",
            rows[0]
        );
        assert_eq!(rows[1].matches('│').count(), 1, "{:?}", rows[1]);
    }

    #[test]
    fn the_resolution_keys_do_nothing_to_a_diff() {
        let h = Harness::new();
        let mut app = h.app(vec![diff_file("a.rs", D_OLD, D_NEW)]);
        render_to_string(&mut app, 96, 14);

        for k in ['1', '2', '3', 'b', 'u'] {
            app.on_key(key(k));
            assert!(app.session().is_none(), "{k} must not invent a session");
        }
        app.on_key(key('w'));
        assert_eq!(app.status.as_deref(), Some("a diff has nothing to write"));
    }

    #[test]
    fn tab_is_inert_in_a_diff() {
        let h = Harness::new();
        let mut app = h.app(vec![diff_file("a.rs", D_OLD, D_NEW)]);
        render_to_string(&mut app, 96, 14);
        app.on_key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Panes, "there is only one panel to focus");
    }

    #[test]
    fn n_and_p_walk_the_hunks_of_a_diff() {
        let h = Harness::new();
        let mut app = h.app(vec![diff_file("a.rs", D_OLD, D_NEW)]);
        render_to_string(&mut app, 96, 8);

        let hunks = app.view.panes.hunks.clone();
        assert!(!hunks.is_empty(), "the fixture must differ");

        // `n` clamps at the last hunk, so with one hunk it stays put.
        app.on_key(key('n'));
        assert_eq!(app.selected, hunks.len() - 1);
        app.on_key(key('p'));
        assert_eq!(app.selected, 0);

        // The hunk is on screen; whether that took any scrolling depends on
        // whether the file fits, which is not what this is testing.
        let start = app.scroll[Focus::Panes.index()];
        let visible = start..start + app.viewport[Focus::Panes.index()];
        assert!(
            visible.contains(&hunks[0]),
            "hunk {} not in {visible:?}",
            hunks[0]
        );
    }

    #[test]
    fn the_status_bar_counts_hunks_rather_than_conflicts() {
        let h = Harness::new();
        let mut app = h.app(vec![diff_file("a.rs", D_OLD, D_NEW)]);
        let out = render_to_string(&mut app, 96, 14);
        assert!(out.contains("hunk 1/"), "{out}");
        assert!(!out.contains("resolved"), "{out}");
    }

    #[test]
    fn an_unchanged_diff_says_so() {
        let h = Harness::new();
        let mut app = h.app(vec![diff_file("a.rs", D_OLD, D_OLD)]);
        let out = render_to_string(&mut app, 96, 14);
        assert!(out.contains("no changes"), "{out}");
    }

    #[test]
    fn the_picker_lists_diff_entries_by_what_happened_to_them() {
        let h = Harness::new();
        let mut app = h.app(vec![
            diff_file("a.rs", D_OLD, D_NEW),
            diff_file("added.rs", "", D_NEW),
            diff_file("gone.rs", D_OLD, ""),
        ]);
        app.on_key(key('f'));
        let out = render_to_string(&mut app, 60, 10);

        assert!(out.contains("modified"), "{out}");
        assert!(out.contains("added"), "{out}");
        assert!(out.contains("deleted"), "{out}");
        assert!(!out.contains("conflict"), "a diff has no conflicts:\n{out}");
    }

    #[test]
    fn stepping_between_diff_files_reloads_the_view() {
        let h = Harness::new();
        let mut app = h.app(vec![
            diff_file("a.rs", D_OLD, D_NEW),
            diff_file("b.rs", D_OLD, D_OLD),
        ]);
        render_to_string(&mut app, 96, 14);

        app.on_key(key(']'));
        assert_eq!(app.title, "b.rs");
        assert!(app.view.panes.hunks.is_empty(), "b.rs is unchanged");
        app.on_key(key('['));
        assert_eq!(app.title, "a.rs");
        assert!(!app.view.panes.hunks.is_empty());
    }

    #[test]
    fn a_diff_renders_at_degenerate_terminal_sizes() {
        let h = Harness::new();
        let mut app = h.app(vec![diff_file("a.rs", D_OLD, D_NEW)]);
        for (w, hh) in [(6, 3), (1, 2), (3, 4), (40, 2), (96, 2)] {
            render_to_string(&mut app, w, hh);
        }
    }

    #[test]
    fn merge_and_diff_entries_can_share_a_workspace() {
        // Nothing stops a workspace holding both, and switching between them
        // must swap the whole screen shape.
        let h = Harness::new();
        let mut app = h.app(vec![file("m.rs", 1), diff_file("d.rs", D_OLD, D_NEW)]);
        assert!(render_to_string(&mut app, 96, 16).contains("MERGED"));
        app.on_key(key(']'));
        assert!(!render_to_string(&mut app, 96, 16).contains("MERGED"));
    }

    // ---- the language picker ----------------------------------------------

    #[test]
    fn capital_l_opens_the_language_list_and_esc_closes_it() {
        let h = Harness::new();
        let mut app = h.app(vec![diff_file("a.rs", D_OLD, D_NEW)]);
        render_to_string(&mut app, 96, 14);

        app.on_key(key('L'));
        assert_eq!(app.screen, Screen::Language);
        let out = render_to_string(&mut app, 60, 14);
        assert!(out.contains("LANGUAGE"), "{out}");
        assert!(out.contains(AUTO_DETECT), "{out}");

        // The list is alphabetical and long, so Rust is off screen until the
        // filter brings it up — which is the reason the filter exists.
        for c in "rust".chars() {
            app.on_key(key(c));
        }
        assert!(render_to_string(&mut app, 60, 14).contains("Rust"));

        app.on_key(KeyEvent::from(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Merge);
        assert_eq!(app.workspace.current().unwrap().language, None);
    }

    #[test]
    fn lowercase_l_still_scrolls_horizontally() {
        let h = Harness::new();
        let mut app = h.app(vec![diff_file("a.rs", D_OLD, D_NEW)]);
        render_to_string(&mut app, 96, 14);
        app.on_key(key('l'));
        assert_eq!(app.screen, Screen::Merge);
        assert!(app.hscroll[Focus::Panes.index()] > 0);
    }

    #[test]
    fn typing_filters_the_list_and_backspace_undoes_it() {
        let h = Harness::new();
        let mut app = h.app(vec![diff_file("a.rs", D_OLD, D_NEW)]);
        app.on_key(key('L'));
        let all = app.language_choices().len();

        for c in "rust".chars() {
            app.on_key(key(c));
        }
        let filtered = app.language_choices();
        assert!(filtered.len() < all, "the filter should narrow the list");
        // case-insensitive, and (auto-detect) always survives
        assert_eq!(filtered[0], AUTO_DETECT);
        assert!(filtered.contains(&"Rust"), "{filtered:?}");

        app.on_key(KeyEvent::from(KeyCode::Backspace));
        assert_eq!(app.language_filter, "rus");
        for _ in 0..5 {
            app.on_key(KeyEvent::from(KeyCode::Backspace));
        }
        assert_eq!(app.language_choices().len(), all);
    }

    #[test]
    fn a_filter_matching_nothing_leaves_only_auto_detect() {
        let h = Harness::new();
        let mut app = h.app(vec![diff_file("a.rs", D_OLD, D_NEW)]);
        app.on_key(key('L'));
        for c in "zzzznotalanguage".chars() {
            app.on_key(key(c));
        }
        assert_eq!(app.language_choices(), vec![AUTO_DETECT]);
        render_to_string(&mut app, 60, 14);
        // and enter on it is harmless
        app.on_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(app.workspace.current().unwrap().language, None);
    }

    #[test]
    fn the_cursor_clamps_at_both_ends_of_the_list() {
        let h = Harness::new();
        let mut app = h.app(vec![diff_file("a.rs", D_OLD, D_NEW)]);
        app.on_key(key('L'));

        for _ in 0..500 {
            app.on_key(KeyEvent::from(KeyCode::Down));
        }
        assert_eq!(app.language_cursor, app.language_choices().len() - 1);
        for _ in 0..500 {
            app.on_key(KeyEvent::from(KeyCode::Up));
        }
        assert_eq!(app.language_cursor, 0);
    }

    /// Move the cursor onto a named language and apply it.
    fn choose(app: &mut App<'_>, name: &str) {
        app.on_key(key('L'));
        for c in name.chars() {
            app.on_key(key(c));
        }
        let at = app
            .language_choices()
            .iter()
            .position(|n| *n == name)
            .unwrap_or_else(|| panic!("{name} not in {:?}", app.language_choices()));
        for _ in 0..at {
            app.on_key(KeyEvent::from(KeyCode::Down));
        }
        app.on_key(KeyEvent::from(KeyCode::Enter));
    }

    #[test]
    fn choosing_a_language_rebuilds_a_diff_immediately() {
        let h = Harness::new();
        let mut app = h.app(vec![diff_file("a.rs", D_OLD, D_NEW)]);
        render_to_string(&mut app, 96, 14);
        assert_eq!(app.view.syntax, "Rust", "detected from the .rs path");

        choose(&mut app, "Markdown");
        assert_eq!(app.screen, Screen::Merge, "no confirmation for a diff");
        assert!(app.pending_language.is_none());
        assert_eq!(
            app.workspace.current().unwrap().language.as_deref(),
            Some("Markdown")
        );
        assert_eq!(app.view.syntax, "Markdown", "the view really rebuilt");
    }

    #[test]
    fn auto_detect_clears_the_override() {
        let h = Harness::new();
        let mut app = h.app(vec![diff_file("a.rs", D_OLD, D_NEW)]);
        render_to_string(&mut app, 96, 14);

        choose(&mut app, "Markdown");
        assert_eq!(app.view.syntax, "Markdown");

        app.on_key(key('L'));
        app.on_key(KeyEvent::from(KeyCode::Enter)); // cursor 0 is (auto-detect)
        assert_eq!(app.workspace.current().unwrap().language, None);
        assert_eq!(app.view.syntax, "Rust", "detection takes over again");
    }

    #[test]
    fn the_attribute_supplies_the_language_when_nothing_is_chosen() {
        let h = Harness::new();
        let mut entry = diff_file("a.weird", D_OLD, D_NEW);
        entry.attr_language = Some("Markdown".into());
        let mut app = h.app(vec![entry]);
        render_to_string(&mut app, 96, 14);
        assert_eq!(app.view.syntax, "Markdown", "from linguist-language");

        // and the list shows it as the one in effect
        app.on_key(key('L'));
        assert!(render_to_string(&mut app, 60, 14).contains("LANGUAGE · Markdown"));
    }

    #[test]
    fn a_merge_with_nothing_resolved_changes_language_without_asking() {
        let h = Harness::new();
        let mut app = h.app(vec![file("a.rs", 1)]);
        render_to_string(&mut app, 96, 16);

        choose(&mut app, "Markdown");
        assert!(app.pending_language.is_none(), "nothing to discard");
        assert_eq!(
            app.workspace.current().unwrap().language.as_deref(),
            Some("Markdown")
        );
        // mergiraf re-ran during the reload, so there is a session again — a
        // new one, built with the language this time.
        assert!(app.workspace.current().unwrap().session.is_some());
        assert_eq!(app.resolved_count(), 0);
    }

    #[test]
    fn a_merge_with_choices_asks_before_discarding_them() {
        let h = Harness::new();
        let mut app = h.app(vec![file("a.rs", 2)]);
        render_to_string(&mut app, 96, 16);
        app.on_key(key('1'));
        assert_eq!(app.resolved_count(), 1);

        choose(&mut app, "Markdown");
        assert_eq!(app.pending_language, Some(Some("Markdown".into())));
        assert_eq!(
            app.workspace.current().unwrap().language,
            None,
            "nothing is applied until it is confirmed"
        );
        assert!(app.workspace.current().unwrap().session.is_some());
        let out = render_to_string(&mut app, 96, 16);
        assert!(out.contains("discards 1 choice"), "{out}");
        assert!(out.contains("enter to confirm"), "{out}");
    }

    #[test]
    fn cancelling_the_confirmation_leaves_the_merge_alone() {
        let h = Harness::new();
        let mut app = h.app(vec![file("a.rs", 2)]);
        render_to_string(&mut app, 96, 16);
        app.on_key(key('1'));

        choose(&mut app, "Markdown");
        app.on_key(KeyEvent::from(KeyCode::Esc));

        assert!(app.pending_language.is_none());
        assert!(!app.quit, "esc must cancel, not quit");
        assert_eq!(app.workspace.current().unwrap().language, None);
        assert_eq!(app.resolved_count(), 1, "the choice survived");
    }

    #[test]
    fn confirming_applies_the_language_and_drops_the_session() {
        let h = Harness::new();
        let mut app = h.app(vec![file("a.rs", 2)]);
        render_to_string(&mut app, 96, 16);
        app.on_key(key('1'));

        choose(&mut app, "Markdown");
        app.on_key(KeyEvent::from(KeyCode::Enter));

        assert!(app.pending_language.is_none());
        assert_eq!(
            app.workspace.current().unwrap().language.as_deref(),
            Some("Markdown")
        );
        assert_eq!(
            app.resolved_count(),
            0,
            "mergiraf re-ran, so the old chunks and their choices are gone"
        );
    }

    #[test]
    fn a_pending_confirmation_swallows_other_keys() {
        let h = Harness::new();
        let mut app = h.app(vec![file("a.rs", 2)]);
        render_to_string(&mut app, 96, 16);
        app.on_key(key('1'));
        choose(&mut app, "Markdown");

        app.on_key(key('j'));
        assert!(app.pending_language.is_some(), "still waiting");
        assert!(app.status.as_deref().unwrap().contains("enter to confirm"));
        app.on_key(key('q'));
        assert!(!app.quit, "q must not slip past the question");
    }

    #[test]
    fn the_language_list_renders_at_degenerate_sizes() {
        let h = Harness::new();
        let mut app = h.app(vec![diff_file("a.rs", D_OLD, D_NEW)]);
        app.on_key(key('L'));
        for (w, hh) in [(6, 3), (1, 2), (3, 4), (20, 2), (96, 2)] {
            render_to_string(&mut app, w, hh);
        }
    }

    // ---- saving -----------------------------------------------------------

    #[test]
    fn writing_without_an_output_path_reports_instead_of_panicking() {
        let h = Harness::new();
        let mut app = h.app(vec![file("a.rs", 1)]);
        app.on_key(key('w'));
        assert_eq!(
            app.status.as_deref(),
            Some("no --output given; nothing written")
        );
    }

    #[test]
    fn writing_saves_the_resolved_file_and_reports_what_is_left() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("merged.rs");
        let h = Harness::new();
        let mut app = h.app_to(vec![file("a.rs", 1)], Destination::Path(Some(path.clone())));

        app.on_key(key('w'));
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("<<<<<<<"), "in:\n{written}");
        assert!(app.status.as_deref().unwrap().contains("1 unresolved"));

        app.on_key(key('1'));
        app.on_key(key('w'));
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(!written.contains("<<<<<<<"), "in:\n{written}");
        assert!(!app.status.as_deref().unwrap().contains("unresolved"));
    }

    #[test]
    fn resolving_again_clears_the_saved_flag() {
        let h = Harness::new();
        let mut app = h.app(vec![file("a.rs", 1)]);
        app.workspace.current_mut().unwrap().saved = true;
        app.on_key(key('1'));
        assert!(!app.workspace.current().unwrap().saved);
    }

    // ---- layout robustness ------------------------------------------------

    #[test]
    fn renders_at_degenerate_terminal_sizes() {
        let h = Harness::new();
        let mut app = h.app(vec![file("a.rs", 1), file("b.rs", 1)]);
        for (w, hh) in [(6, 4), (1, 3), (3, 4), (40, 5), (20, 2)] {
            render_to_string(&mut app, w, hh);
        }
        app.on_key(key('f'));
        for (w, hh) in [(6, 4), (1, 2), (40, 3)] {
            render_to_string(&mut app, w, hh);
        }
    }

    #[test]
    fn a_long_path_is_shortened_rather_than_overflowing() {
        let h = Harness::new();
        let mut app = h.app(vec![
            file("a/very/deeply/nested/directory/with/a/long/name.rs", 1),
            file("b.rs", 1),
        ]);
        app.on_key(key('f'));
        let out = render_to_string(&mut app, 40, 8);
        for line in out.lines() {
            assert!(display_width(line) <= 40, "{line:?}");
        }
        assert!(out.contains("name.rs"), "the tail survives:\n{out}");
    }

    #[test]
    fn scroll_clamps_in_both_panels() {
        let h = Harness::new();
        let mut app = h.app(vec![file("a.rs", 1)]);
        render_to_string(&mut app, 96, 12);

        for focus in [Focus::Panes, Focus::Merged] {
            app.focus = focus;
            for _ in 0..80 {
                app.on_key(key('j'));
            }
            assert!(app.scroll[focus.index()] <= app.max_scroll(focus));
            for _ in 0..80 {
                app.on_key(key('k'));
            }
            assert_eq!(app.scroll[focus.index()], 0);
        }
    }

    #[test]
    fn clipped_output_never_exceeds_the_requested_width() {
        let s = vec![StyledSpan::new("a世b界c", RED, None)];
        for skip in 0..8 {
            for width in 0..10 {
                let out = clip_spans(&s, skip, width);
                let w: usize = out.iter().map(|x| display_width(&x.content)).sum();
                assert!(w <= width, "skip={skip} width={width} produced {w}");
            }
        }
    }
}
