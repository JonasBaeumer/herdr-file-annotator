//! Ratatui review UI: file navigator + diff view.
//!
//! M2 added a two-pane layout (file navigator on the left, the selected
//! file's diff on the right) replacing M1's single scrolling view. M3 adds
//! line-anchored annotations: a visual-select-and-comment flow anchored to
//! the diff cursor, surfaced as gutter markers and returned to `pane.rs` as
//! `Outcome::annotations`.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::OnceLock;

use anyhow::Result;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect, Size},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame, Terminal,
};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

use crate::diff::{DiffLine, DiffModel, FileDiff, FileStatus, Origin};
use crate::protocol::{Annotation, LineRange, ReviewRequest, Side, Verdict};

/// What the reviewer decided, handed back to `pane.rs`.
pub struct Outcome {
    pub verdict: Verdict,
    pub summary: Option<String>,
    /// All annotations the reviewer left, in creation order. Empty for
    /// `Verdict::Cancelled` — a cancelled review carries no feedback.
    pub annotations: Vec<Annotation>,
}

/// Syntax highlighting resources: a syntax set (language grammars) and a
/// single fixed theme. Built once (`SyntaxSet::load_defaults_newlines` and
/// `ThemeSet::load_defaults` are both nontrivial parses) and reused for the
/// lifetime of the process via `OnceLock`, rather than being threaded
/// through as owned state on `App` (which would tangle `App`'s lifetime
/// with the highlighter's).
struct Highlighter {
    syntax_set: SyntaxSet,
    theme: Theme,
}

static HIGHLIGHTER: OnceLock<Highlighter> = OnceLock::new();

fn highlighter() -> &'static Highlighter {
    HIGHLIGHTER.get_or_init(|| {
        let syntax_set = SyntaxSet::load_defaults_newlines();
        let theme_set = ThemeSet::load_defaults();
        let theme = theme_set.themes["base16-eighties.dark"].clone();
        Highlighter { syntax_set, theme }
    })
}

/// Look up the syntax for a file by its path's extension, falling back to
/// plain text (no highlighting, just the theme's default foreground) when
/// there's no extension or no matching grammar.
fn syntax_for_path<'a>(hl: &'a Highlighter, path: &str) -> &'a SyntaxReference {
    Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .and_then(|ext| hl.syntax_set.find_syntax_by_extension(ext))
        .unwrap_or_else(|| hl.syntax_set.find_syntax_plain_text())
}

/// Run the review UI to completion and return the reviewer's verdict.
///
/// `model` is the already-loaded diff (or the error from trying to load it —
/// the UI still lets the reviewer cancel out even when the parser/git failed).
pub fn run(request: &ReviewRequest, model: Result<DiffModel>) -> Result<Outcome> {
    let _guard = TermGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut app = App::new(request, &model);

    loop {
        terminal.draw(|frame| draw(frame, &app))?;
        match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                let size = terminal.size()?;
                if let Some(outcome) = app.handle_key(key, size) {
                    return Ok(outcome);
                }
            }
            // Resize, mouse, release events, etc: just redraw next iteration.
            _ => {}
        }
    }
}

/// RAII guard so a panic can't leave the terminal in raw/alternate mode.
struct TermGuard;

impl TermGuard {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, cursor::Hide)?;
        Ok(TermGuard)
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), cursor::Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

/// Which body pane currently has keyboard focus.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Navigator,
    Diff,
}

/// File-list selection, decoupled from rendering so it's plain and testable.
#[derive(Default)]
struct NavState {
    selected: usize,
}

impl NavState {
    fn down(&mut self, len: usize) {
        if len == 0 {
            self.selected = 0;
            return;
        }
        self.selected = (self.selected + 1).min(len - 1);
    }

    fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    fn top(&mut self) {
        self.selected = 0;
    }

    fn bottom(&mut self, len: usize) {
        self.selected = len.saturating_sub(1);
    }
}

/// Diff-pane scroll position: the index of the topmost visible display row.
/// Decoupled from rendering (no terminal/frame types) so the clamping and
/// hunk-jump math can be unit tested directly.
#[derive(Default)]
struct DiffViewState {
    /// Topmost visible row (derived: follows the cursor).
    scroll: usize,
    /// The addressed row — what j/k move, what gets highlighted, and what a
    /// future annotation anchors to.
    cursor: usize,
}

impl DiffViewState {
    /// Keep the cursor visible: scroll follows it at both edges.
    fn follow(&mut self, viewport: usize) {
        let viewport = viewport.max(1);
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + viewport {
            self.scroll = self.cursor + 1 - viewport;
        }
    }

    fn reset(&mut self) {
        self.scroll = 0;
        self.cursor = 0;
    }

    fn down(&mut self, row_count: usize, viewport: usize) {
        self.cursor = (self.cursor + 1).min(row_count.saturating_sub(1));
        self.follow(viewport);
    }

    fn up(&mut self, viewport: usize) {
        self.cursor = self.cursor.saturating_sub(1);
        self.follow(viewport);
    }

    fn page_down(&mut self, page: usize, row_count: usize, viewport: usize) {
        self.cursor = (self.cursor + page).min(row_count.saturating_sub(1));
        self.follow(viewport);
    }

    fn page_up(&mut self, page: usize, viewport: usize) {
        self.cursor = self.cursor.saturating_sub(page);
        self.follow(viewport);
    }

    fn top(&mut self) {
        self.cursor = 0;
        self.scroll = 0;
    }

    fn bottom(&mut self, row_count: usize, viewport: usize) {
        self.cursor = row_count.saturating_sub(1);
        self.follow(viewport);
    }

    /// Move the cursor to the next hunk-header row strictly after it.
    /// No-op if there is none.
    fn next_hunk(&mut self, hunk_rows: &[usize], viewport: usize) {
        if let Some(&next) = hunk_rows.iter().find(|&&r| r > self.cursor) {
            self.cursor = next;
            self.follow(viewport);
        }
    }

    /// Move the cursor to the previous hunk-header row strictly before it.
    /// No-op if there is none.
    fn prev_hunk(&mut self, hunk_rows: &[usize], viewport: usize) {
        if let Some(&prev) = hunk_rows.iter().rev().find(|&&r| r < self.cursor) {
            self.cursor = prev;
            self.follow(viewport);
        }
    }
}

/// One flattened row of the diff display.
enum DiffRow<'a> {
    HunkHeader(&'a str),
    Line(&'a DiffLine),
    Binary,
    NoContent,
}

/// Flatten a file's hunks into display rows (a hunk-header row followed by
/// its lines, for each hunk). Binary files and files with no hunks collapse
/// to a single placeholder row.
fn flatten_rows(file: &FileDiff) -> Vec<DiffRow<'_>> {
    if file.binary {
        return vec![DiffRow::Binary];
    }
    if file.hunks.is_empty() {
        return vec![DiffRow::NoContent];
    }
    let mut rows = Vec::new();
    for hunk in &file.hunks {
        rows.push(DiffRow::HunkHeader(&hunk.header));
        for line in &hunk.lines {
            rows.push(DiffRow::Line(line));
        }
    }
    rows
}

/// Indices within `rows` that are hunk-header rows, used for n/p jumps.
fn hunk_row_indices(rows: &[DiffRow]) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter_map(|(i, r)| matches!(r, DiffRow::HunkHeader(_)).then_some(i))
        .collect()
}

/// Semantic label a reviewer can attach to a comment. `Ctrl-T` cycles
/// through these (and back to no tag) while the comment input is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tag {
    Verify,
    Fix,
    Question,
    Nit,
}

impl Tag {
    /// The lowercase wire label, also used as the protocol's `Annotation::tag`.
    fn label(&self) -> &'static str {
        match self {
            Tag::Verify => "verify",
            Tag::Fix => "fix",
            Tag::Question => "question",
            Tag::Nit => "nit",
        }
    }

    fn from_label(label: &str) -> Option<Tag> {
        match label {
            "verify" => Some(Tag::Verify),
            "fix" => Some(Tag::Fix),
            "question" => Some(Tag::Question),
            "nit" => Some(Tag::Nit),
            _ => None,
        }
    }

    /// Cycle order: none → Verify → Fix → Question → Nit → none.
    fn next(current: Option<Tag>) -> Option<Tag> {
        match current {
            None => Some(Tag::Verify),
            Some(Tag::Verify) => Some(Tag::Fix),
            Some(Tag::Fix) => Some(Tag::Question),
            Some(Tag::Question) => Some(Tag::Nit),
            Some(Tag::Nit) => None,
        }
    }
}

/// Gutter-marker color for a tag, matching the protocol's string label
/// (pending annotations store the tag as `Option<String>`, not `Tag`,
/// since that's what crosses the wire). Untagged annotations render White.
fn tag_color(tag: Option<&str>) -> Color {
    match tag {
        Some("verify") => Color::Yellow,
        Some("fix") => Color::Red,
        Some("question") => Color::Cyan,
        Some("nit") => Color::DarkGray,
        _ => Color::White,
    }
}

/// What the "request changes" / "comment" input bar is doing right now.
enum InputMode {
    /// The "request changes" summary prompt (unchanged from M2's `input:
    /// Option<String>`, just relocated into this enum).
    Summary { buf: String },
    /// The annotation comment prompt, opened by `c` in diff focus.
    Comment {
        buf: String,
        tag: Option<Tag>,
        /// `Some(idx)` when editing an existing `pending[idx]` rather than
        /// creating a new annotation.
        editing: Option<usize>,
        /// Flattened-row range (inclusive) this comment will anchor to.
        row_start: usize,
        row_end: usize,
    },
}

/// An annotation the reviewer has left but not yet submitted with a verdict.
/// `row_start`/`row_end` are flattened-row indices (inclusive) into
/// `files[file_idx]`'s rows — used for gutter markers and for finding the
/// annotation under the cursor to edit or delete.
struct PendingAnnotation {
    file_idx: usize,
    row_start: usize,
    row_end: usize,
    annotation: Annotation,
}

/// Build an `Annotation` from a flattened-row selection. Only `DiffRow::Line`
/// rows in `row_start..=row_end` count; hunk headers and placeholder rows are
/// ignored. Prefers the new-file side: if any covered line has a `new_no`
/// (context or added lines), the annotation anchors to the new side with the
/// min/max of those line numbers; otherwise, if any covered line has an
/// `old_no` (a remove-only selection), it anchors to the old side. A
/// selection covering no `DiffRow::Line` at all (headers/placeholder only)
/// resolves to `None` — nothing to save.
fn resolve_annotation(
    file: &FileDiff,
    rows: &[DiffRow],
    row_start: usize,
    row_end: usize,
    tag: Option<Tag>,
    comment: String,
) -> Option<Annotation> {
    let mut new_nos: Vec<u32> = Vec::new();
    let mut old_nos: Vec<u32> = Vec::new();
    for idx in row_start..=row_end {
        if let Some(DiffRow::Line(line)) = rows.get(idx) {
            if let Some(n) = line.new_no {
                new_nos.push(n);
            }
            if let Some(o) = line.old_no {
                old_nos.push(o);
            }
        }
    }

    let (side, start, end) = if let (Some(&min), Some(&max)) =
        (new_nos.iter().min(), new_nos.iter().max())
    {
        (Side::New, min, max)
    } else if let (Some(&min), Some(&max)) = (old_nos.iter().min(), old_nos.iter().max()) {
        (Side::Old, min, max)
    } else {
        return None;
    };

    Some(Annotation {
        file: file.path.clone(),
        lines: LineRange { start, end },
        side,
        tag: tag.map(|t| t.label().to_string()),
        comment,
    })
}

/// Visible diff rows, derived from the terminal size via the SAME split
/// logic as the real layout (header + note + footer chrome, then body_split,
/// then the diff pane's top/bottom border) — so cursor-following stays
/// correct in both the side-by-side and stacked layouts.
fn diff_viewport_rows(term_size: Size) -> usize {
    let body = Rect::new(0, 0, term_size.width, term_size.height.saturating_sub(3));
    let (_, diff_area) = body_split(body);
    (diff_area.height.saturating_sub(2) as usize).max(1)
}

/// Half a screen's worth of diff rows.
fn half_page(term_size: Size) -> usize {
    (diff_viewport_rows(term_size) / 2).max(1)
}

/// Marker color, matching git status vocabulary.
fn marker_color(status: FileStatus) -> Color {
    match status {
        FileStatus::Modified => Color::Yellow,
        FileStatus::Added => Color::Green,
        FileStatus::Deleted => Color::Red,
        FileStatus::Renamed => Color::Cyan,
        FileStatus::Untracked => Color::Magenta,
    }
}

/// The path shown for a file: `old → new` for renames, else just the path.
fn file_display_path(file: &FileDiff) -> String {
    match (&file.old_path, file.status) {
        (Some(old), FileStatus::Renamed) => format!("{old} \u{2192} {}", file.path),
        _ => file.path.clone(),
    }
}

struct App<'a> {
    request: &'a ReviewRequest,
    model: &'a Result<DiffModel>,
    focus: Focus,
    nav: NavState,
    diff: DiffViewState,
    /// Some(mode) while the "request changes" summary prompt or the
    /// annotation comment prompt is open.
    input: Option<InputMode>,
    /// Set by `v` in diff focus at the cursor row; the active selection is
    /// `min(anchor, cursor)..=max(anchor, cursor)` and grows/shrinks as the
    /// cursor moves. Diff-focus only; cleared by a second `v`, by `Esc`, or
    /// by opening a comment with `c`.
    visual_anchor: Option<usize>,
    /// Annotations the reviewer has left so far, in creation order.
    pending: Vec<PendingAnnotation>,
    /// Fully syntax-highlighted body rows per file, keyed by index into
    /// `model.files`. Computed once up front in `new` rather than lazily on
    /// first view: model sizes here (a code review's changed files) are
    /// moderate, and precomputing avoids needing interior mutability just
    /// so `draw` (which only borrows `App` immutably) can populate a cache
    /// on demand. Never invalidated — the model is immutable for the life
    /// of the UI.
    row_cache: HashMap<usize, Vec<Line<'static>>>,
}

impl<'a> App<'a> {
    fn new(request: &'a ReviewRequest, model: &'a Result<DiffModel>) -> Self {
        let mut row_cache = HashMap::new();
        if let Ok(m) = model {
            let hl = highlighter();
            for (i, file) in m.files.iter().enumerate() {
                row_cache.insert(i, highlight_file_rows(hl, file));
            }
        }
        App {
            request,
            model,
            focus: Focus::Navigator,
            nav: NavState::default(),
            diff: DiffViewState::default(),
            input: None,
            visual_anchor: None,
            pending: Vec::new(),
            row_cache,
        }
    }

    fn files(&self) -> &[FileDiff] {
        match self.model {
            Ok(m) => &m.files,
            Err(_) => &[],
        }
    }

    fn selected_file(&self) -> Option<&FileDiff> {
        self.files().get(self.nav.selected)
    }

    fn diff_rows(&self) -> Vec<DiffRow<'_>> {
        self.selected_file().map(flatten_rows).unwrap_or_default()
    }

    /// All pending annotations, in creation order, cloned for handoff.
    fn pending_annotations(&self) -> Vec<Annotation> {
        self.pending.iter().map(|p| p.annotation.clone()).collect()
    }

    /// Handle one key event. Returns `Some(outcome)` once the reviewer has
    /// made a final decision (approve / request changes / cancel).
    fn handle_key(&mut self, key: KeyEvent, term_size: Size) -> Option<Outcome> {
        // Ctrl-C always aborts, even mid-input.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Some(Outcome {
                verdict: Verdict::Cancelled,
                summary: Some("reviewer cancelled".into()),
                annotations: Vec::new(),
            });
        }

        // Taken out (rather than borrowed via `as_mut`) so the Comment arm
        // below is free to call `self.files()` / mutate `self.pending`
        // without fighting the borrow checker over an outstanding borrow of
        // `self.input`.
        if let Some(mut mode) = self.input.take() {
            let mut close = false;
            let mut outcome = None;
            match &mut mode {
                InputMode::Summary { buf } => match key.code {
                    KeyCode::Enter => {
                        let text = buf.trim().to_string();
                        let summary = if text.is_empty() { None } else { Some(text) };
                        outcome = Some(Outcome {
                            verdict: Verdict::RequestChanges,
                            summary,
                            annotations: self.pending_annotations(),
                        });
                        close = true;
                    }
                    KeyCode::Esc => close = true,
                    KeyCode::Backspace => {
                        buf.pop();
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        buf.push(c)
                    }
                    _ => {}
                },
                InputMode::Comment { buf, tag, editing, row_start, row_end } => match key.code {
                    KeyCode::Char('t') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        *tag = Tag::next(*tag);
                    }
                    KeyCode::Enter => {
                        let text = buf.trim().to_string();
                        if !text.is_empty() {
                            let resolved = self
                                .files()
                                .get(self.nav.selected)
                                .map(|file| (file, flatten_rows(file)))
                                .and_then(|(file, rows)| {
                                    resolve_annotation(file, &rows, *row_start, *row_end, *tag, text)
                                });
                            if let Some(annotation) = resolved {
                                let item = PendingAnnotation {
                                    file_idx: self.nav.selected,
                                    row_start: *row_start,
                                    row_end: *row_end,
                                    annotation,
                                };
                                match *editing {
                                    Some(idx) => self.pending[idx] = item,
                                    None => self.pending.push(item),
                                }
                            }
                        }
                        close = true;
                    }
                    KeyCode::Esc => close = true,
                    KeyCode::Backspace => {
                        buf.pop();
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        buf.push(c)
                    }
                    _ => {}
                },
            }
            if !close {
                self.input = Some(mode);
            }
            return outcome;
        }

        // Global verdict keys (disabled while an input prompt is open, handled above).
        match key.code {
            KeyCode::Char('q') => {
                return Some(Outcome {
                    verdict: Verdict::Cancelled,
                    summary: Some("reviewer cancelled".into()),
                    annotations: Vec::new(),
                });
            }
            KeyCode::Esc => {
                // A live visual selection swallows Esc (clear it) rather
                // than cancelling the whole review.
                if self.visual_anchor.is_some() {
                    self.visual_anchor = None;
                    return None;
                }
                return Some(Outcome {
                    verdict: Verdict::Cancelled,
                    summary: Some("reviewer cancelled".into()),
                    annotations: Vec::new(),
                });
            }
            KeyCode::Char('a') => {
                return Some(Outcome {
                    verdict: Verdict::Approve,
                    summary: None,
                    annotations: self.pending_annotations(),
                });
            }
            KeyCode::Char('r') => {
                self.input = Some(InputMode::Summary { buf: String::new() });
                return None;
            }
            _ => {}
        }

        self.handle_nav_key(key, term_size);
        None
    }

    /// `c` in diff focus, input closed: open the comment prompt. Uses the
    /// visual selection if a `v` anchor is set (and clears it); otherwise
    /// the cursor row alone — unless that row is already covered by a
    /// pending annotation on this file, in which case it opens in edit mode
    /// (prefilled, replacing rather than duplicating on save).
    fn open_comment_input(&mut self) {
        let cursor = self.diff.cursor;
        if let Some(anchor) = self.visual_anchor.take() {
            self.input = Some(InputMode::Comment {
                buf: String::new(),
                tag: None,
                editing: None,
                row_start: anchor.min(cursor),
                row_end: anchor.max(cursor),
            });
            return;
        }

        let selected = self.nav.selected;
        if let Some(idx) = self
            .pending
            .iter()
            .position(|p| p.file_idx == selected && p.row_start <= cursor && cursor <= p.row_end)
        {
            let p = &self.pending[idx];
            self.input = Some(InputMode::Comment {
                buf: p.annotation.comment.clone(),
                tag: p.annotation.tag.as_deref().and_then(Tag::from_label),
                editing: Some(idx),
                row_start: p.row_start,
                row_end: p.row_end,
            });
            return;
        }

        self.input = Some(InputMode::Comment {
            buf: String::new(),
            tag: None,
            editing: None,
            row_start: cursor,
            row_end: cursor,
        });
    }

    /// `x` in diff focus, input closed: delete the pending annotation
    /// covering the cursor row on the current file, if any.
    fn delete_pending_at_cursor(&mut self) {
        let cursor = self.diff.cursor;
        let selected = self.nav.selected;
        if let Some(idx) = self
            .pending
            .iter()
            .position(|p| p.file_idx == selected && p.row_start <= cursor && cursor <= p.row_end)
        {
            self.pending.remove(idx);
        }
    }

    fn handle_nav_key(&mut self, key: KeyEvent, term_size: Size) {
        match self.focus {
            Focus::Navigator => {
                let len = self.files().len();
                let before = self.nav.selected;
                match key.code {
                    KeyCode::Char('j') | KeyCode::Down => self.nav.down(len),
                    KeyCode::Char('k') | KeyCode::Up => self.nav.up(),
                    KeyCode::Char('g') => self.nav.top(),
                    KeyCode::Char('G') => self.nav.bottom(len),
                    KeyCode::Char('l') | KeyCode::Enter | KeyCode::Tab => {
                        self.focus = Focus::Diff
                    }
                    _ => {}
                }
                if self.nav.selected != before {
                    self.diff.reset();
                }
            }
            Focus::Diff => {
                let row_count = self.diff_rows().len();
                let viewport = diff_viewport_rows(term_size);
                match key.code {
                    KeyCode::Char('j') | KeyCode::Down => self.diff.down(row_count, viewport),
                    KeyCode::Char('k') | KeyCode::Up => self.diff.up(viewport),
                    KeyCode::Char('d') | KeyCode::PageDown => {
                        self.diff.page_down(half_page(term_size), row_count, viewport)
                    }
                    KeyCode::Char('u') | KeyCode::PageUp => {
                        self.diff.page_up(half_page(term_size), viewport)
                    }
                    KeyCode::Char('n') => {
                        self.diff.next_hunk(&hunk_row_indices(&self.diff_rows()), viewport)
                    }
                    KeyCode::Char('p') => {
                        self.diff.prev_hunk(&hunk_row_indices(&self.diff_rows()), viewport)
                    }
                    KeyCode::Char('g') => self.diff.top(),
                    KeyCode::Char('G') => self.diff.bottom(row_count, viewport),
                    KeyCode::Char('h') | KeyCode::Tab => self.focus = Focus::Navigator,
                    KeyCode::Char('v') => {
                        self.visual_anchor = match self.visual_anchor {
                            Some(_) => None,
                            None => Some(self.diff.cursor),
                        };
                    }
                    KeyCode::Char('c') => self.open_comment_input(),
                    KeyCode::Char('x') => self.delete_pending_at_cursor(),
                    _ => {}
                }
            }
        }
    }

    /// `path:line` for the cursor row, shown in the footer so the reviewer
    /// always knows where they are. Prefers the new-file line number; removed
    /// lines fall back to the old side, marked as such.
    fn cursor_position(&self) -> Option<String> {
        let file = self.selected_file()?;
        let rows = self.diff_rows();
        match rows.get(self.diff.cursor)? {
            DiffRow::Line(line) => match (line.new_no, line.old_no) {
                (Some(n), _) => Some(format!("{}:{}", file.path, n)),
                (None, Some(o)) => Some(format!("{}:{} (old)", file.path, o)),
                (None, None) => Some(file.path.clone()),
            },
            DiffRow::HunkHeader(_) => Some(format!("{} (hunk)", file.path)),
            DiffRow::Binary | DiffRow::NoContent => Some(file.path.clone()),
        }
    }
}

fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Length(1), // note
            Constraint::Min(1),    // body
            Constraint::Length(1), // footer
        ])
        .split(area);

    draw_header(frame, rows[0], app.request);
    draw_note(frame, rows[1], app.request, app.pending.len());
    draw_body(frame, rows[2], app);
    draw_footer(frame, rows[3], app);
}

fn draw_header(frame: &mut Frame, area: Rect, request: &ReviewRequest) {
    let title = format!(
        " REVIEW {} (vs {})",
        request.working_dir,
        request.baseline.as_deref().unwrap_or("uncommitted")
    );
    let header = Paragraph::new(title).style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_widget(header, area);
}

fn draw_note(frame: &mut Frame, area: Rect, request: &ReviewRequest, pending_count: usize) {
    let mut note = request
        .note
        .as_deref()
        .map(|n| format!(" agent: {n}"))
        .unwrap_or_else(|| " agent is waiting for your review".to_string());
    if pending_count > 0 {
        note.push_str(&format!(" \u{b7} {pending_count} annotation(s)"));
    }
    let note = Paragraph::new(note).style(Style::default().fg(Color::Yellow));
    frame.render_widget(note, area);
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    const GLOBAL_HINTS: &str = "a approve \u{b7} r request changes \u{b7} q cancel";
    let text = match &app.input {
        Some(InputMode::Summary { buf }) => format!(" request changes \u{2014} summary: {buf}"),
        Some(InputMode::Comment { buf, tag, .. }) => {
            let tag_label = tag.map(|t| t.label()).unwrap_or("none");
            format!(" comment [tag: {tag_label}]: {buf}")
        }
        None => match app.focus {
            Focus::Navigator => format!(
                " j/k move \u{b7} g/G first/last \u{b7} l/enter/tab focus diff \u{b7} {GLOBAL_HINTS}"
            ),
            Focus::Diff => {
                // Position first: when the footer clips in a narrow pane,
                // "where am I" survives and only the key hints get cut.
                let pos = app.cursor_position().unwrap_or_default();
                format!(
                    " {pos} \u{b7} j/k move \u{b7} d/u half page \u{b7} n/p hunk \u{b7} g/G top/bottom \u{b7} h/tab navigator \u{b7} v select \u{b7} c comment \u{b7} x delete \u{b7} {GLOBAL_HINTS}"
                )
            }
        },
    };
    let footer = Paragraph::new(text).style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_widget(footer, area);
}

fn draw_body(frame: &mut Frame, area: Rect, app: &App) {
    match app.model {
        Ok(model) if model.files.is_empty() => draw_empty_message(frame, area),
        Ok(model) => draw_panes(frame, area, app, &model.files),
        Err(err) => draw_panes_with_error(frame, area, app, err),
    }
}

fn draw_empty_message(frame: &mut Frame, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Fill(1), Constraint::Length(1), Constraint::Fill(1)])
        .split(area);
    let msg = Paragraph::new("(no changes vs baseline)").alignment(Alignment::Center);
    frame.render_widget(msg, rows[1]);
}

/// Below this width a side-by-side split leaves the diff no room for code
/// after the 24-col navigator minimum (herdr splits get narrow fast), so we
/// stack: navigator strip on top, diff below.
const STACK_THRESHOLD: u16 = 64;

fn body_split(area: Rect) -> (Rect, Rect) {
    if area.width < STACK_THRESHOLD {
        let nav_height = ((area.height as u32 * 30 / 100) as u16).clamp(4, 10).min(area.height);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(nav_height), Constraint::Min(0)])
            .split(area);
        return (rows[0], rows[1]);
    }
    let nav_width = ((area.width as u32 * 30 / 100) as u16).max(24).min(area.width);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(nav_width), Constraint::Min(0)])
        .split(area);
    (cols[0], cols[1])
}

fn pane_block(title: impl std::fmt::Display, focused: bool) -> Block<'static> {
    let (border_type, color) =
        if focused { (BorderType::Thick, Color::Cyan) } else { (BorderType::Plain, Color::DarkGray) };
    Block::default()
        .borders(Borders::ALL)
        .border_type(border_type)
        .border_style(Style::default().fg(color))
        .title(format!(" {title} "))
}

fn draw_panes(frame: &mut Frame, area: Rect, app: &App, files: &[FileDiff]) {
    let (nav_area, diff_area) = body_split(area);
    draw_navigator(frame, nav_area, app, files);
    draw_diff(frame, diff_area, app, files.get(app.nav.selected));
}

fn draw_panes_with_error(frame: &mut Frame, area: Rect, app: &App, err: &anyhow::Error) {
    let (nav_area, diff_area) = body_split(area);
    draw_navigator(frame, nav_area, app, &[]);

    let block = pane_block("diff", app.focus == Focus::Diff);
    let text = Paragraph::new(format!("{err:#}"))
        .style(Style::default().fg(Color::Red))
        .wrap(Wrap { trim: false })
        .block(block);
    frame.render_widget(text, diff_area);
}

fn draw_navigator(frame: &mut Frame, area: Rect, app: &App, files: &[FileDiff]) {
    let block = pane_block(format!("files ({})", files.len()), app.focus == Focus::Navigator);
    let items: Vec<ListItem> = files.iter().map(nav_item).collect();
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    let mut state = ListState::default();
    if !files.is_empty() {
        state.select(Some(app.nav.selected));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn nav_item(file: &FileDiff) -> ListItem<'_> {
    let line = Line::from(vec![
        Span::styled(
            format!("{} ", file.status.marker()),
            Style::default().fg(marker_color(file.status)).add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("{} ", file_display_path(file))),
        Span::styled(format!("+{}", file.adds), Style::default().fg(Color::Green)),
        Span::raw(" "),
        Span::styled(format!("-{}", file.dels), Style::default().fg(Color::Red)),
    ]);
    ListItem::new(line)
}

fn draw_diff(frame: &mut Frame, area: Rect, app: &App, file: Option<&FileDiff>) {
    let title = file.map(file_display_path).unwrap_or_else(|| "diff".to_string());
    let block = pane_block(title, app.focus == Focus::Diff);

    if file.is_none() {
        frame.render_widget(block, area);
        return;
    }

    // Pre-highlighted in `App::new`; drawing just clones the cached, owned
    // lines rather than re-running syntect on every frame.
    let mut lines: Vec<Line> = app.row_cache.get(&app.nav.selected).cloned().unwrap_or_default();

    // Gutter markers: one per row covered by a pending annotation on this
    // file, colored by tag. Overwrites the first character of the gutter
    // span in place so the column width doesn't shift.
    for pending in app.pending.iter().filter(|p| p.file_idx == app.nav.selected) {
        let color = tag_color(pending.annotation.tag.as_deref());
        for row in pending.row_start..=pending.row_end {
            if let Some(line) = lines.get_mut(row) {
                if let Some(span) = line.spans.first_mut() {
                    let mut chars: Vec<char> = span.content.chars().collect();
                    if let Some(first) = chars.first_mut() {
                        *first = '\u{25cf}'; // ●
                    }
                    span.content = chars.into_iter().collect::<String>().into();
                    span.style = span.style.fg(color);
                }
            }
        }
    }

    // Visual selection: apply before the cursor overlay so the cursor row's
    // background wins where the two overlap.
    if let Some(anchor) = app.visual_anchor {
        let (start, end) = (anchor.min(app.diff.cursor), anchor.max(app.diff.cursor));
        for row in start..=end {
            if let Some(line) = lines.get_mut(row) {
                for span in &mut line.spans {
                    span.style = span.style.bg(VISUAL_SELECTION_BG);
                }
            }
        }
    }

    // Cursor row: overlay a background on every span (overriding add/remove
    // tints — visibility of "where am I" beats the origin tint for one row).
    if let Some(line) = lines.get_mut(app.diff.cursor) {
        for span in &mut line.spans {
            span.style = span.style.bg(CURSOR_BG);
        }
    }
    let paragraph = Paragraph::new(lines).block(block).scroll((app.diff.scroll as u16, 0));
    frame.render_widget(paragraph, area);
}

/// Cursor-row background: a cool slate that reads over both the plain ground
/// and the add/remove tints.
const CURSOR_BG: Color = Color::Rgb(48, 54, 72);

/// Visual-selection background: a warm, muted amber distinct from both the
/// cursor slate and the add/remove tints.
const VISUAL_SELECTION_BG: Color = Color::Rgb(60, 45, 25);

/// Add-line content background (dark green).
const ADD_BG: Color = Color::Rgb(0, 60, 0);
/// Remove-line content background (dark red).
const REMOVE_BG: Color = Color::Rgb(70, 0, 0);

/// Highlight a whole file's flattened diff rows into owned, display-ready
/// `Line`s. Run once per file (see `App`'s row cache) rather than per draw
/// frame: `syntect` highlighting is real parsing work, not something to
/// repeat 30+ times a second while the reviewer scrolls.
///
/// Highlighting runs hunk-by-hunk: each hunk gets a fresh `HighlightLines`
/// seeded with the file's syntax, fed context/add/remove lines in order.
/// This is an approximation (the hunk mixes two file states, old and new)
/// but keeps highlighter state coherent within a hunk without needing to
/// reconstruct the two full file sides.
fn highlight_file_rows(hl: &'static Highlighter, file: &FileDiff) -> Vec<Line<'static>> {
    let syntax = syntax_for_path(hl, &file.path);
    let rows = flatten_rows(file);
    let mut out = Vec::with_capacity(rows.len());
    let mut hunk_hl: Option<HighlightLines<'static>> = None;

    for row in &rows {
        match row {
            DiffRow::HunkHeader(header) => {
                hunk_hl = Some(HighlightLines::new(syntax, &hl.theme));
                out.push(Line::styled((*header).to_string(), Style::default().fg(Color::Cyan)));
            }
            DiffRow::Binary => {
                out.push(Line::styled("(binary file)", Style::default().fg(Color::DarkGray)))
            }
            DiffRow::NoContent => {
                out.push(Line::styled("(no content)", Style::default().fg(Color::DarkGray)))
            }
            DiffRow::Line(line) => {
                let state = hunk_hl.get_or_insert_with(|| HighlightLines::new(syntax, &hl.theme));
                out.push(highlight_diff_line(hl, state, line));
            }
        }
    }
    out
}

/// Highlight one diff line's content and compose it with the existing
/// gutter/marker signaling: a dim line-number gutter, an origin-colored
/// `+`/`-`/` ` marker, then syntect-colored content spans. Add/remove lines
/// get a background tint on the marker and content (not the gutter) so the
/// signal reads at a glance without drowning per-token foreground colors.
fn highlight_diff_line(
    hl: &'static Highlighter,
    state: &mut HighlightLines<'static>,
    line: &DiffLine,
) -> Line<'static> {
    let old_str = line.old_no.map(|n| n.to_string()).unwrap_or_default();
    let new_str = line.new_no.map(|n| n.to_string()).unwrap_or_default();
    let gutter = format!("{old_str:>4} {new_str:>4} ");

    let (marker, marker_fg, bg) = match line.origin {
        Origin::Add => ("+", Color::Green, Some(ADD_BG)),
        Origin::Remove => ("-", Color::Red, Some(REMOVE_BG)),
        Origin::Context => (" ", Color::Reset, None),
    };

    let mut marker_style = Style::default().fg(marker_fg);
    if let Some(bg) = bg {
        marker_style = marker_style.bg(bg);
    }

    let mut spans = vec![
        Span::styled(gutter, Style::default().fg(Color::DarkGray)),
        Span::styled(marker.to_string(), marker_style),
    ];

    // syntect's `highlight_line` needs a trailing newline for correct state
    // tracking; feed it one and strip it back out of the resulting spans.
    let mut fed = line.content.clone();
    fed.push('\n');
    let ranges = state.highlight_line(&fed, &hl.syntax_set).unwrap_or_default();
    for (style, text) in ranges {
        let text = text.strip_suffix('\n').unwrap_or(text);
        if text.is_empty() {
            continue;
        }
        let fg = Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);
        let mut span_style = Style::default().fg(fg);
        if let Some(bg) = bg {
            span_style = span_style.bg(bg);
        }
        spans.push(Span::styled(text.to_string(), span_style));
    }

    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::Hunk;

    fn line(origin: Origin, old_no: Option<u32>, new_no: Option<u32>, content: &str) -> DiffLine {
        DiffLine { origin, old_no, new_no, content: content.to_string() }
    }

    fn sample_file() -> FileDiff {
        FileDiff {
            path: "src/lib.rs".to_string(),
            old_path: None,
            status: FileStatus::Modified,
            binary: false,
            adds: 2,
            dels: 1,
            hunks: vec![
                Hunk {
                    header: "@@ -1,3 +1,4 @@".to_string(),
                    old_start: 1,
                    old_count: 3,
                    new_start: 1,
                    new_count: 4,
                    lines: vec![
                        line(Origin::Context, Some(1), Some(1), "fn main() {"),
                        line(Origin::Add, None, Some(2), "    setup();"),
                        line(Origin::Context, Some(2), Some(3), "    run();"),
                    ],
                },
                Hunk {
                    header: "@@ -10,2 +11,3 @@".to_string(),
                    old_start: 10,
                    old_count: 2,
                    new_start: 11,
                    new_count: 3,
                    lines: vec![
                        line(Origin::Remove, Some(10), None, "    old();"),
                        line(Origin::Add, None, Some(11), "    new();"),
                        line(Origin::Context, Some(11), Some(12), "}"),
                    ],
                },
            ],
        }
    }

    #[test]
    fn flatten_rows_orders_headers_before_their_lines() {
        let file = sample_file();
        let rows = flatten_rows(&file);
        // 2 headers + 3 lines each = 8 rows.
        assert_eq!(rows.len(), 8);
        assert!(matches!(rows[0], DiffRow::HunkHeader(h) if h == "@@ -1,3 +1,4 @@"));
        assert!(matches!(rows[1], DiffRow::Line(_)));
        assert!(matches!(rows[4], DiffRow::HunkHeader(h) if h == "@@ -10,2 +11,3 @@"));
    }

    #[test]
    fn flatten_rows_binary_and_empty_collapse_to_one_row() {
        let mut file = sample_file();
        file.binary = true;
        assert!(matches!(flatten_rows(&file)[..], [DiffRow::Binary]));

        file.binary = false;
        file.hunks.clear();
        assert!(matches!(flatten_rows(&file)[..], [DiffRow::NoContent]));
    }

    #[test]
    fn hunk_row_indices_matches_header_positions() {
        let file = sample_file();
        let rows = flatten_rows(&file);
        assert_eq!(hunk_row_indices(&rows), vec![0, 4]);
    }

    #[test]
    fn next_and_prev_hunk_jump_to_neighboring_headers() {
        let hunks = vec![0usize, 4];
        let mut state = DiffViewState { scroll: 0, cursor: 0 };
        let vp = 10;

        // From the first header, next jumps the CURSOR to the second header;
        // a further next is a no-op.
        state.next_hunk(&hunks, vp);
        assert_eq!(state.cursor, 4);
        state.next_hunk(&hunks, vp);
        assert_eq!(state.cursor, 4);

        // From a row between headers, prev jumps back to the nearest header before it.
        state.cursor = 6;
        state.prev_hunk(&hunks, vp);
        assert_eq!(state.cursor, 4);
        state.prev_hunk(&hunks, vp);
        assert_eq!(state.cursor, 0);
        // No header before row 0: no-op.
        state.prev_hunk(&hunks, vp);
        assert_eq!(state.cursor, 0);
    }

    #[test]
    fn diff_view_cursor_clamps_at_both_ends() {
        let mut state = DiffViewState { scroll: 0, cursor: 0 };
        let vp = 10;
        state.up(vp); // already at 0, saturating
        assert_eq!(state.cursor, 0);

        state.down(3, vp); // row_count 3 -> max cursor index 2
        state.down(3, vp);
        state.down(3, vp);
        assert_eq!(state.cursor, 2);

        state.page_up(10, vp);
        assert_eq!(state.cursor, 0);

        state.page_down(10, 3, vp);
        assert_eq!(state.cursor, 2);

        state.bottom(3, vp);
        assert_eq!(state.cursor, 2);
        state.top();
        assert_eq!(state.cursor, 0);
    }

    #[test]
    fn scroll_follows_cursor_at_both_edges() {
        let mut state = DiffViewState { scroll: 0, cursor: 0 };
        let (rows, vp) = (100, 10);

        // Moving below the viewport bottom drags scroll down so the cursor
        // stays the last visible row.
        for _ in 0..15 {
            state.down(rows, vp);
        }
        assert_eq!(state.cursor, 15);
        assert_eq!(state.scroll, 6); // 15 - 10 + 1

        // Inside the viewport: scroll stays put.
        state.up(vp);
        assert_eq!((state.cursor, state.scroll), (14, 6));

        // Moving above the viewport top drags scroll up to the cursor.
        state.cursor = 6;
        state.up(vp);
        assert_eq!((state.cursor, state.scroll), (5, 5));

        // A far jump (G) lands the cursor on the last row, viewport showing
        // the tail.
        state.bottom(rows, vp);
        assert_eq!((state.cursor, state.scroll), (99, 90));
    }

    #[test]
    fn nav_state_clamps_to_file_count() {
        let mut nav = NavState::default();
        nav.up(); // saturating at 0
        assert_eq!(nav.selected, 0);

        nav.down(2);
        assert_eq!(nav.selected, 1);
        nav.down(2); // already at last index (1)
        assert_eq!(nav.selected, 1);

        nav.bottom(5);
        assert_eq!(nav.selected, 4);
        nav.top();
        assert_eq!(nav.selected, 0);

        // No files: selection pinned to 0.
        nav.down(0);
        assert_eq!(nav.selected, 0);
    }

    #[test]
    fn highlighted_rs_add_line_keeps_content_and_add_background() {
        let file = FileDiff {
            path: "src/lib.rs".to_string(),
            old_path: None,
            status: FileStatus::Modified,
            binary: false,
            adds: 1,
            dels: 0,
            hunks: vec![Hunk {
                header: "@@ -1,1 +1,2 @@".to_string(),
                old_start: 1,
                old_count: 1,
                new_start: 1,
                new_count: 2,
                lines: vec![line(Origin::Add, None, Some(2), "    let x = 1;")],
            }],
        };

        let rows = highlight_file_rows(highlighter(), &file);
        // rows[0] is the hunk header; rows[1] is the add line.
        assert_eq!(rows.len(), 2);
        let add_line = &rows[1];

        // Spans 0 and 1 are the gutter and the "+" marker; everything after
        // that is syntect's tokenization of the content. Concatenating them
        // back together must reproduce the original content exactly.
        let content: String =
            add_line.spans[2..].iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(content, "    let x = 1;");

        // Every content span (foreground comes from syntect) still carries
        // the add-line background tint.
        for span in &add_line.spans[2..] {
            assert_eq!(span.style.bg, Some(ADD_BG));
        }
        // The marker span carries it too.
        assert_eq!(add_line.spans[1].style.bg, Some(ADD_BG));
        // The gutter does not.
        assert_eq!(add_line.spans[0].style.bg, None);

        // Syntax highlighting is real: the content carries syntect RGB
        // foregrounds, and a Rust keyword line is not monochrome (the `let`
        // token differs from at least one other token's color).
        let fgs: Vec<_> = add_line.spans[2..]
            .iter()
            .map(|s| s.style.fg)
            .collect();
        assert!(
            fgs.iter().all(|fg| matches!(fg, Some(Color::Rgb(..)))),
            "expected RGB foregrounds from syntect, got {fgs:?}"
        );
        assert!(
            fgs.iter().collect::<std::collections::HashSet<_>>().len() > 1,
            "expected more than one distinct token color, got {fgs:?}"
        );
    }

    #[test]
    fn narrow_body_stacks_navigator_above_diff() {
        // 40 cols (the width the e2e run hit): side-by-side leaves no room
        // for code, so the split must go vertical.
        let (nav, diff) = body_split(Rect::new(0, 0, 40, 30));
        assert_eq!(nav.width, 40);
        assert_eq!(diff.width, 40);
        assert!(nav.height >= 4 && nav.height <= 10);
        assert_eq!(nav.y + nav.height, diff.y);

        // Wide pane keeps the horizontal split with the 24-col floor.
        let (nav, diff) = body_split(Rect::new(0, 0, 120, 30));
        assert_eq!(nav.height, 30);
        assert_eq!(nav.width, 36);
        assert_eq!(nav.x + nav.width, diff.x);
    }

    // --- M3 annotation flow ------------------------------------------

    fn sample_request() -> ReviewRequest {
        ReviewRequest {
            version: 1,
            working_dir: "/tmp/repo".to_string(),
            baseline: None,
            note: None,
        }
    }

    /// A hunk of two consecutive removed lines and nothing else, for
    /// exercising the remove-only (`Side::Old`) branch of `resolve_annotation`
    /// with a genuine min/max (not just a single row).
    fn remove_only_file() -> FileDiff {
        FileDiff {
            path: "src/gone.rs".to_string(),
            old_path: None,
            status: FileStatus::Modified,
            binary: false,
            adds: 0,
            dels: 2,
            hunks: vec![Hunk {
                header: "@@ -5,2 +5,0 @@".to_string(),
                old_start: 5,
                old_count: 2,
                new_start: 5,
                new_count: 0,
                lines: vec![
                    line(Origin::Remove, Some(5), None, "old_a();"),
                    line(Origin::Remove, Some(6), None, "old_b();"),
                ],
            }],
        }
    }

    #[test]
    fn resolve_annotation_mixed_range_prefers_new_side() {
        let file = sample_file();
        let rows = flatten_rows(&file);
        // rows[5..=7] (second hunk): Remove(old 10), Add(new 11), Context(old 11, new 12).
        let annotation = resolve_annotation(&file, &rows, 5, 7, None, "check this".to_string())
            .expect("range contains Line rows");
        assert_eq!(annotation.side, Side::New);
        assert_eq!(annotation.lines.start, 11);
        assert_eq!(annotation.lines.end, 12);
        assert_eq!(annotation.file, "src/lib.rs");
        assert_eq!(annotation.comment, "check this");
    }

    #[test]
    fn resolve_annotation_remove_only_range_resolves_to_old_side() {
        let file = remove_only_file();
        let rows = flatten_rows(&file);
        // rows[0] is the hunk header; rows[1..=2] are the two remove lines.
        let annotation = resolve_annotation(&file, &rows, 1, 2, Some(Tag::Fix), "dead code".to_string())
            .expect("remove-only range still resolves");
        assert_eq!(annotation.side, Side::Old);
        assert_eq!(annotation.lines.start, 5);
        assert_eq!(annotation.lines.end, 6);
        assert_eq!(annotation.tag.as_deref(), Some("fix"));
    }

    #[test]
    fn resolve_annotation_header_only_range_returns_none() {
        let file = sample_file();
        let rows = flatten_rows(&file);
        // rows[0] is a HunkHeader; no DiffRow::Line in a single-header range.
        assert!(resolve_annotation(&file, &rows, 0, 0, None, "nope".to_string()).is_none());
    }

    #[test]
    fn editing_existing_annotation_replaces_rather_than_duplicates() {
        let request = sample_request();
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![sample_file()] });
        let mut app = App::new(&request, &model);
        app.focus = Focus::Diff;
        let size = Size::new(120, 40);

        // Move the cursor onto a Line row (row 1: the first line of hunk 1).
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), size);
        assert_eq!(app.diff.cursor, 1);

        // `c` with no anchor and nothing pending: fresh comment at the cursor row.
        assert!(app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), size).is_none());
        for ch in "first".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), size);
        }
        assert!(app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), size).is_none());
        assert_eq!(app.pending.len(), 1);
        assert_eq!(app.pending[0].annotation.comment, "first");

        // `c` again at the same cursor row: the existing annotation covers
        // it, so this must open in edit mode, prefilled.
        assert!(app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), size).is_none());
        match &app.input {
            Some(InputMode::Comment { editing, buf, .. }) => {
                assert_eq!(*editing, Some(0));
                assert_eq!(buf, "first");
            }
            other => panic!("expected comment input in edit mode, got a different state (variant present: {})", other.is_some()),
        }

        // Replace the text and save: must overwrite pending[0], not append.
        for _ in 0.."first".chars().count() {
            app.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), size);
        }
        for ch in "second".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), size);
        }
        assert!(app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), size).is_none());

        assert_eq!(app.pending.len(), 1, "editing must replace, not duplicate");
        assert_eq!(app.pending[0].annotation.comment, "second");
    }

    #[test]
    fn esc_clears_visual_anchor_without_cancelling_but_q_still_cancels() {
        let request = sample_request();
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![sample_file()] });
        let mut app = App::new(&request, &model);
        app.focus = Focus::Diff;
        let size = Size::new(120, 40);

        // `v` sets the anchor; no outcome.
        assert!(app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE), size).is_none());
        assert!(app.visual_anchor.is_some());

        // Esc with a live anchor clears it and does NOT cancel the review.
        assert!(app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), size).is_none());
        assert!(app.visual_anchor.is_none());

        // A second Esc, now with no anchor, would cancel — but `q` always cancels.
        let outcome = app
            .handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), size)
            .expect("q cancels");
        assert_eq!(outcome.verdict, Verdict::Cancelled);
        assert!(outcome.annotations.is_empty());
    }
}
