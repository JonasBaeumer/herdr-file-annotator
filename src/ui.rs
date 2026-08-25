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
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect, Size},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame, Terminal,
};
use syntect::easy::HighlightLines;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

use crate::diff::{load_source, DiffLine, DiffModel, FileDiff, FileStatus, Origin};
use crate::protocol::{Annotation, GotoTarget, LineRange, ReviewRequest, Side, Verdict};

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
pub fn run(
    request: &ReviewRequest,
    model: Result<DiffModel>,
    goto_rx: std::sync::mpsc::Receiver<GotoTarget>,
) -> Result<Outcome> {
    let _guard = TermGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut app = App::new(request, &model);

    loop {
        terminal.draw(|frame| draw(frame, &app))?;
        // Agent-pushed navigation (guided walkthroughs) arrives between
        // keystrokes; drain it before waiting on input, and poll rather
        // than block so pushes render within a tick even when the
        // reviewer's hands are off the keyboard. A disconnected channel
        // means the server shut the socket (timeout, or the agent process
        // exited): the verdict can no longer be delivered, so keeping the
        // pane alive would let the reviewer finish a review into the void —
        // exit as cancelled instead.
        let size = terminal.size()?;
        if drain_navigation(&mut app, &goto_rx, size) {
            return Ok(Outcome {
                verdict: Verdict::Cancelled,
                summary: Some("agent disconnected; the review could not be delivered".into()),
                annotations: Vec::new(),
            });
        }
        if !event::poll(std::time::Duration::from_millis(50))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                let size = terminal.size()?;
                if let Some(outcome) = app.handle_key(key, size) {
                    return Ok(outcome);
                }
            }
            Event::Mouse(mouse) => {
                let size = terminal.size()?;
                app.handle_mouse(mouse, size);
            }
            Event::Resize(width, height) => {
                // Same reflow need as the navigator toggle: a smaller
                // viewport must not strand the cursor off-screen.
                app.ensure_cursor_visible(Size { width, height });
            }
            // Release events, etc: just redraw next iteration.
            _ => {}
        }
    }
}

/// Apply all pending navigation pushes. Returns true when the channel is
/// DISCONNECTED — the goto-reader thread ended because the server closed the
/// socket — which the caller must treat as "this review can no longer be
/// delivered", distinct from the ordinary empty-channel case.
fn drain_navigation(
    app: &mut App,
    goto_rx: &std::sync::mpsc::Receiver<GotoTarget>,
    size: Size,
) -> bool {
    loop {
        match goto_rx.try_recv() {
            Ok(target) => app.apply_goto(&target, size),
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return true,
        }
    }
}

/// RAII guard so a panic can't leave the terminal in raw/alternate mode.
struct TermGuard;

impl TermGuard {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen, cursor::Hide, EnableMouseCapture)?;
        Ok(TermGuard)
    }
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableMouseCapture, cursor::Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

/// Which body pane currently has keyboard focus.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Navigator,
    Diff,
}

/// What the right-hand pane renders: the unified diff, or the selected
/// file's post-change source. Global rather than per-file — `t` switches the
/// whole review's reading mode, and carrying it per file would make the same
/// key mean different things on different rows of the navigator.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum ViewMode {
    Diff,
    Source,
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
    /// Horizontal pan (columns of code shifted off the left edge); the
    /// line-number gutter and origin marker stay pinned.
    hscroll: usize,
}

impl DiffViewState {
    fn reset(&mut self) {
        self.scroll = 0;
        self.hscroll = 0;
        self.cursor = 0;
    }

    fn down(&mut self, row_count: usize) {
        self.cursor = (self.cursor + 1).min(row_count.saturating_sub(1));
    }

    fn up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn page_down(&mut self, page: usize, row_count: usize) {
        self.cursor = (self.cursor + page).min(row_count.saturating_sub(1));
    }

    fn page_up(&mut self, page: usize) {
        self.cursor = self.cursor.saturating_sub(page);
    }

    fn top(&mut self) {
        self.cursor = 0;
        self.scroll = 0;
    }

    fn bottom(&mut self, row_count: usize) {
        self.cursor = row_count.saturating_sub(1);
    }

    /// Move the cursor to the next hunk-header row strictly after it.
    /// No-op if there is none.
    fn next_hunk(&mut self, hunk_rows: &[usize]) {
        if let Some(&next) = hunk_rows.iter().find(|&&r| r > self.cursor) {
            self.cursor = next;
        }
    }

    /// Move the cursor to the previous hunk-header row strictly before it.
    /// No-op if there is none.
    fn prev_hunk(&mut self, hunk_rows: &[usize]) {
        if let Some(&prev) = hunk_rows.iter().rev().find(|&&r| r < self.cursor) {
            self.cursor = prev;
        }
    }
}

/// Mapping between BASE rows (the flattened diff rows the cursor moves over,
/// what annotations anchor to) and DISPLAY rows (base rows plus the inline
/// comment rows woven in directly under the lines they annotate). The cursor
/// lives in base space; the scroll offset lives in display space so the
/// rendered `Paragraph` can be scrolled directly.
struct DispMap {
    /// One entry per inline comment row: the base row it hangs under.
    /// Sorted ascending; duplicates mean several comments under one row.
    /// Every entry is a VISIBLE base row — the builder skips comments whose
    /// anchor rows are folded away.
    ends: Vec<usize>,
    /// Sorted, disjoint runs of base rows with NO display row at all: the
    /// TAILS of collapsed folds. A fold's head row stays in the display as
    /// its pill, so a run here never starts at row 0. Empty everywhere
    /// except a fold-bearing source view.
    hidden: Vec<(usize, usize)>,
}

impl DispMap {
    /// A map with no hidden rows — every view except a fold-bearing source
    /// view. (Tests use it heavily; production goes through `disp_map`.)
    #[cfg(test)]
    fn new(ends: Vec<usize>) -> Self {
        Self::with_hidden(ends, Vec::new())
    }

    fn with_hidden(mut ends: Vec<usize>, mut hidden: Vec<(usize, usize)>) -> Self {
        ends.sort_unstable();
        hidden.sort_unstable();
        DispMap { ends, hidden }
    }

    /// The nearest base row at or before `base` that HAS a display row:
    /// rows inside a hidden tail resolve to their fold's head (the pill).
    fn normalize(&self, base: usize) -> usize {
        for &(s, e) in &self.hidden {
            if s <= base && base <= e {
                return s.saturating_sub(1); // the fold head sits just above its tail
            }
        }
        base
    }

    /// Hidden rows strictly before `base` (callers pass a normalized base,
    /// so the partial-overlap arm is defensive only).
    fn hidden_before(&self, base: usize) -> usize {
        self.hidden
            .iter()
            .map(|&(s, e)| {
                if e < base {
                    e - s + 1
                } else if s < base {
                    base - s
                } else {
                    0
                }
            })
            .sum()
    }

    /// Display index of a base row: shifted UP for every hidden row above
    /// it and DOWN for every comment row hanging under an earlier base row.
    /// A base row inside a hidden tail maps to its fold's pill row.
    fn disp(&self, base: usize) -> usize {
        let b = self.normalize(base);
        b - self.hidden_before(b) + self.ends.iter().take_while(|&&e| e < b).count()
    }

    /// Number of comment rows hanging directly under this base row.
    fn extra_at(&self, base: usize) -> usize {
        self.ends.iter().filter(|&&e| e == base).count()
    }

    fn total(&self, base_count: usize) -> usize {
        let hidden: usize = self.hidden.iter().map(|&(s, e)| e - s + 1).sum();
        base_count - hidden.min(base_count) + self.ends.len()
    }

    /// Inverse mapping for mouse hits: the base row rendered at (or hanging
    /// above) a display row — clicking an inline comment resolves to the
    /// line it annotates, clicking a fold pill to the fold's head row.
    /// Returns the largest base row whose display index is <= `disp_row`,
    /// clamped to the file's rows. The walk starts from the LAST base row,
    /// not from `disp_row`: with hidden rows in play `disp(b)` can be far
    /// below `b`, so `disp_row` itself is no longer an upper bound for the
    /// answer.
    fn base_at(&self, disp_row: usize, base_count: usize) -> usize {
        if base_count == 0 {
            return 0;
        }
        let mut b = base_count - 1;
        while b > 0 && self.disp(b) > disp_row {
            b -= 1;
        }
        self.normalize(b)
    }
}

/// Minimum collapsed-run length worth a fold pill. Shorter gaps between
/// focus regions stay visible: a pill row that hides one or two rows saves
/// almost nothing and costs a click to see past.
const MIN_FOLD_LINES: usize = 3;

/// Derive the collapsed base-row runs (0-based, inclusive, sorted) for a
/// source file of `line_count` lines under agent-pushed focus `regions`
/// (1-based, inclusive, any order). Advisory input by protocol contract, so
/// this normalizes rather than errors: regions are clamped to the file,
/// degenerate ones dropped, and regions whose gap is shorter than
/// `MIN_FOLD_LINES` merge. The returned runs are the complement — the
/// stretches between regions that actually collapse. No regions surviving
/// the clamp folds nothing (the whole file stays visible).
fn fold_runs(regions: &[LineRange], line_count: usize) -> Vec<(usize, usize)> {
    if line_count == 0 || regions.is_empty() {
        return Vec::new();
    }
    let mut vis: Vec<(usize, usize)> = regions
        .iter()
        .filter(|r| r.start >= 1 && r.start <= r.end && (r.start as usize) <= line_count)
        .map(|r| ((r.start - 1) as usize, (r.end as usize).min(line_count) - 1))
        .collect();
    if vis.is_empty() {
        return Vec::new();
    }
    vis.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (s, e) in vis {
        match merged.last_mut() {
            Some((_, le)) if s <= *le + MIN_FOLD_LINES => *le = (*le).max(e),
            _ => merged.push((s, e)),
        }
    }
    let mut runs = Vec::new();
    let mut next_unfolded = 0usize;
    for &(s, e) in &merged {
        if s > next_unfolded && s - next_unfolded >= MIN_FOLD_LINES {
            runs.push((next_unfolded, s - 1));
        }
        next_unfolded = next_unfolded.max(e + 1);
    }
    if line_count > next_unfolded && line_count - next_unfolded >= MIN_FOLD_LINES {
        runs.push((next_unfolded, line_count - 1));
    }
    runs
}

/// The fold run whose rows contain `base` — as its pill head or inside its
/// hidden tail — if any.
fn fold_run_containing(runs: &[(usize, usize)], base: usize) -> Option<(usize, usize)> {
    runs.iter().copied().find(|&(s, e)| s <= base && base <= e)
}

/// Normalize a bag of fold runs into the sorted, disjoint form the display
/// math needs: overlapping and directly adjacent runs merge into one
/// (adjacent because two touching pills read as one collapsed stretch and
/// should cost one row, not two).
fn merge_runs(mut runs: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    runs.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (s, e) in runs {
        match merged.last_mut() {
            Some((_, le)) if s <= *le + 1 => *le = (*le).max(e),
            _ => merged.push((s, e)),
        }
    }
    merged
}

/// The foldable block UNDER `row`: the run of following lines strictly more
/// indented than `row`'s line, with interior blank lines belonging to the
/// block and trailing blank lines trimmed off it. `None` when `row` is
/// blank, out of range, or heads no block. Indentation is measured in
/// leading whitespace characters — comparisons stay within one file, where
/// indentation style is consistent, so tabs-vs-spaces width games don't
/// change any ordering this cares about.
///
/// DELIBERATE boundary (PR #17 review): the heuristic is structural, never
/// grammatical — it recognizes closing delimiters at base indent (below)
/// but no language keywords, so e.g. a rustfmt-style `where` clause at the
/// header's own indent ends the block early. Recognizing it would mean
/// hardcoding per-language keywords and invite endless special-casing;
/// the documented fallback for unrecognized layouts is selecting the rows
/// and folding with `v` + `f`.
fn indent_block_below(lines: &[String], row: usize) -> Option<(usize, usize)> {
    fn indent_of(line: &str) -> Option<usize> {
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            None // blank lines have no indent identity
        } else {
            Some(line.len() - trimmed.len())
        }
    }
    fn starts_closing(line: &str) -> bool {
        matches!(line.trim_start().chars().next(), Some(')' | ']' | '}'))
    }
    let base = indent_of(lines.get(row)?)?;
    let mut end = row; // last row CONFIRMED to belong to the block
    let mut i = row + 1;
    while i < lines.len() {
        match indent_of(&lines[i]) {
            // Blank: tentatively part of the block — confirmed only if a
            // deeper-indented line follows, otherwise it trails and trims.
            None => {}
            Some(ind) if ind > base => end = i,
            // A base-indent line that OPENS with a closing delimiter still
            // belongs to the block it closes — the `):` ending a multi-line
            // def, the `}` ending a braced body, a `} else {` chain link.
            // Without this the scan breaks at the closing delimiter of the
            // header's own signature and folds only the parameter lines.
            // (Only at exactly base indent: shallower means an OUTER scope
            // is closing, and that block this row does not belong to.)
            Some(ind) if ind == base && starts_closing(&lines[i]) => end = i,
            Some(_) => break,
        }
        i += 1;
    }
    (end > row).then_some((row + 1, end))
}

/// True when `base` is a fold HEAD: the row rendered as the pill.
fn is_fold_head(runs: &[(usize, usize)], base: usize) -> bool {
    runs.iter().any(|&(s, _)| s == base)
}

/// The single display row standing in for a collapsed fold. `hidden_lines`
/// counts the whole run (the head line's own content is behind the pill
/// too); `notes` is how many pending annotations touch the folded stretch.
fn fold_pill_line(hidden_lines: usize, notes: usize) -> Line<'static> {
    let notes_part = match notes {
        0 => String::new(),
        1 => " · 1 note".to_string(),
        n => format!(" · {n} notes"),
    };
    Line::from(Span::styled(
        format!("⋯ {hidden_lines} lines folded{notes_part} — enter/click expands ⋯"),
        Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
    ))
}

/// Display-space scroll follow: keep the cursor row AND any comment rows
/// hanging under it inside the viewport. Pure so it's directly testable.
fn follow_display(scroll: usize, cursor: usize, map: &DispMap, viewport: usize) -> usize {
    let viewport = viewport.max(1);
    let dc = map.disp(cursor);
    let tail = dc + map.extra_at(cursor);
    if dc < scroll {
        dc
    } else if tail >= scroll + viewport {
        (tail + 1).saturating_sub(viewport)
    } else {
        scroll
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
///
/// The protocol annotation's file/line range is the ONLY anchor stored: it is
/// what crosses the wire, and it is view-independent. Display anchors (which
/// rows to dot, where to weave the note) are derived per view on demand by
/// `App::pending_anchor`, so one annotation lands in the right place in both
/// the diff and the source view — and an annotation with no place in the
/// current view (an old-side one in source view, say) simply doesn't draw,
/// while still counting and still going back to the agent.
struct PendingAnnotation {
    file_idx: usize,
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

/// The inverse of `resolve_annotation` for the DIFF view: which flattened
/// rows an annotation covers. A row counts when it's a `DiffRow::Line` whose
/// number on the annotation's own side falls inside its line range; the
/// result is the (min, max) of those row indices. `None` means the
/// annotation has no place in this view at all — its lines aren't part of
/// any hunk (context far from the change, or the wrong side of a rename).
fn annotation_rows(annotation: &Annotation, rows: &[DiffRow]) -> Option<(usize, usize)> {
    let mut first: Option<usize> = None;
    let mut last: Option<usize> = None;
    for (idx, row) in rows.iter().enumerate() {
        let DiffRow::Line(line) = row else { continue };
        let side_no = match annotation.side {
            Side::New => line.new_no,
            Side::Old => line.old_no,
        };
        let Some(no) = side_no else { continue };
        if no >= annotation.lines.start && no <= annotation.lines.end {
            first.get_or_insert(idx);
            last = Some(idx);
        }
    }
    Some((first?, last?))
}

/// The same mapping for the SOURCE view, where it's pure arithmetic: row `i`
/// is line `i + 1` of the file on disk. Old-side annotations have no place
/// here (the source IS the new side), and neither does anything at all when
/// the file has no source view (`line_count == 0`). Ranges reaching past the
/// end of the file clamp to its last row rather than vanishing.
fn source_annotation_rows(annotation: &Annotation, line_count: usize) -> Option<(usize, usize)> {
    if annotation.side != Side::New || line_count == 0 {
        return None;
    }
    let last = line_count - 1;
    let start = (annotation.lines.start.max(1) as usize - 1).min(last);
    let end = (annotation.lines.end.max(1) as usize - 1).min(last).max(start);
    Some((start, end))
}

/// Per-view context for turning annotations into display anchors: the
/// flattened rows in diff view, the source file's line count in source view.
/// Built once per pass so a file full of annotations doesn't re-flatten the
/// diff once per annotation.
enum ViewAnchors<'a> {
    Diff(Vec<DiffRow<'a>>),
    Source(usize),
}

impl ViewAnchors<'_> {
    fn anchor(&self, annotation: &Annotation) -> Option<(usize, usize)> {
        match self {
            ViewAnchors::Diff(rows) => annotation_rows(annotation, rows),
            ViewAnchors::Source(line_count) => source_annotation_rows(annotation, *line_count),
        }
    }
}

/// Cursor remap, diff → source: the source row showing the same line. Rows
/// with no new-side line (removed lines, hunk headers, placeholders) have no
/// counterpart, so the cursor goes to the top of the file.
fn diff_row_to_source_row(rows: &[DiffRow], cursor: usize) -> usize {
    match rows.get(cursor) {
        Some(DiffRow::Line(line)) => {
            line.new_no.map(|n| n.saturating_sub(1) as usize).unwrap_or(0)
        }
        _ => 0,
    }
}

/// Cursor remap, source → diff: the first diff row showing that line. A line
/// outside every hunk isn't in the diff at all, so the cursor goes to the top.
fn source_row_to_diff_row(rows: &[DiffRow], cursor: usize) -> usize {
    let target = cursor as u32 + 1;
    rows.iter()
        .position(|row| matches!(row, DiffRow::Line(line) if line.new_no == Some(target)))
        .unwrap_or(0)
}

/// Visible diff rows, derived from the terminal size via the SAME split
/// logic as the real layout (header + note + footer chrome, then body_split,
/// then the diff pane's top/bottom border) — so cursor-following stays
/// correct in both the side-by-side and stacked layouts.
fn diff_viewport_rows(term_size: Size, show_navigator: bool, nav_width: u16) -> usize {
    let body = Rect::new(0, 0, term_size.width, term_size.height.saturating_sub(3));
    let (_, diff_area) = body_split(body, show_navigator, nav_width);
    (diff_area.height.saturating_sub(2) as usize).max(1)
}

/// Half a screen's worth of diff rows.
fn half_page(term_size: Size, show_navigator: bool, nav_width: u16) -> usize {
    (diff_viewport_rows(term_size, show_navigator, nav_width) / 2).max(1)
}

/// Display rows a wheel/trackpad tick scrolls the diff.
const WHEEL_STEP: usize = 3;

/// Columns one horizontal pan step shifts the code.
const HSCROLL_STEP: usize = 8;

/// Columns of the pinned lead on line rows: the 11-col line-number gutter
/// plus the 1-col origin marker (see `highlight_diff_line`'s format).
const GUTTER_AND_MARKER_COLS: usize = 12;

/// The navigator/diff rects for mouse hit-testing, mirroring `draw`'s
/// layout: header (1) + note (1) above the body, footer (1) below.
fn body_rects(term_size: Size, show_navigator: bool, nav_width: u16) -> (Rect, Rect) {
    let body = Rect::new(0, 2, term_size.width, term_size.height.saturating_sub(3));
    body_split(body, show_navigator, nav_width)
}

/// Inner (borderless) width of the diff pane — the wrap width for inline
/// comments; must match what `draw_diff` derives from its render area.
fn diff_inner_width(term_size: Size, show_navigator: bool, nav_width: u16) -> usize {
    let (_, diff_rect) = body_rects(term_size, show_navigator, nav_width);
    (diff_rect.width.saturating_sub(2)).max(1) as usize
}

/// Fit a buffer into `avail` columns keeping its END visible: long input
/// shows `…` plus the tail (where the caret is).
fn tail_fit(s: &str, avail: usize) -> String {
    let len = s.chars().count();
    if len <= avail {
        return s.to_string();
    }
    let keep = avail.saturating_sub(1);
    let tail: String = s.chars().skip(len - keep).collect();
    format!("\u{2026}{tail}")
}

fn rect_contains(rect: Rect, column: u16, row: u16) -> bool {
    column >= rect.x
        && column < rect.x + rect.width
        && row >= rect.y
        && row < rect.y + rect.height
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
    /// Agent-pushed gotos that arrived while an input bar was open, held in
    /// arrival order and replayed in that order the moment `input` goes back
    /// to `None`. `apply_goto` never disturbs an open input, but the targets
    /// are still valid and must not be silently lost. A queue, not a
    /// latest-only slot: goto pushes are stateFUL now that they can carry
    /// `focus` regions — an earlier held focus for one file must survive a
    /// later held goto for another, which replay-in-order gives for free.
    pending_gotos: Vec<GotoTarget>,
    /// Set by `v` in diff focus at the cursor row; the active selection is
    /// `min(anchor, cursor)..=max(anchor, cursor)` and grows/shrinks as the
    /// cursor moves. Diff-focus only; cleared by a second `v`, by `Esc`, or
    /// by opening a comment with `c`.
    visual_anchor: Option<usize>,
    /// Base row where a left-button press landed in the diff; a subsequent
    /// drag turns it into the visual anchor (mouse range selection).
    drag_origin: Option<usize>,
    /// `b` collapses the file navigator to give the diff the full width.
    show_navigator: bool,
    /// The reviewer's `[`/`]` navigator-width preference in columns; `0`
    /// means auto (the 30% default). Clamped at layout time in
    /// `body_split`, so it survives terminal resizes without going stale.
    nav_width: u16,
    /// True while a left-button drag that started on the navigator/diff
    /// divider is resizing the split — the drag moves the boundary instead
    /// of growing a selection. Cleared on button release.
    resizing_navigator: bool,
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
    /// Diff or source in the right-hand pane; `t` toggles.
    view: ViewMode,
    /// Highlighted source rows per file, keyed by index into `model.files`.
    /// `Err(reason)` for files that have no source view (deleted, binary,
    /// unreadable) — the reason is what the placeholder row shows. Unlike
    /// `row_cache` this is filled LAZILY, from key/mouse handlers only
    /// (`ensure_source_loaded`): reading every file off disk up front would
    /// be wasted I/O for a mode most reviews never enter, and `draw` borrows
    /// `App` immutably so it can never populate a cache itself.
    source_cache: HashMap<usize, Result<SourceFile, String>>,
    /// Per file, per view: the maximum horizontal pan (`pan_cap`'s value).
    /// The diff-view cap is precomputed once in `App::new` from the diff
    /// rows; the source-view cap is computed lazily in
    /// `ensure_source_loaded`, from that file's source rows, the first time
    /// its source view is entered — diff and source rows can differ wildly
    /// in width, so one cap per file isn't enough once a second view exists.
    pan_limit: HashMap<(usize, ViewMode), usize>,
    /// Each file's on-disk modification time when the review began,
    /// captured in `new` right after `diff::load` produced the model. The
    /// diff itself is a snapshot of that moment; source view instead reads
    /// the worktree fresh the first time `t` is pressed, which can be much
    /// later. If something touches the file in between, the two views
    /// would silently show different revisions and a source-view
    /// annotation's line numbers would no longer describe the diff the
    /// agent receives — `ensure_source_loaded` checks the current mtime
    /// against this baseline before trusting a lazy read. Absent entries
    /// (the initial stat failed) mean there's nothing to compare against,
    /// not that drift is impossible.
    source_baseline_mtime: HashMap<usize, std::time::SystemTime>,
    /// Whether the `?` key-reference overlay is showing. While true it owns
    /// the keyboard entirely (see `handle_key`): everything but `?`/`Esc`/
    /// `Enter`/`q` (close) and `j`/`k` (scroll) is swallowed, and — crucially
    /// — `q` here does NOT cancel the review the way it does everywhere
    /// else. Toggled by `?` only when no input bar is open, so a literal
    /// `?` can still be typed into a comment or summary.
    help_open: bool,
    /// Scroll offset, in overlay content rows, for the help overlay. Reset
    /// to 0 every time the overlay opens so it never reopens mid-scroll.
    help_scroll: usize,
    /// Agent-pushed focus per file (index into `model.files`): the raw
    /// 1-based regions from the last goto push that carried a `focus`.
    /// Fold runs derive from these lazily against the file's source line
    /// count, so a focus can arrive before the source is ever loaded (or
    /// for a file whose source never loads — it's simply inert there).
    focus_regions: HashMap<usize, Vec<LineRange>>,
    /// Folds the reviewer expanded by hand (Enter/click on a pill), per
    /// file: the exact runs subtracted from the derived set. Cleared when a
    /// new focus push replaces that file's regions — a fresh focus starts
    /// fresh.
    folds_expanded: HashMap<usize, Vec<(usize, usize)>>,
    /// Folds the reviewer created by hand (`f` on a selection or on a
    /// block header), per file: base-row runs collapsed in source view,
    /// independent of any agent focus. Normalized (sorted, merged) on
    /// insertion; `active_folds` unions them with the focus-derived runs.
    manual_folds: HashMap<usize, Vec<(usize, usize)>>,
}

/// The file's current modification time, or `None` if it can't be stat'd
/// (missing, permission error) — treated as "no signal" rather than an
/// error, matching how deleted/unreadable files are already handled
/// elsewhere in source view.
fn file_mtime(working_dir: &str, path: &str) -> Option<std::time::SystemTime> {
    std::fs::metadata(Path::new(working_dir).join(path)).ok()?.modified().ok()
}

/// One file's source view: its highlighted rows and how many lines it has
/// (the same number, kept alongside so row-count queries don't depend on the
/// rendered rows).
struct SourceFile {
    lines: Vec<Line<'static>>,
    count: usize,
    /// The raw source text, one entry per line — kept alongside the
    /// rendered rows because fold-by-indentation needs to measure leading
    /// whitespace, which the highlighted spans no longer expose cleanly.
    raw: Vec<String>,
}

/// The pinned-span count `pan_and_clip` uses for a row (gutter + origin
/// marker on line rows; nothing on single-span header/placeholder rows).
fn pinned_spans(line: &Line) -> usize {
    if line.spans.len() >= 3 {
        2
    } else {
        0
    }
}

/// A row's pannable display columns: its total width minus the pinned lead.
fn pannable_cols(line: &Line) -> usize {
    line.spans
        .iter()
        .skip(pinned_spans(line))
        .map(|s| str_cols(&s.content))
        .sum()
}

/// The display width of a row's final grapheme cluster in its pannable
/// (unpinned) content — scalar-sum width per the render model, so a
/// trailing family emoji reserves its full rendered footprint. Rows with
/// no pannable content default to 1 (nothing to protect).
fn trailing_cell_width(line: &Line) -> usize {
    let content: String =
        line.spans.iter().skip(pinned_spans(line)).map(|s| s.content.as_ref()).collect();
    content.graphemes(true).next_back().map(|g| str_cols(g).max(1)).unwrap_or(1)
}

/// The largest horizontal pan that still leaves the widest row's own final
/// character genuinely inspectable: enough short of that row's pannable
/// width for the `‹` clip marker (1 col) plus its actual trailing glyph
/// width (1 or 2 cols, not a flat worst-case 2) — otherwise the marker
/// replaces the last visible character, and on narrow panes a wasted
/// reserve column can put the true final character permanently out of
/// reach. A flat worst-case reservation of 3 would cost narrow panes the
/// one column a single-width trailing glyph didn't need reserved.
/// When several rows tie for the widest, reserves for whichever of THEM
/// has the widest trailing glyph, so panning to this cap is safe for all.
fn pan_cap_for_rows(rows: &[Line]) -> usize {
    let max_cols = rows.iter().map(pannable_cols).max().unwrap_or(0);
    let reserve = rows
        .iter()
        .filter(|r| pannable_cols(r) == max_cols)
        .map(|r| 1 + trailing_cell_width(r))
        .max()
        .unwrap_or(3);
    max_cols.saturating_sub(reserve)
}

impl<'a> App<'a> {
    fn new(request: &'a ReviewRequest, model: &'a Result<DiffModel>) -> Self {
        let mut row_cache = HashMap::new();
        let mut pan_limit = HashMap::new();
        let mut source_baseline_mtime = HashMap::new();
        if let Ok(m) = model {
            let hl = highlighter();
            for (i, file) in m.files.iter().enumerate() {
                let rows = highlight_file_rows(hl, file);
                pan_limit.insert((i, ViewMode::Diff), pan_cap_for_rows(&rows));
                row_cache.insert(i, rows);
                if let Some(mtime) = file_mtime(&request.working_dir, &file.path) {
                    source_baseline_mtime.insert(i, mtime);
                }
            }
        }
        App {
            request,
            model,
            focus: Focus::Navigator,
            nav: NavState::default(),
            diff: DiffViewState::default(),
            input: None,
            pending_gotos: Vec::new(),
            visual_anchor: None,
            drag_origin: None,
            show_navigator: true,
            nav_width: 0,
            resizing_navigator: false,
            pending: Vec::new(),
            row_cache,
            view: ViewMode::Diff,
            source_cache: HashMap::new(),
            pan_limit,
            source_baseline_mtime,
            help_open: false,
            help_scroll: 0,
            focus_regions: HashMap::new(),
            folds_expanded: HashMap::new(),
            manual_folds: HashMap::new(),
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

    /// Rows the pane currently addresses — what the cursor moves over and
    /// what mouse hits clamp to. Diff rows or source lines depending on the
    /// view; the `(no source view: …)` placeholder counts as one row.
    fn view_row_count(&self) -> usize {
        match self.view {
            ViewMode::Diff => self.diff_rows().len(),
            ViewMode::Source => self.source_row_count(),
        }
    }

    /// Lines in the selected file's source, or 0 when it has no source view
    /// (no file selected, deleted, binary, unreadable, or not yet loaded) —
    /// which is exactly the condition that makes source annotations inert.
    fn source_line_count(&self) -> usize {
        match self.source_cache.get(&self.nav.selected) {
            Some(Ok(source)) => source.count,
            _ => 0,
        }
    }

    fn source_row_count(&self) -> usize {
        if self.selected_file().is_none() {
            return 0;
        }
        match self.source_cache.get(&self.nav.selected) {
            Some(Ok(source)) => source.count,
            // No source view: the placeholder row still occupies one row.
            _ => 1,
        }
    }

    /// The pane's display rows for the current view, owned and ready to
    /// patch (gutter dots, selection, cursor) before rendering.
    fn view_lines(&self) -> Vec<Line<'static>> {
        match self.view {
            ViewMode::Diff => self.row_cache.get(&self.nav.selected).cloned().unwrap_or_default(),
            ViewMode::Source => match self.source_cache.get(&self.nav.selected) {
                Some(Ok(source)) => source.lines.clone(),
                Some(Err(reason)) => vec![source_placeholder_line(reason)],
                // Only reachable if a draw beats the handler that loads;
                // still renders something rather than an empty pane.
                None => vec![source_placeholder_line("not loaded")],
            },
        }
    }

    /// Context for resolving annotations to display rows in the current view.
    fn view_anchors(&self) -> ViewAnchors<'_> {
        match self.view {
            ViewMode::Diff => ViewAnchors::Diff(self.diff_rows()),
            ViewMode::Source => ViewAnchors::Source(self.source_line_count()),
        }
    }

    /// Display rows `pending[idx]` covers in the CURRENT view, or `None` when
    /// it belongs to another file or isn't representable here. The single
    /// place the view dispatch happens: gutter dots, note weaving, the
    /// display map, edit-under-cursor and delete all go through it.
    fn pending_anchor(&self, idx: usize) -> Option<(usize, usize)> {
        let pending = self.pending.get(idx)?;
        if pending.file_idx != self.nav.selected {
            return None;
        }
        self.view_anchors().anchor(&pending.annotation)
    }

    /// Index of the pending annotation covering `row` in the current view.
    fn pending_at_row(&self, row: usize) -> Option<usize> {
        let anchors = self.view_anchors();
        self.pending.iter().position(|p| {
            p.file_idx == self.nav.selected
                && matches!(anchors.anchor(&p.annotation), Some((start, end)) if start <= row && row <= end)
        })
    }

    /// All pending annotations, in creation order, cloned for handoff.
    fn pending_annotations(&self) -> Vec<Annotation> {
        self.pending.iter().map(|p| p.annotation.clone()).collect()
    }

    /// Load (once) the selected file's source rows into the cache. Called
    /// from handlers only — never from `draw`.
    fn ensure_source_loaded(&mut self) {
        let idx = self.nav.selected;
        if self.source_cache.contains_key(&idx) {
            return;
        }
        // Copied out so the immutable borrow of the model ends before the
        // cache insert below.
        let Some((path, status, binary)) =
            self.files().get(idx).map(|f| (f.path.clone(), f.status, f.binary))
        else {
            return;
        };
        let entry = if status == FileStatus::Deleted {
            Err("file deleted".to_string())
        } else if binary {
            Err("binary file".to_string())
        } else if self.drifted(idx, &path) {
            Err("file changed since the review started".to_string())
        } else {
            match load_source(&self.request.working_dir, &path) {
                Ok(lines) => {
                    let rows = highlight_source_rows(highlighter(), &path, &lines);
                    self.pan_limit.insert((idx, ViewMode::Source), pan_cap_for_rows(&rows));
                    Ok(SourceFile { lines: rows, count: lines.len(), raw: lines })
                }
                Err(err) => Err(err.root_cause().to_string()),
            }
        };
        self.source_cache.insert(idx, entry);
    }

    /// Whether file `idx`'s on-disk mtime no longer matches the baseline
    /// captured in `new` when the review began. `false` when there's no
    /// baseline to compare against (the initial stat failed) — that's a
    /// separate, unrelated failure mode, not evidence of drift.
    fn drifted(&self, idx: usize, path: &str) -> bool {
        match self.source_baseline_mtime.get(&idx) {
            Some(&baseline) => file_mtime(&self.request.working_dir, path) != Some(baseline),
            None => false,
        }
    }

    /// After the navigator selection changes: reset the pane and, in source
    /// view, make sure the newly selected file's source is in the cache.
    fn file_changed(&mut self) {
        self.diff.reset();
        if self.view == ViewMode::Source {
            self.ensure_source_loaded();
        }
    }

    /// Apply a push's focus payload to file `idx`'s stored focus state:
    /// `None` leaves it untouched, `Some(empty)` clears it, and
    /// `Some(regions)` replaces it wholesale and resets any hand-expanded
    /// folds — a fresh focus starts fresh.
    fn apply_focus(&mut self, idx: usize, focus: &Option<Vec<LineRange>>) {
        if let Some(regions) = focus {
            if regions.is_empty() {
                self.focus_regions.remove(&idx);
            } else {
                self.focus_regions.insert(idx, regions.clone());
            }
            self.folds_expanded.remove(&idx);
        }
    }

    /// Agent-pushed navigation: focus `target.file` at new-side line
    /// `target.line` in the CURRENT view. Advisory by contract — unknown
    /// files are ignored, out-of-range lines clamp, and in diff view a line
    /// outside the hunks lands on the nearest following changed/context row,
    /// or the last new-side row if none follows (never wraps to the top).
    /// Never disturbs an open input bar — while one is open the target is
    /// queued in `pending_gotos` and replayed once it closes, rather than
    /// lost.
    fn apply_goto(&mut self, target: &GotoTarget, term_size: Size) {
        if self.input.is_some() {
            // Queued and replayed in arrival order once the input closes.
            // Order matters because pushes are stateful: a held focus for
            // one file must survive a later held goto for any file, and
            // replaying the sequence exactly as the agent sent it needs no
            // special-casing of what each push carries.
            self.pending_gotos.push(target.clone());
            return;
        }
        let Ok(model) = self.model else { return };
        let Some(idx) = model.files.iter().position(|f| f.path == target.file) else {
            return;
        };
        // A line-0 push against a file the reviewer isn't currently looking
        // at only updates that file's stored focus (used to clear a stale
        // focus after the reviewer has already moved elsewhere) — it must
        // not steal the pane away to show that file, which file_changed()
        // below would do by switching nav.selected and resetting the cursor.
        if target.line == 0 && idx != self.nav.selected {
            self.apply_focus(idx, &target.focus);
            return;
        }
        if idx != self.nav.selected {
            self.nav.selected = idx;
            self.file_changed();
        }
        self.apply_focus(idx, &target.focus);
        // An explicit view request switches the pane before the line maps —
        // advisory like everything else: an unknown view string is ignored,
        // and a source request for a file with no usable source side
        // (deleted, binary, unreadable) is ignored too, keeping the useful
        // diff on screen. Any actual switch clears a live visual selection:
        // its anchor indexes the OLD view's row space, and combining it with
        // a new-space cursor would let `c` save an unrelated line range
        // (same rule as the manual `t` toggle).
        match target.view.as_deref() {
            Some("diff") if self.view != ViewMode::Diff => {
                self.view = ViewMode::Diff;
                self.visual_anchor = None;
                self.diff.reset();
            }
            Some("source") => {
                // The usability check must run for EVERY source request, not
                // only when entering source view: a pane already in source
                // mode steered to a deleted/binary/unreadable file would
                // otherwise show the placeholder instead of keeping the
                // useful diff, breaking the documented "ignored" contract.
                let started_in_source = self.view == ViewMode::Source;
                self.view = ViewMode::Source;
                self.ensure_source_loaded();
                if matches!(self.source_cache.get(&self.nav.selected), Some(Err(_))) {
                    self.view = ViewMode::Diff; // no usable source: request ignored
                }
                // Clear the selection only when the EFFECTIVE view changed
                // relative to where this push started — that's when the row
                // space under a live anchor shifts.
                if started_in_source != (self.view == ViewMode::Source) {
                    self.visual_anchor = None;
                    self.diff.reset();
                }
            }
            _ => {}
        }
        // `line: 0` is the don't-move-the-cursor sentinel (a focus-clearing
        // push shouldn't yank the pane to the top). The view/focus parts
        // above still applied; the cursor just stays put — snapped out of
        // any fold the new focus put under it.
        if target.line == 0 {
            self.snap_cursor_out_of_folds(true);
            self.focus = Focus::Diff;
            self.ensure_cursor_visible(term_size);
            return;
        }
        // A focus push lands on its first listed region AS NORMALIZED by
        // this pane, not as sent: `fold_runs` drops regions that miss the
        // file entirely, so landing on a dropped region's start would clamp
        // to the file's end and — worse — auto-expand the trailing fold the
        // surviving regions just created. Land on the first region that
        // intersects the file instead; a focus whose regions ALL miss the
        // file is the same ignored advisory input `fold_runs` already
        // treats it as — it folds nothing and moves nothing.
        let mut line = target.line;
        if let Some(regions) = &target.focus {
            if !regions.is_empty() && self.view == ViewMode::Source {
                let count = self.source_line_count() as u32;
                match regions.iter().find(|r| r.start >= 1 && r.start <= r.end && r.start <= count)
                {
                    Some(first) => line = first.start,
                    None => {
                        self.snap_cursor_out_of_folds(true);
                        self.focus = Focus::Diff;
                        self.ensure_cursor_visible(term_size);
                        return;
                    }
                }
            }
        }
        let row = match self.view {
            ViewMode::Source => {
                (line.saturating_sub(1) as usize).min(self.source_row_count().saturating_sub(1))
            }
            ViewMode::Diff => {
                let rows = self.diff_rows();
                rows.iter()
                    .position(
                        |r| matches!(r, DiffRow::Line(l) if l.new_no == Some(line)),
                    )
                    .or_else(|| {
                        rows.iter().position(
                            |r| matches!(r, DiffRow::Line(l) if l.new_no.is_some_and(|n| n >= line)),
                        )
                    })
                    // Past the last new-side line: clamp to it, matching
                    // source view's clamp-to-last-line rather than falling
                    // through to row 0 (which would read as "jumped to the
                    // top" for a target that was actually past the end).
                    .or_else(|| {
                        rows.iter().rposition(|r| matches!(r, DiffRow::Line(l) if l.new_no.is_some()))
                    })
                    .unwrap_or(0)
            }
        };
        self.diff.cursor = row;
        // A goto into a folded stretch reveals it — the agent must never
        // point the reviewer at a row the pane is hiding (the pill head
        // included: its own line's content is behind the pill too). Manual
        // folds count: hand-folded or agent-folded, hidden is hidden.
        let folds = self.active_folds();
        if let Some(run) = fold_run_containing(&folds, row) {
            self.expand_run(run);
        }
        self.focus = Focus::Diff;
        self.ensure_cursor_visible(term_size);
    }

    /// `t` in diff focus: switch views, carrying the cursor to the row
    /// showing the same line of code, and starting the new view unpanned.
    fn toggle_view(&mut self, term_size: Size) {
        match self.view {
            ViewMode::Diff => {
                let target = diff_row_to_source_row(&self.diff_rows(), self.diff.cursor);
                self.view = ViewMode::Source;
                self.ensure_source_loaded();
                self.diff.cursor = target.min(self.source_row_count().saturating_sub(1));
            }
            ViewMode::Source => {
                self.view = ViewMode::Diff;
                self.diff.cursor = source_row_to_diff_row(&self.diff_rows(), self.diff.cursor);
            }
        }
        // A live `v` selection is anchored in the OLD view's row space, so
        // it would paint an unrelated range here: drop it with the view.
        self.visual_anchor = None;
        // Entering source view can land the mapped cursor inside a fold
        // the agent focused earlier; continue past it rather than sit on a
        // hidden row.
        self.snap_cursor_out_of_folds(true);
        self.diff.hscroll = 0;
        self.diff.scroll = 0;
        self.ensure_cursor_visible(term_size);
    }

    /// Source-view counterpart to `resolve_annotation`: no row scan needed,
    /// since source row `i` simply IS new-side line `i + 1`.
    fn resolve_source_annotation(
        &self,
        row_start: usize,
        row_end: usize,
        tag: Option<Tag>,
        comment: String,
    ) -> Option<Annotation> {
        let file = self.selected_file()?;
        let count = self.source_line_count();
        if count == 0 {
            return None;
        }
        let start = row_start.min(count - 1);
        let end = row_end.min(count - 1).max(start);
        Some(Annotation {
            file: file.path.clone(),
            lines: LineRange { start: start as u32 + 1, end: end as u32 + 1 },
            side: Side::New,
            tag: tag.map(|t| t.label().to_string()),
            comment,
        })
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
                            match *editing {
                                // Editing keeps the original file/range/side
                                // — only the comment and tag change. Rebuilding
                                // the range from `row_start`/`row_end` here
                                // would re-derive it from whatever the CURRENT
                                // view's row span happens to be, which silently
                                // narrows or shifts it whenever the annotation
                                // was saved from a different view: a source
                                // selection can cover lines the diff never
                                // shows at all (far context, outside every
                                // hunk), so re-resolving from diff rows alone
                                // loses everything the diff doesn't display.
                                Some(idx) => {
                                    if let Some(p) = self.pending.get_mut(idx) {
                                        p.annotation.tag = tag.map(|t| t.label().to_string());
                                        p.annotation.comment = text;
                                    }
                                }
                                None => {
                                    // The comment's rows are rows of the
                                    // CURRENT view, so each view resolves
                                    // them its own way.
                                    let resolved = match self.view {
                                        ViewMode::Diff => self
                                            .files()
                                            .get(self.nav.selected)
                                            .map(|file| (file, flatten_rows(file)))
                                            .and_then(|(file, rows)| {
                                                resolve_annotation(
                                                    file, &rows, *row_start, *row_end, *tag, text,
                                                )
                                            }),
                                        ViewMode::Source => self.resolve_source_annotation(
                                            *row_start, *row_end, *tag, text,
                                        ),
                                    };
                                    if let Some(annotation) = resolved {
                                        self.pending.push(PendingAnnotation {
                                            file_idx: self.nav.selected,
                                            annotation,
                                        });
                                    }
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
            } else if !self.pending_gotos.is_empty() {
                // The input just closed: catch up, in arrival order, to
                // whatever navigation was held while the reviewer typed.
                for target in std::mem::take(&mut self.pending_gotos) {
                    self.apply_goto(&target, term_size);
                }
            }
            // Typing can grow the editing box (more wrapped rows) or close
            // it back down to nothing — either way the set of display rows
            // under the cursor just changed, so re-follow it now rather than
            // leaving the scroll offset stuck until the next nav key.
            self.ensure_cursor_visible(term_size);
            return outcome;
        }

        // The help overlay owns the keyboard while it's open: close keys
        // close it (note `q` does NOT fall through to the cancel arm below
        // — a reviewer dismissing the help must not accidentally cancel the
        // whole review), j/k scroll, everything else is swallowed rather
        // than reaching the nav/verdict handling underneath.
        if self.help_open {
            match key.code {
                KeyCode::Char('?') | KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                    self.help_open = false;
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    let max = self.help_max_scroll(term_size);
                    self.help_scroll = (self.help_scroll + 1).min(max);
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.help_scroll = self.help_scroll.saturating_sub(1);
                }
                _ => {}
            }
            return None;
        }

        // Global verdict keys (disabled while an input prompt is open, handled above).
        match key.code {
            KeyCode::Char('?') => {
                self.help_open = true;
                self.help_scroll = 0;
                return None;
            }
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
            KeyCode::Char('b') => {
                self.show_navigator = !self.show_navigator;
                if !self.show_navigator && self.focus == Focus::Navigator {
                    self.focus = Focus::Diff;
                }
                // The toggle changes the diff viewport (height in stacked
                // layout, wrap width everywhere): reflow immediately or the
                // cursor can sit outside the new viewport until the next key.
                self.ensure_cursor_visible(term_size);
                return None;
            }
            KeyCode::Char('z') => {
                if let Err(err) = crate::herdr::zoom_toggle_current() {
                    eprintln!("herdr-annotator pane: {err:#}");
                }
                return None;
            }
            _ => {}
        }

        self.handle_nav_key(key, term_size);
        self.ensure_cursor_visible(term_size);
        None
    }

    /// Index into `pending` of the annotation currently being edited, if
    /// any — the single source of truth for "which saved annotation's rows
    /// get replaced by the editing box at the same anchor". `disp_map` and
    /// `draw_diff` both need this to agree, or the rendered rows and the
    /// scroll/mouse display map disagree with each other.
    fn editing_annotation_idx(&self) -> Option<usize> {
        match &self.input {
            Some(InputMode::Comment { editing: Some(idx), .. }) => Some(*idx),
            _ => None,
        }
    }

    /// The display map for the currently selected file: saved annotations
    /// plus the editing box while the comment prompt is open (its rows
    /// occupy display space too, so scrolling must account for them).
    /// One `ends` entry per DISPLAY row a comment occupies — wrapped
    /// comments push several — so the scroll math sees their real height.
    /// `inner_width` is the diff pane's inner width (wrap width source).
    /// The collapsed fold runs (0-based base-row runs, sorted) for the
    /// selected file in the CURRENT view. Always empty in diff view — the
    /// diff already shows only its hunks — and for files whose source isn't
    /// usable (the focus stays stored, just inert).
    fn active_folds(&self) -> Vec<(usize, usize)> {
        if self.view != ViewMode::Source {
            return Vec::new();
        }
        let count = self.source_line_count();
        if count == 0 {
            return Vec::new();
        }
        let mut runs: Vec<(usize, usize)> = Vec::new();
        if let Some(regions) = self.focus_regions.get(&self.nav.selected) {
            let mut derived = fold_runs(regions, count);
            if let Some(expanded) = self.folds_expanded.get(&self.nav.selected) {
                derived.retain(|r| !expanded.contains(r));
            }
            runs.extend(derived);
        }
        if let Some(manual) = self.manual_folds.get(&self.nav.selected) {
            runs.extend(manual.iter().copied());
        }
        merge_runs(runs)
    }

    /// The selected file's raw source lines, when its source is usable.
    fn source_raw(&self) -> Option<&[String]> {
        match self.source_cache.get(&self.nav.selected) {
            Some(Ok(source)) => Some(&source.raw),
            _ => None,
        }
    }

    /// After a cursor move landed inside a fold's hidden tail: moving down
    /// continues to the first row past the fold (falling back to the pill
    /// when the fold reaches the end of the file); moving up stops on the
    /// pill. The pill head itself is a real cursor stop — that's how the
    /// keyboard reaches Enter-to-expand.
    fn snap_cursor_out_of_folds(&mut self, moved_down: bool) {
        let folds = self.active_folds();
        let Some((s, e)) = fold_run_containing(&folds, self.diff.cursor) else { return };
        if self.diff.cursor == s {
            return;
        }
        self.diff.cursor = if moved_down && e + 1 < self.view_row_count() { e + 1 } else { s };
    }

    /// Enter (or a click) on a fold pill: reveal that run. No-op anywhere
    /// else.
    fn expand_fold_at_cursor(&mut self) {
        let folds = self.active_folds();
        if let Some(run) = fold_run_containing(&folds, self.diff.cursor) {
            if run.0 == self.diff.cursor {
                self.expand_run(run);
            }
        }
    }

    /// Reveal one displayed run. A displayed run can be a merger of manual
    /// folds and focus-derived folds, so expansion dismantles every
    /// constituent it intersects: manual runs are deleted outright, derived
    /// runs go onto the expanded list `active_folds` subtracts.
    fn expand_run(&mut self, run: (usize, usize)) {
        let idx = self.nav.selected;
        let count = self.source_line_count();
        if let Some(manual) = self.manual_folds.get_mut(&idx) {
            manual.retain(|&(s, e)| e < run.0 || run.1 < s);
        }
        if self.manual_folds.get(&idx).is_some_and(|m| m.is_empty()) {
            self.manual_folds.remove(&idx);
        }
        if let Some(regions) = self.focus_regions.get(&idx) {
            let derived = fold_runs(regions, count);
            let expanded = self.folds_expanded.entry(idx).or_default();
            for d in derived.into_iter().filter(|&(s, e)| s <= run.1 && run.0 <= e) {
                if !expanded.contains(&d) {
                    expanded.push(d);
                }
            }
        }
    }

    /// `f` in source view: fold the visual selection if one is active,
    /// otherwise the indentation block UNDER the cursor line (the header
    /// line itself stays visible — `f` on a `def`/`fn` line tucks the body
    /// away). Runs shorter than `MIN_FOLD_LINES` don't fold, same as the
    /// focus path. No-op in diff view and on files with no usable source.
    fn fold_at_cursor_or_selection(&mut self) {
        if self.view != ViewMode::Source {
            return;
        }
        let count = self.source_line_count();
        if count == 0 {
            return;
        }
        let run = match self.visual_anchor {
            Some(anchor) => {
                let s = anchor.min(self.diff.cursor);
                let e = anchor.max(self.diff.cursor).min(count - 1);
                (s, e)
            }
            None => {
                let Some(raw) = self.source_raw() else { return };
                match indent_block_below(raw, self.diff.cursor) {
                    Some(run) => run,
                    None => return,
                }
            }
        };
        if run.1 - run.0 + 1 < MIN_FOLD_LINES {
            return;
        }
        self.visual_anchor = None;
        let entry = self.manual_folds.entry(self.nav.selected).or_default();
        entry.push(run);
        *entry = merge_runs(std::mem::take(entry));
        // A cursor inside the new fold's tail lands on its pill — for a
        // selection fold that's the collapsed range's head; a block fold
        // leaves the cursor on the still-visible header line above it.
        if let Some(r) = fold_run_containing(&self.active_folds(), self.diff.cursor) {
            self.diff.cursor = r.0;
        }
    }

    /// `F` in source view: reveal everything in this file — manual folds
    /// are deleted, focus-derived folds are all marked expanded. The stored
    /// focus regions survive, so a fresh agent push starts from its own
    /// clean slate rather than resurrecting what the reviewer dismissed.
    fn unfold_all(&mut self) {
        if self.view != ViewMode::Source {
            return;
        }
        let idx = self.nav.selected;
        self.manual_folds.remove(&idx);
        if let Some(regions) = self.focus_regions.get(&idx) {
            let derived = fold_runs(regions, self.source_line_count());
            let expanded = self.folds_expanded.entry(idx).or_default();
            for d in derived {
                if !expanded.contains(&d) {
                    expanded.push(d);
                }
            }
        }
    }

    fn disp_map(&self, inner_width: usize) -> DispMap {
        // While editing an existing annotation, its saved rows are replaced
        // by the box at the same anchor — count the box, not the saved text.
        let editing_idx = self.editing_annotation_idx();

        let folds = self.active_folds();
        let anchors = self.view_anchors();
        let mut ends: Vec<usize> = Vec::new();
        for (i, p) in self.pending.iter().enumerate().filter(|(_, p)| p.file_idx == self.nav.selected)
        {
            if Some(i) == editing_idx {
                continue;
            }
            // Annotations with no anchor in this view aren't drawn, so they
            // occupy no display rows either.
            let Some((_, row_end)) = anchors.anchor(&p.annotation) else { continue };
            // Same for annotations whose anchor row is folded away — the
            // pill's note badge stands in for them.
            if fold_run_containing(&folds, row_end).is_some() {
                continue;
            }
            let h = comment_height(p.annotation.tag.as_deref(), &p.annotation.comment, inner_width);
            ends.extend(std::iter::repeat(row_end).take(h));
        }
        if let Some(InputMode::Comment { buf, row_end, .. }) = &self.input {
            // Both a fresh comment and an edit weave the box (new AND edit).
            let h = editing_box_height(buf, inner_width);
            ends.extend(std::iter::repeat(*row_end).take(h));
        }
        DispMap::with_hidden(ends, folds.iter().map(|&(s, e)| (s + 1, e)).collect())
    }

    /// Largest useful horizontal pan for the selected file IN THE CURRENT
    /// VIEW — see `pan_cap_for_rows`. The diff-view cap is precomputed once
    /// per file in `App::new`; the source-view cap is computed lazily in
    /// `ensure_source_loaded` from that file's own (much wider or narrower)
    /// source rows, since a diff row's width says nothing about a source
    /// row's width.
    fn pan_cap(&self) -> usize {
        self.pan_limit.get(&(self.nav.selected, self.view)).copied().unwrap_or(0)
    }

    /// The horizontal-pan offset a single rightward step should land on: a
    /// full `HSCROLL_STEP` jump, UNLESS some row's pannable width sits
    /// strictly inside that jump. A short row would otherwise vanish from
    /// "showing its first few columns" straight to "fully panned off,
    /// empty" in one step — with a much longer row elsewhere in the same
    /// file supplying a big enough `pan_cap`, every offset that would have
    /// revealed the short row's remaining content becomes permanently
    /// unreachable (Right/Left only ever move in whole `HSCROLL_STEP`s).
    /// Stepping by a single column instead whenever that would happen keeps
    /// every row's content reachable, while long lines with no such
    /// short-row conflict still jump the full step.
    /// One pan step in columns: the full `HSCROLL_STEP` on normal panes,
    /// but never more than half the visible CODE columns — in a pane
    /// narrower than the step, whole-step jumps skip offsets that were
    /// never on screen (middle of short rows, tails at the cap), so narrow
    /// panes fine-step down to single columns.
    fn pan_step(&self, term_size: Size) -> usize {
        let code_cols = diff_inner_width(term_size, self.show_navigator, self.nav_width)
            .saturating_sub(GUTTER_AND_MARKER_COLS);
        HSCROLL_STEP.min((code_cols / 2).max(1))
    }

    fn next_pan_stop(&self, current: usize, step: usize) -> usize {
        let target = current + step;
        // The overshoot check must scan the rows actually ON SCREEN, not
        // always the diff's: in source view those are wholly different rows
        // (a source file's line widths have nothing to do with the diff's),
        // so checking `row_cache` there would miss a short SOURCE row's
        // overshoot entirely and jump the full step over it — the same bug
        // `short_rows_stay_reachable_when_a_longer_row_sets_a_big_pan_cap`
        // once pinned for diff rows, reappearing in the other view.
        let rows: Option<&Vec<Line<'static>>> = match self.view {
            ViewMode::Diff => self.row_cache.get(&self.nav.selected),
            ViewMode::Source => self
                .source_cache
                .get(&self.nav.selected)
                .and_then(|r| r.as_ref().ok())
                .map(|s| &s.lines),
        };
        // <= target, not < target: a row whose pannable width lands EXACTLY
        // on the step boundary is just as skipped-over as one strictly
        // inside it — landing there means every offset that would have
        // revealed that row's middle/tail (current+1..target-1) was never
        // visited, and `target` itself is already "fully panned off, empty"
        // for that row.
        let overshoots_a_row =
            rows.into_iter().flatten().map(pannable_cols).any(|cols| cols > current && cols <= target);
        if overshoots_a_row {
            current + 1
        } else {
            target
        }
    }

    /// Display-space scroll follow, run after every key that can move the
    /// cursor or change which comment rows exist.
    fn ensure_cursor_visible(&mut self, term_size: Size) {
        let map = self.disp_map(diff_inner_width(term_size, self.show_navigator, self.nav_width));
        self.diff.scroll = follow_display(
            self.diff.scroll,
            self.diff.cursor,
            &map,
            diff_viewport_rows(term_size, self.show_navigator, self.nav_width),
        );
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, term_size: Size) {
        // The help overlay swallows mouse input the same way it swallows
        // most keys — except the wheel, which scrolls it, since that's easy
        // to support and matches j/k.
        if self.help_open {
            if let MouseEventKind::ScrollDown | MouseEventKind::ScrollUp = mouse.kind {
                let max = self.help_max_scroll(term_size);
                if matches!(mouse.kind, MouseEventKind::ScrollDown) {
                    self.help_scroll = (self.help_scroll + 1).min(max);
                } else {
                    self.help_scroll = self.help_scroll.saturating_sub(1);
                }
            }
            return;
        }
        // While an input bar is open the keyboard owns the interaction;
        // stray clicks/scrolls shouldn't move state under the typed comment.
        if self.input.is_some() {
            return;
        }
        let (nav_rect, diff_rect) = body_rects(term_size, self.show_navigator, self.nav_width);
        let in_nav = rect_contains(nav_rect, mouse.column, mouse.row);
        let in_diff = rect_contains(diff_rect, mouse.column, mouse.row);

        match mouse.kind {
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let down = matches!(mouse.kind, MouseEventKind::ScrollDown);
                if in_nav {
                    let len = self.files().len();
                    let before = self.nav.selected;
                    if down {
                        self.nav.down(len);
                    } else {
                        self.nav.up();
                    }
                    if self.nav.selected != before {
                        self.file_changed();
                    }
                } else if in_diff {
                    self.wheel_diff(down, term_size);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if self.on_divider(mouse.column, mouse.row, term_size) {
                    self.resizing_navigator = true;
                    return;
                }
                if in_nav {
                    if let Some(idx) = self.nav_index_at(mouse.row, nav_rect) {
                        self.focus = Focus::Navigator;
                        if idx != self.nav.selected {
                            self.nav.selected = idx;
                            self.file_changed();
                        }
                    }
                } else if in_diff && self.view_row_count() > 0 {
                    self.focus = Focus::Diff;
                    // A fresh click always starts over: cursor moves, any
                    // keyboard/mouse selection is discarded, and the pressed
                    // row is remembered so a drag can grow a range from it.
                    self.visual_anchor = None;
                    let base = self.diff_base_at(mouse.row, diff_rect);
                    self.diff.cursor = base;
                    self.drag_origin = Some(base);
                    // Clicking a fold pill expands it in place.
                    self.expand_fold_at_cursor();
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                // A divider drag moves the boundary to the mouse column,
                // through the same clamps as `[`/`]` — the divider sits at
                // the diff's left border, i.e. at x == nav_width.
                if self.resizing_navigator {
                    self.nav_width =
                        mouse.column.clamp(NAV_MIN_WIDTH, nav_max_width(term_size.width));
                    self.ensure_cursor_visible(term_size);
                    return;
                }
                if let Some(origin) = self.drag_origin {
                    if self.view_row_count() == 0 {
                        return;
                    }
                    let base = self.diff_base_at(mouse.row, diff_rect);
                    if base != origin && self.visual_anchor.is_none() {
                        self.visual_anchor = Some(origin);
                    }
                    self.diff.cursor = base;
                    self.ensure_cursor_visible(term_size);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.drag_origin = None;
                self.resizing_navigator = false;
            }
            MouseEventKind::ScrollRight => {
                if in_diff {
                    self.diff.hscroll = self
                        .next_pan_stop(self.diff.hscroll, self.pan_step(term_size))
                        .min(self.pan_cap());
                }
            }
            MouseEventKind::ScrollLeft => {
                if in_diff {
                    self.diff.hscroll = self.diff.hscroll.saturating_sub(self.pan_step(term_size));
                }
            }
            _ => {}
        }
    }

    /// Wheel over the diff: move the viewport, and drag the cursor along
    /// only when it would leave the visible area (editor-style), so plain
    /// reading scrolls never disturb the cursor.
    fn wheel_diff(&mut self, down: bool, term_size: Size) {
        let base_count = self.view_row_count();
        if base_count == 0 {
            return;
        }
        let map = self.disp_map(diff_inner_width(term_size, self.show_navigator, self.nav_width));
        let max_scroll = map.total(base_count).saturating_sub(1);
        self.diff.scroll = if down {
            (self.diff.scroll + WHEEL_STEP).min(max_scroll)
        } else {
            self.diff.scroll.saturating_sub(WHEEL_STEP)
        };
        let viewport = diff_viewport_rows(term_size, self.show_navigator, self.nav_width);
        let cursor_disp = map.disp(self.diff.cursor);
        if cursor_disp < self.diff.scroll {
            self.diff.cursor = map.base_at(self.diff.scroll, base_count);
        } else if cursor_disp >= self.diff.scroll + viewport {
            self.diff.cursor =
                map.base_at(self.diff.scroll + viewport.saturating_sub(1), base_count);
        }
    }

    /// File index under a mouse row in the navigator, replicating the list's
    /// deterministic scroll offset (a fresh ListState per frame scrolls just
    /// enough to keep the selected row visible).
    fn nav_index_at(&self, mouse_row: u16, nav_rect: Rect) -> Option<usize> {
        let files = self.files();
        let inner_y = nav_rect.y + 1;
        let inner_h = nav_rect.height.saturating_sub(2);
        if inner_h == 0 || mouse_row < inner_y || mouse_row >= inner_y + inner_h {
            return None;
        }
        let inner_h = inner_h as usize;
        let offset = if self.nav.selected >= inner_h { self.nav.selected + 1 - inner_h } else { 0 };
        let idx = offset + (mouse_row - inner_y) as usize;
        (idx < files.len()).then_some(idx)
    }

    /// Base row under a mouse row in the diff pane; rows outside the inner
    /// area clamp to its edges so drags past the border keep selecting.
    fn diff_base_at(&self, mouse_row: u16, diff_rect: Rect) -> usize {
        let base_count = self.view_row_count();
        let map = self.disp_map((diff_rect.width.saturating_sub(2)).max(1) as usize);
        let inner_y = diff_rect.y + 1;
        let inner_h = diff_rect.height.saturating_sub(2).max(1);
        let row = mouse_row.clamp(inner_y, inner_y + inner_h - 1);
        let disp_row = self.diff.scroll + (row - inner_y) as usize;
        map.base_at(disp_row.min(map.total(base_count).saturating_sub(1)), base_count)
    }

    /// `c` in diff focus, input closed: open the comment prompt. Uses the
    /// visual selection if a `v` anchor is set (and clears it); otherwise
    /// the cursor row alone — unless that row is already covered by a
    /// pending annotation on this file, in which case it opens in edit mode
    /// (prefilled, replacing rather than duplicating on save).
    fn open_comment_input(&mut self) {
        let cursor = self.diff.cursor;
        // A fold pill isn't a code row: a comment "on" it would silently
        // anchor to the hidden line behind it. Expand first, then annotate.
        if is_fold_head(&self.active_folds(), cursor) && self.visual_anchor.is_none() {
            return;
        }
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

        if let Some(idx) = self.pending_at_row(cursor) {
            // Reopen at the annotation's anchor in THIS view, not wherever
            // it was first written.
            let (row_start, row_end) = self.pending_anchor(idx).unwrap_or((cursor, cursor));
            let p = &self.pending[idx];
            self.input = Some(InputMode::Comment {
                buf: p.annotation.comment.clone(),
                tag: p.annotation.tag.as_deref().and_then(Tag::from_label),
                editing: Some(idx),
                row_start,
                row_end,
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
        if let Some(idx) = self.pending_at_row(self.diff.cursor) {
            self.pending.remove(idx);
        }
    }

    /// `[` / `]`: move the navigator/diff boundary by `NAV_RESIZE_STEP`
    /// columns (shrink / widen the file list). No-op while the navigator is
    /// hidden and in the stacked layout, where the navigator trades height,
    /// not width. Starts from the RENDERED width, so the first press works
    /// from the auto (30%) default too.
    fn resize_navigator(&mut self, widen: bool, term_size: Size) {
        if !self.show_navigator || term_size.width < STACK_THRESHOLD {
            return;
        }
        let (nav_rect, _) = body_rects(term_size, true, self.nav_width);
        let current = nav_rect.width;
        let target = if widen {
            current.saturating_add(NAV_RESIZE_STEP)
        } else {
            current.saturating_sub(NAV_RESIZE_STEP)
        };
        // `handle_key` re-follows the cursor for every key right after
        // `handle_nav_key` returns, so this doesn't need to do it again.
        self.nav_width = target.clamp(NAV_MIN_WIDTH, nav_max_width(term_size.width));
    }

    /// Whether (`column`, `row`) sits on the navigator/diff divider — the
    /// navigator's right border or the diff's left border, within the body
    /// rows. Only meaningful in the side-by-side layout; the stacked layout
    /// has no vertical divider to grab.
    fn on_divider(&self, column: u16, row: u16, term_size: Size) -> bool {
        if !self.show_navigator || term_size.width < STACK_THRESHOLD {
            return false;
        }
        let (nav_rect, diff_rect) = body_rects(term_size, self.show_navigator, self.nav_width);
        if nav_rect.width == 0 || row < diff_rect.y || row >= diff_rect.y + diff_rect.height {
            return false;
        }
        column + 1 == diff_rect.x || column == diff_rect.x
    }

    fn handle_nav_key(&mut self, key: KeyEvent, term_size: Size) {
        match key.code {
            KeyCode::Char('[') => return self.resize_navigator(false, term_size),
            KeyCode::Char(']') => return self.resize_navigator(true, term_size),
            _ => {}
        }
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
                    self.file_changed();
                }
            }
            Focus::Diff => {
                let row_count = self.view_row_count();
                let cursor_before = self.diff.cursor;
                match key.code {
                    KeyCode::Char('j') | KeyCode::Down => self.diff.down(row_count),
                    KeyCode::Char('k') | KeyCode::Up => self.diff.up(),
                    KeyCode::Char('d') | KeyCode::PageDown => {
                        self.diff.page_down(half_page(term_size, self.show_navigator, self.nav_width), row_count)
                    }
                    KeyCode::Char('u') | KeyCode::PageUp => {
                        self.diff.page_up(half_page(term_size, self.show_navigator, self.nav_width))
                    }
                    // Hunk jumps only mean something in the diff view.
                    KeyCode::Char('n') if self.view == ViewMode::Diff => {
                        self.diff.next_hunk(&hunk_row_indices(&self.diff_rows()))
                    }
                    KeyCode::Char('p') if self.view == ViewMode::Diff => {
                        self.diff.prev_hunk(&hunk_row_indices(&self.diff_rows()))
                    }
                    KeyCode::Char('g') => self.diff.top(),
                    KeyCode::Char('G') => self.diff.bottom(row_count),
                    KeyCode::Right | KeyCode::Char('L') => {
                        self.diff.hscroll =
                            self.next_pan_stop(self.diff.hscroll, self.pan_step(term_size))
                                .min(self.pan_cap())
                    }
                    KeyCode::Left | KeyCode::Char('H') => {
                        self.diff.hscroll = self.diff.hscroll.saturating_sub(self.pan_step(term_size))
                    }
                    KeyCode::Char('0') => self.diff.hscroll = 0,
                    KeyCode::Char('h') | KeyCode::Tab => {
                        // Focusing an invisible pane strands the keyboard
                        // (j/k would switch files with no visible feedback):
                        // going "to the files" while collapsed reveals them.
                        self.show_navigator = true;
                        self.focus = Focus::Navigator;
                    }
                    KeyCode::Char('v') => {
                        self.visual_anchor = match self.visual_anchor {
                            Some(_) => None,
                            None => Some(self.diff.cursor),
                        };
                    }
                    KeyCode::Char('c') => self.open_comment_input(),
                    KeyCode::Char('x') => self.delete_pending_at_cursor(),
                    KeyCode::Char('t') => self.toggle_view(term_size),
                    KeyCode::Enter => self.expand_fold_at_cursor(),
                    KeyCode::Char('f') => self.fold_at_cursor_or_selection(),
                    KeyCode::Char('F') => self.unfold_all(),
                    _ => {}
                }
                // Any move that landed inside a fold's hidden tail continues
                // in its own direction: down past the fold, up onto the pill.
                if self.diff.cursor != cursor_before {
                    self.snap_cursor_out_of_folds(self.diff.cursor > cursor_before);
                }
            }
        }
    }

    /// `path:line` for the cursor row, shown in the footer so the reviewer
    /// always knows where they are. Prefers the new-file line number; removed
    /// lines fall back to the old side, marked as such.
    fn cursor_position(&self) -> Option<String> {
        let file = self.selected_file()?;
        if self.view == ViewMode::Source {
            // Source rows ARE lines, so the position is exact — unless the
            // file has no source view, where the single row is a placeholder.
            return Some(if self.source_line_count() > 0 {
                format!("{}:{}", file.path, self.diff.cursor + 1)
            } else {
                file.path.clone()
            });
        }
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

    /// The `?` overlay's content, grouped into sections. **Finish** always
    /// comes first (it applies everywhere); the section matching the
    /// CURRENT focus (`Files` for `Focus::Navigator`, `Diff` for
    /// `Focus::Diff`) is flagged `current` so `help_lines` can highlight its
    /// header — the same "where am I" instinct the footer's position
    /// already serves. `Annotate`, `Layout & views`, and `Mouse` are never
    /// flagged: they're not a focus/view of their own, just always-on
    /// actions reachable from wherever the reviewer is.
    fn help_sections(&self) -> Vec<HelpSection> {
        let files_current = self.focus == Focus::Navigator;
        let diff_current = self.focus == Focus::Diff;
        // n/p (hunk jumps) are a no-op outside diff view — see
        // `handle_nav_key` — so the overlay says so right on the row rather
        // than advertising a dead key while it's the active view.
        let hunk_note =
            if self.view == ViewMode::Source { " \u{2014} inactive in source view" } else { "" };

        vec![
            // The overlay is itself a modal state: while it is open, the
            // close keys close IT, not the review — the reference must
            // describe the state the reviewer is actually in.
            HelpSection {
                name: "This help",
                current: false,
                rows: vec![
                    HelpRow::new("? / esc / q / enter", "close this help"),
                    HelpRow::new("j / k", "scroll it"),
                ],
            },
            HelpSection {
                name: "Finish (once help is closed)",
                current: false,
                rows: vec![
                    HelpRow::new("a", "approve"),
                    HelpRow::new("r", "request changes"),
                    HelpRow::new("q", "cancel the review"),
                    HelpRow::new("esc", "cancel (clears an active selection first)"),
                    HelpRow::new("ctrl+c", "cancel, even mid-input"),
                ],
            },
            HelpSection {
                name: "Files",
                current: files_current,
                rows: vec![
                    HelpRow::new("j / k", "move down / up"),
                    HelpRow::new("g / G", "first / last file"),
                    HelpRow::new("l / enter / tab", "focus the diff"),
                ],
            },
            HelpSection {
                name: "Diff",
                current: diff_current,
                rows: {
                    let mut rows = vec![
                        HelpRow::new("j / k", "move cursor"),
                        HelpRow::new("g / G", "top / bottom"),
                        HelpRow::new("\u{2190} / \u{2192} (H/L)", "pan left / right"),
                        HelpRow::new("0", "reset pan"),
                        HelpRow::new("d / u", "half page down / up"),
                        HelpRow { key: "n / p", desc: format!("next / prev hunk{hunk_note}") },
                        HelpRow::new("h / tab", "focus the files"),
                        HelpRow::new("t", "toggle diff / source view"),
                    ];
                    // Folding lives in source view only: the diff already
                    // shows just its hunks.
                    if self.view == ViewMode::Source {
                        rows.push(HelpRow::new("f", "fold the selection / the block under the cursor"));
                        rows.push(HelpRow::new("F", "unfold everything in this file"));
                    }
                    if !self.active_folds().is_empty() {
                        rows.push(HelpRow::new("enter", "expand the fold under the cursor"));
                    }
                    rows
                },
            },
            HelpSection {
                name: "Annotate",
                current: false,
                rows: vec![
                    HelpRow::new("v", "start / clear a selection"),
                    HelpRow::new("c", "comment on selection or cursor line"),
                    HelpRow::new("  ctrl+t", "cycle the tag"),
                    HelpRow::new("  enter", "save the comment"),
                    HelpRow::new("  esc", "cancel the comment"),
                    HelpRow::new("c (annotated line)", "edit the comment"),
                    HelpRow::new("x", "delete annotation at cursor"),
                ],
            },
            HelpSection {
                name: "Layout & views",
                current: false,
                rows: vec![
                    HelpRow::new("b", "show / hide the files pane"),
                    HelpRow::new("[ / ]", "shrink / widen the files pane"),
                    HelpRow::new("z", "zoom the pane"),
                    HelpRow::new("?", "toggle this help"),
                ],
            },
            HelpSection {
                name: "Mouse",
                current: false,
                rows: vec![
                    HelpRow::new("wheel", "scroll the files or the diff"),
                    HelpRow::new("horiz. wheel", "pan the diff"),
                    HelpRow::new("click", "select a file / move the cursor"),
                    HelpRow::new("drag", "select a range in the diff"),
                    HelpRow::new("drag the divider", "resize the file list"),
                ],
            },
        ]
    }

    /// `help_sections` flattened into display lines, each row clipped to
    /// `width` display columns (see `head_fit`) so nothing overruns the
    /// overlay regardless of pane size.
    fn help_lines(&self, width: usize) -> Vec<Line<'static>> {
        const KEY_COL: usize = 16;
        let mut lines = Vec::new();
        for (i, section) in self.help_sections().into_iter().enumerate() {
            if i > 0 {
                lines.push(Line::raw(""));
            }
            let marker = if section.current { "\u{25b8} " } else { "  " };
            let suffix = if section.current { " (current)" } else { "" };
            let header = format!("{marker}{}{suffix}", section.name);
            let style = if section.current {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            };
            lines.push(Line::styled(head_fit(&header, width), style));
            for row in section.rows {
                let text = format!(" {:<KEY_COL$} {}", row.key, row.desc);
                lines.push(Line::raw(head_fit(&text, width)));
            }
        }
        lines
    }

    /// The largest `help_scroll` that still leaves the overlay's content
    /// filling its viewport — shared by `handle_key`'s j/k and
    /// `handle_mouse`'s wheel so scrolling can't run past the end.
    fn help_max_scroll(&self, term_size: Size) -> usize {
        let (width, _) = help_overlay_geometry(term_size, 0);
        let inner_width = width.saturating_sub(2).max(1) as usize;
        let content_len = self.help_lines(inner_width).len();
        let (_, height) = help_overlay_geometry(term_size, content_len);
        let viewport = height.saturating_sub(2) as usize;
        content_len.saturating_sub(viewport)
    }
}

struct HelpRow {
    key: &'static str,
    desc: String,
}

impl HelpRow {
    fn new(key: &'static str, desc: &str) -> Self {
        HelpRow { key, desc: desc.to_string() }
    }
}

/// One section of the `?` overlay's content — see `App::help_sections`.
struct HelpSection {
    name: &'static str,
    /// Whether this section matches the reviewer's current focus/view, so
    /// `help_lines` can highlight its header.
    current: bool,
    rows: Vec<HelpRow>,
}

/// The overlay's (width, height) for a `term_size`-sized frame and a given
/// content length (line count from `App::help_lines`). Width never depends
/// on content, only on the terminal — capped at 72 columns, and shrinking
/// with a 4-column margin (2 either side) on narrower ones. Height fits the
/// content up to the same margin against the terminal's height, so a short
/// key list doesn't reserve a full-screen box, and a tall one still leaves
/// a visible frame around it rather than filling the terminal edge-to-edge.
fn help_overlay_geometry(term_size: Size, content_len: usize) -> (u16, u16) {
    const MARGIN: u16 = 4;
    const MAX_WIDTH: u16 = 72;
    let width = term_size.width.saturating_sub(MARGIN).min(MAX_WIDTH).max(1);
    let max_height = term_size.height.saturating_sub(MARGIN).max(3);
    let height = (content_len as u16).saturating_add(2).min(max_height).max(3);
    (width, height)
}

/// Clip a plain (unstyled) overlay row to `width` display columns,
/// truncating the END with an ellipsis. The inverse of `tail_fit`: overlay
/// rows are static reference text, not a live-typed buffer, so what matters
/// is keeping the START (the key column) rather than the caret end.
/// Grapheme-cluster aware, like `pan_and_clip`, so a clip never splits a
/// multi-codepoint glyph — though overlay text is plain ASCII plus a
/// handful of single-width symbols (arrows, the bullet, the middle dot), so
/// in practice this never has to make that call.
fn head_fit(s: &str, width: usize) -> String {
    if str_cols(s) <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let avail = width - 1; // room for the trailing ellipsis
    let mut out = String::new();
    let mut cols = 0usize;
    for g in s.graphemes(true) {
        let w = UnicodeWidthStr::width(g);
        if cols + w > avail {
            break;
        }
        out.push_str(g);
        cols += w;
    }
    out.push('\u{2026}');
    out
}

/// Render the `?` overlay: a `Clear`ed, centered, bordered box over
/// whatever's already drawn. Content is pre-clipped per row by
/// `App::help_lines`, so the `Paragraph` here just scrolls — it doesn't
/// wrap (wrapping would defeat the per-row clipping and let a long row spill
/// onto an extra line, throwing off the scroll math).
fn draw_help_overlay(frame: &mut Frame, area: Rect, app: &App) {
    let term_size = Size { width: area.width, height: area.height };
    let (width, _) = help_overlay_geometry(term_size, 0);
    let inner_width = width.saturating_sub(2).max(1) as usize;
    let content = app.help_lines(inner_width);
    let (_, height) = help_overlay_geometry(term_size, content.len());
    let rect = Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    );

    let viewport = height.saturating_sub(2) as usize;
    let max_scroll = content.len().saturating_sub(viewport);
    let scroll = app.help_scroll.min(max_scroll) as u16;

    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Thick)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" keys ");
    let paragraph = Paragraph::new(content).block(block).scroll((scroll, 0));
    frame.render_widget(paragraph, rect);
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
    if app.help_open {
        draw_help_overlay(frame, area, app);
    }
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

/// Footer text for the request-changes summary bar: the label, then either
/// the full buffer plus the `⏎ send · esc cancel` hint (when both fit `width`)
/// or just a tail-fit buffer with the hint dropped (when they don't) — the
/// same end-of-buffer-visible rule the comment bar below also follows.
fn summary_footer_text(buf: &str, width: usize) -> String {
    let label = " request changes \u{2014} summary: ";
    let suffix = " \u{23ce} send \u{b7} esc cancel";
    // Display columns, not chars: a CJK buffer can "fit" by char count while
    // its real rendered width (2 columns/char) already overflows the footer
    // once the hint suffix is appended.
    let avail = width.saturating_sub(str_cols(label));
    if str_cols(buf) + str_cols(suffix) <= avail {
        format!("{label}{buf}{suffix}")
    } else {
        format!("{label}{}", tail_fit(buf, avail))
    }
}

/// The non-input footer, replacing the old per-focus hint sausage (a wall of
/// key hints that ran off the right edge of narrow panes) with a short,
/// constant tail — the full key reference now lives in the `?` overlay
/// instead of being crammed into one line. `context` is the position
/// (`path:line`, diff focus) or `files (n)` (navigator focus); it comes
/// FIRST so it's what survives if the pane is too narrow for the rest, the
/// same "position outlives the hints" convention the old footer used.
fn slim_footer_text(context: &str, width: usize) -> String {
    let hints = " \u{b7} a approve \u{b7} r request changes \u{b7} q cancel \u{b7} ? help";
    let full = format!(" {context}{hints}");
    if str_cols(&full) <= width {
        return full;
    }
    let ctx_only = format!(" {context}");
    if str_cols(&ctx_only) <= width {
        return ctx_only;
    }
    tail_fit(&ctx_only, width)
}

fn draw_footer(frame: &mut Frame, area: Rect, app: &App) {
    let text = match &app.input {
        // Input bars keep the END of a long buffer visible (that's where the
        // caret is) by trimming from the left with an ellipsis.
        Some(InputMode::Summary { buf }) => summary_footer_text(buf, area.width as usize),
        Some(InputMode::Comment { buf, tag, .. }) => {
            let tag_label = tag.map(|t| t.label()).unwrap_or("none");
            let label = format!(" comment [tag: {tag_label}]: ");
            format!("{label}{}", tail_fit(buf, (area.width as usize).saturating_sub(label.chars().count())))
        }
        None => {
            let context = match app.focus {
                Focus::Navigator => format!("files ({})", app.files().len()),
                Focus::Diff => app.cursor_position().unwrap_or_default(),
            };
            slim_footer_text(&context, area.width as usize)
        }
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

/// Narrowest useful navigator: a short filename plus the +N -N counts.
const NAV_MIN_WIDTH: u16 = 14;
/// Columns one `[`/`]` press moves the navigator/diff boundary.
const NAV_RESIZE_STEP: u16 = 4;

/// Columns the diff pane always keeps, even when the navigator is maxed
/// out: its two borders plus a readable stretch of code.
const DIFF_READABLE_FLOOR: u16 = 40;

/// The widest the navigator may grow in `area`: always leave the code pane
/// its borders plus a readable stretch of content.
fn nav_max_width(area_width: u16) -> u16 {
    area_width.saturating_sub(DIFF_READABLE_FLOOR).max(NAV_MIN_WIDTH)
}

/// `nav_width` is the reviewer's `[`/`]` preference in columns; `0` means
/// "auto" (the 30% default). Preferences are clamped here, at layout time,
/// so a resize of the terminal re-clamps automatically. Width preferences
/// only apply to the side-by-side layout — the stacked layout (narrow
/// panes) keeps its height formula.
fn body_split(area: Rect, show_navigator: bool, nav_width: u16) -> (Rect, Rect) {
    if !show_navigator {
        // Navigator collapsed (`b`): the diff gets the whole body.
        return (Rect::new(area.x, area.y, 0, 0), area);
    }
    if area.width < STACK_THRESHOLD {
        let nav_height = ((area.height as u32 * 30 / 100) as u16).clamp(4, 10).min(area.height);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(nav_height), Constraint::Min(0)])
            .split(area);
        return (rows[0], rows[1]);
    }
    let auto = ((area.width as u32 * 30 / 100) as u16).max(24);
    let chosen = if nav_width == 0 { auto } else { nav_width };
    let nav_cols = chosen.clamp(NAV_MIN_WIDTH, nav_max_width(area.width)).min(area.width);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(nav_cols), Constraint::Min(0)])
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
    let (nav_area, diff_area) = body_split(area, app.show_navigator, app.nav_width);
    if app.show_navigator {
        draw_navigator(frame, nav_area, app, files);
    }
    draw_diff(frame, diff_area, app, files.get(app.nav.selected));
}

fn draw_panes_with_error(frame: &mut Frame, area: Rect, app: &App, err: &anyhow::Error) {
    let (nav_area, diff_area) = body_split(area, app.show_navigator, app.nav_width);
    if app.show_navigator {
        draw_navigator(frame, nav_area, app, &[]);
    }

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
    let mut title = file.map(file_display_path).unwrap_or_else(|| "diff".to_string());
    if app.view == ViewMode::Source {
        title.push_str(" [source]");
    }
    let block = pane_block(title, app.focus == Focus::Diff);

    if file.is_none() {
        frame.render_widget(block, area);
        return;
    }

    // Pre-highlighted (diff rows in `App::new`, source rows on first `t`);
    // drawing just clones the cached, owned lines rather than re-running
    // syntect on every frame.
    let mut lines: Vec<Line> = app.view_lines();

    // Where each annotation sits in THIS view; annotations with no anchor
    // here (an old-side one in source view, or lines outside every hunk in
    // diff view) draw nothing at all.
    let anchors = app.view_anchors();

    // Gutter markers: one per row covered by a pending annotation on this
    // file, colored by tag. Overwrites the first character of the gutter
    // span in place so the column width doesn't shift.
    for pending in app.pending.iter().filter(|p| p.file_idx == app.nav.selected) {
        let Some((row_start, row_end)) = anchors.anchor(&pending.annotation) else { continue };
        let color = tag_color(pending.annotation.tag.as_deref());
        for row in row_start..=row_end {
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
    // background wins where the two overlap. While the comment box is open
    // the anchor has already been consumed into the input state, so the
    // range being commented on is painted from there instead — otherwise the
    // selection appears to collapse to the cursor row mid-typing.
    let selection = match (&app.input, app.visual_anchor) {
        (Some(InputMode::Comment { row_start, row_end, .. }), _) => Some((*row_start, *row_end)),
        (_, Some(anchor)) => {
            Some((anchor.min(app.diff.cursor), anchor.max(app.diff.cursor)))
        }
        _ => None,
    };
    if let Some((start, end)) = selection {
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

    let inner_width = (area.width.saturating_sub(2)).max(1) as usize;

    // Horizontal pan + clip indicators for code rows. Gutter and origin
    // marker (the first two spans of a line row) stay pinned; hunk headers
    // and placeholders (single-span rows) pan whole. Runs after the style
    // patches (they only touch styles/first gutter char) and before comment
    // weaving (comments wrap instead of panning).
    for line in &mut lines {
        let pinned = if line.spans.len() >= 3 { 2 } else { 0 };
        pan_and_clip(line, app.diff.hscroll, inner_width, pinned);
    }

    // Fold collapse (agent-focused source view): each run's head row becomes
    // its pill and the tail rows disappear. Descending order keeps earlier
    // indices valid. Must run AFTER the base-row patches above (they index
    // base rows) and BEFORE comment weaving (whose splice indices below are
    // fold-compressed). The cursor background is re-applied to the pill
    // because replacing the head row discarded the patched line.
    let folds = app.active_folds();
    for &(s, e) in folds.iter().rev() {
        if s >= lines.len() {
            continue;
        }
        let notes = app
            .pending
            .iter()
            .filter(|p| p.file_idx == app.nav.selected)
            .filter(|p| {
                matches!(anchors.anchor(&p.annotation), Some((rs, re)) if rs <= e && s <= re)
            })
            .count();
        let mut pill = fold_pill_line(e - s + 1, notes);
        if app.diff.cursor == s {
            for span in &mut pill.spans {
                span.style = span.style.bg(CURSOR_BG);
            }
        }
        lines.drain(s + 1..(e + 1).min(lines.len()));
        lines[s] = pill;
    }
    // Fold-compressed display index of a VISIBLE base row: what `row_end`
    // means as an index into `lines` after the drain above (and before the
    // comment insertion below).
    let folded_row = |base: usize| -> usize {
        base - folds.iter().map(|&(s, e)| if e < base { e - s } else { 0 }).sum::<usize>()
    };

    // Inline comment rows, woven in directly under the lines they annotate
    // (GitHub-style) so feedback sits next to the code instead of living only
    // in the bottom bar. Long comments wrap to the pane width as continuation
    // rows. Must run AFTER all base-row patches above — the patches index
    // base rows, and insertion shifts everything below it. Groups are
    // inserted in descending anchor order to keep earlier indices valid.
    // Anchors folded away aren't woven — their fold's note badge stands in.
    //
    // While the comment prompt is open (new or edit), the dashed editing box
    // is woven at its anchor instead of a plain preview; when editing an
    // existing annotation, that annotation's saved rows are skipped (the box
    // replaces them at the same anchor) so they don't render stale text.
    let editing_idx = app.editing_annotation_idx();
    let mut groups: std::collections::BTreeMap<usize, Vec<Line>> = std::collections::BTreeMap::new();
    for (i, p) in
        app.pending.iter().enumerate().filter(|(_, p)| p.file_idx == app.nav.selected)
    {
        if Some(i) == editing_idx {
            continue;
        }
        let Some((_, row_end)) = anchors.anchor(&p.annotation) else { continue };
        if fold_run_containing(&folds, row_end).is_some() {
            continue;
        }
        groups.entry(folded_row(row_end)).or_default().extend(inline_comment_lines(
            p.annotation.tag.as_deref(),
            &p.annotation.comment,
            inner_width,
        ));
    }
    if let Some(InputMode::Comment { buf, tag, row_end, .. }) = &app.input {
        groups.entry(folded_row(*row_end)).or_default().extend(editing_box_lines(
            buf,
            *tag,
            inner_width,
        ));
    }
    for (end, group) in groups.into_iter().rev() {
        let at = (end + 1).min(lines.len());
        lines.splice(at..at, group);
    }

    let paragraph = Paragraph::new(lines).block(block).scroll((scroll_row_u16(app.diff.scroll), 0));
    frame.render_widget(paragraph, area);
}

/// Narrow a display-space scroll offset to the `u16` `Paragraph::scroll`
/// takes, WITHOUT wrapping. `MAX_SOURCE_LINES`'s margin for woven comment
/// rows is a soft guideline sized for a realistic annotation count, not an
/// enforced bound — a saved comment or the open editing box can add extra
/// display rows without limit, so `scroll` itself could still exceed
/// `u16::MAX` given enough of them. Clamping here means that pathological
/// case stops scrolling further, rather than the offset wrapping and the
/// pane silently rendering unrelated earlier rows while the cursor/footer
/// still report the true, later position.
fn scroll_row_u16(scroll: usize) -> u16 {
    scroll.min(u16::MAX as usize) as u16
}

fn str_cols(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Pan a rendered row `hscroll` display columns to the left — keeping its
/// first `pinned` spans (gutter + origin marker) in place — then clip it to
/// `width` display columns. Clipped edges get dim `‹` / `…` indicators so
/// the reviewer can tell content continues off-screen.
///
/// Two invariants, deliberately separate:
/// - WIDTHS are unicode-width 0.2 string widths per grapheme — the exact
///   crate+version pair ratatui 0.29 pins for its own rendering, so a ZWJ
///   family emoji counts whatever ratatui will actually draw it as.
///   Diverging from the render layer's math would misalign every row.
/// - ATOMICITY is by extended grapheme cluster (UAX #29 via
///   unicode-segmentation): a boundary never splits a cluster — no half
///   families, orphaned flag halves, or bare Indic conjunct pieces. A
///   cluster straddling the pan boundary drops whole with pad spaces so
///   columns stay aligned; one straddling the clip budget drops whole under
///   the … marker. Earlier hand-rolled ZWJ/flag/modifier heuristics kept
///   missing scripts (viramas were next); UAX #29 is the single primitive
///   that ends that series. Segmentation runs per span — syntect splits on
///   token boundaries, which do not land inside grapheme clusters.
fn pan_and_clip(line: &mut Line<'static>, hscroll: usize, width: usize, pinned: usize) {
    let pinned = pinned.min(line.spans.len());

    if hscroll > 0 {
        let mut col = 0usize; // columns consumed from the unpinned content
        let mut dropped = false;
        let mut pad_cols = 0usize; // columns dropped past the boundary, refilled as spaces
        for span in line.spans.iter_mut().skip(pinned) {
            if col >= hscroll && pad_cols == 0 {
                break;
            }
            let mut kept = String::new();
            for g in span.content.graphemes(true) {
                let w = str_cols(g);
                if col < hscroll {
                    // Still panning: the cluster drops WHOLE; if it straddles
                    // the boundary the overshoot comes back as pad spaces.
                    col += w;
                    dropped = true;
                    if col > hscroll {
                        pad_cols += col - hscroll;
                    }
                } else {
                    if pad_cols > 0 {
                        kept.extend(std::iter::repeat(' ').take(pad_cols));
                        pad_cols = 0;
                    }
                    kept.push_str(g);
                }
            }
            span.content = kept.into();
        }
        if dropped {
            // Mark the left clip on the first visible cluster, preserving
            // its full display width ("‹" plus pad spaces for wide clusters)
            // so columns stay aligned with unpanned rows.
            for span in line.spans.iter_mut().skip(pinned) {
                if !span.content.is_empty() {
                    let mut graphemes = span.content.graphemes(true);
                    let first = graphemes.next().unwrap_or_default();
                    let cluster_width = str_cols(first).max(1);
                    let rest: String = graphemes.collect();
                    span.content =
                        format!("\u{2039}{}{rest}", " ".repeat(cluster_width - 1)).into();
                    break;
                }
            }
        }
    }

    let total: usize = line.spans.iter().map(|s| str_cols(&s.content)).sum();
    if total > width {
        let keep = width.saturating_sub(1);
        let mut used = 0usize;
        for span in line.spans.iter_mut() {
            let cols = str_cols(&span.content);
            if used + cols <= keep {
                used += cols;
                continue;
            }
            let mut kept = String::new();
            for g in span.content.graphemes(true) {
                let w = str_cols(g);
                if used + w > keep {
                    // The straddling cluster drops whole: rendering a
                    // truncated prefix would show a DIFFERENT glyph (a
                    // family cut to a couple, half a flag, a bare conjunct).
                    break;
                }
                kept.push_str(g);
                used += w;
            }
            span.content = kept.into();
            // Force every following span to truncate to empty (a dropped
            // straddler leaves at most a small gap before the marker).
            used = keep;
        }
        line.spans.push(Span::styled("\u{2026}", Style::default().fg(Color::DarkGray)));
    }
}

/// Comment-row background: a warm dark slate distinct from both the code
/// ground and the cursor row.
const COMMENT_BG: Color = Color::Rgb(38, 34, 28);

/// Editing-box chrome color: rules, corners, and the vertical `┆` borders.
const EDIT_RULE_FG: Color = Color::DarkGray;

/// Wrap width for the editing box's content rows: the inner pane width minus
/// the `┆ ` prefix, one spare column reserved for the typing caret, and the
/// closing `┆`.
fn editing_wrap_width(inner_width: usize) -> usize {
    inner_width.saturating_sub(4).max(1)
}

/// Display rows the editing box occupies at a given pane width — MUST agree
/// with `editing_box_lines` (content rows, wrapped at `editing_wrap_width`,
/// plus the top and bottom rule rows) — feeds the display map so scroll/mouse
/// math sees the box's real height.
fn editing_box_height(buf: &str, inner_width: usize) -> usize {
    wrap_comment(buf, editing_wrap_width(inner_width)).len() + 2
}

/// Build the annot-style dashed editing box: a top rule, one content row per
/// wrapped line of `buf` (with a typing caret on the last row — an empty
/// buffer still renders one row, just the caret), and a bottom rule carrying
/// the commit/tag/cancel button chips.
fn editing_box_lines(buf: &str, tag: Option<Tag>, inner_width: usize) -> Vec<Line<'static>> {
    // NOT `.max(4)`: the box must fit the pane it's actually drawn into, not
    // a hypothetical wider one — forcing a wider width here just moves the
    // overflow from "rows too wide" to "box wider than the pane".
    let width = inner_width.max(1);
    let rule_style = Style::default().fg(EDIT_RULE_FG).bg(COMMENT_BG);
    let text_style = Style::default().fg(Color::White).bg(COMMENT_BG);

    let mut lines = Vec::with_capacity(editing_box_height(buf, width));

    // Top rule: ╭ + ╌…╌ + ╮, spanning the full inner width.
    let dash_len = width.saturating_sub(2);
    lines.push(Line::from(Span::styled(
        format!("\u{256d}{}\u{256e}", "\u{254c}".repeat(dash_len)),
        rule_style,
    )));

    // Content rows: ┆ <text, padded> ┆ — the caret occupies the one spare
    // column `editing_wrap_width` reserves beyond the wrapped text.
    let wrap_width = editing_wrap_width(width);
    let middle_width = wrap_width + 1;
    let chunks = wrap_comment(buf, wrap_width);
    let last = chunks.len() - 1;
    for (i, chunk) in chunks.into_iter().enumerate() {
        let mut middle = chunk;
        // Display columns, not chars: a chunk of wide (e.g. CJK) glyphes has
        // fewer chars than the columns it renders as, so char-counting here
        // under-pads and pushes the closing border past the box's actual
        // width.
        let mut used = str_cols(&middle);
        if i == last {
            middle.push('\u{258f}'); // typing caret
            used += str_cols("\u{258f}");
        }
        if used < middle_width {
            middle.push_str(&" ".repeat(middle_width - used));
        }
        lines.push(Line::from(vec![
            Span::styled("\u{2506} ", rule_style),
            Span::styled(middle, text_style),
            Span::styled("\u{2506}", rule_style),
        ]));
    }

    lines.push(bottom_rule_line(tag, width));

    // Belt-and-braces never-overflow: `editing_wrap_width`'s own `.max(1)`
    // floor (content needs at least one column to make progress) means a
    // pane narrower than the box's minimum chrome (prefix + one content
    // column + caret + closing border, 5 columns) still renders a row wider
    // than `width` even after the change above. Clip every row with the
    // same column-accurate, cluster-atomic primitive diff rows already use,
    // rather than duplicating that logic here.
    for line in &mut lines {
        pan_and_clip(line, 0, width, 0);
    }
    lines
}

/// Bottom rule of the editing box: `╰╌` then the commit/tag/cancel button
/// chips embedded in the dashed rule, then dash fill and the closing `╯`.
/// When the chips don't all fit, they're dropped right-to-priority (tag
/// first, then cancel; commit is never dropped) so the rule never overflows
/// `width`.
fn bottom_rule_line(tag: Option<Tag>, width: usize) -> Line<'static> {
    let width = width.max(1);
    let tag_label = tag.map(|t| t.label()).unwrap_or("none");
    let commit_chip = "[ commit \u{23ce} ]".to_string();
    let tag_chip = format!("[ tag ^T: {tag_label} ]");
    let cancel_chip = "[ cancel esc ]".to_string();

    let rule_style = Style::default().fg(EDIT_RULE_FG).bg(COMMENT_BG);
    let chip_style =
        Style::default().fg(Color::White).bg(COMMENT_BG).add_modifier(Modifier::REVERSED);

    const PREFIX: &str = "\u{2570}\u{254c}"; // ╰╌
    const SEP: char = '\u{254c}';
    const CORNER: char = '\u{256f}';

    let attempts: [Vec<String>; 4] = [
        vec![commit_chip.clone(), tag_chip.clone(), cancel_chip.clone()],
        vec![commit_chip.clone(), cancel_chip.clone()],
        vec![commit_chip.clone()],
        Vec::new(),
    ];

    for chips in attempts {
        let seps = chips.len().saturating_sub(1);
        let chips_len: usize = chips.iter().map(|c| c.chars().count()).sum();
        let used = PREFIX.chars().count() + chips_len + seps;
        if used + 1 <= width {
            let mut spans = vec![Span::styled(PREFIX, rule_style)];
            for (i, chip) in chips.into_iter().enumerate() {
                if i > 0 {
                    spans.push(Span::styled(SEP.to_string(), rule_style));
                }
                spans.push(Span::styled(chip, chip_style));
            }
            let fill = width - used - 1;
            spans.push(Span::styled(SEP.to_string().repeat(fill), rule_style));
            spans.push(Span::styled(CORNER.to_string(), rule_style));
            return Line::from(spans);
        }
    }

    // Even a bare `╰╌…╯` doesn't fit (pathologically narrow pane): fall back
    // to plain dash fill, exactly `width` columns, so the invariant (never
    // overflow) always holds.
    Line::from(Span::styled(SEP.to_string().repeat(width), rule_style))
}

/// Greedy word wrap on display columns (`str_cols`, ratatui's own model) —
/// the same width unit `pan_and_clip` uses, so a row of CJK text wraps at
/// the columns it actually renders as rather than at half that many chars.
/// A word longer than the width is hard-broken cluster-atomic (UAX #29 via
/// `graphemes(true)`): a boundary never splits a cluster, so a straddling
/// wide glyph moves to the next row whole rather than rendering half of it.
/// Always yields at least one (possibly empty) chunk so an empty live
/// preview still renders its row.
fn wrap_comment(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_cols = 0usize;
    for word in text.split(' ') {
        let mut word: Vec<&str> = word.graphemes(true).collect();
        loop {
            let sep = if current_cols == 0 { 0 } else { 1 };
            let word_cols: usize = word.iter().map(|g| str_cols(g)).sum();
            if current_cols + sep + word_cols <= width {
                if sep == 1 {
                    current.push(' ');
                }
                current.extend(word.iter().copied());
                current_cols += sep + word_cols;
                break;
            }
            if word_cols > width {
                // Hard-break an overlong word at whatever space remains,
                // taking whole clusters until the next one wouldn't fit (but
                // always at least one, so a single cluster wider than the
                // whole box still makes forward progress).
                if current_cols == 0 {
                    let avail = width.saturating_sub(sep).max(1);
                    let mut take_cols = 0usize;
                    let mut take = 0usize;
                    for g in &word {
                        let g_cols = str_cols(g).max(1);
                        if take > 0 && take_cols + g_cols > avail {
                            break;
                        }
                        take_cols += g_cols;
                        take += 1;
                        if take_cols >= avail {
                            break;
                        }
                    }
                    let taken: String = word.drain(..take).collect();
                    current.push_str(&taken);
                    chunks.push(std::mem::take(&mut current));
                    current_cols = 0;
                    continue;
                }
            }
            chunks.push(std::mem::take(&mut current));
            current_cols = 0;
        }
    }
    chunks.push(current);
    chunks
}

/// The text width available to comment content once the `┃ [tag] ` marker
/// (and the matching indent on continuation rows) is accounted for.
fn comment_text_width(tag: Option<&str>, inner_width: usize) -> usize {
    let marker_len = comment_marker(tag).chars().count();
    inner_width.saturating_sub(marker_len).max(8)
}

/// First-row lead for a saved note: a tag-colored bar, then the bracketed
/// tag. Untagged notes show `[note]`.
fn comment_marker(tag: Option<&str>) -> String {
    format!("\u{2503} [{}] ", tag.unwrap_or("note"))
}

/// Continuation-row lead: the same bar, then enough spaces to line up under
/// the first row's text column (same total width as `comment_marker`).
fn comment_marker_continuation(tag: Option<&str>) -> String {
    let marker_len = comment_marker(tag).chars().count();
    format!("\u{2503} {}", " ".repeat(marker_len.saturating_sub(2)))
}

/// Display rows one comment occupies at a given pane width — MUST agree with
/// `inline_comment_lines`, and feeds the display map so scrolling accounts
/// for wrapped comments.
fn comment_height(tag: Option<&str>, text: &str, inner_width: usize) -> usize {
    wrap_comment(text, comment_text_width(tag, inner_width)).len()
}

/// Build one saved comment as display rows: `┃ [tag] text…` with wrapped
/// continuation rows indented under the text column. The editing box (see
/// `editing_box_lines`) handles the in-progress/typing case; this renders
/// only settled, saved annotations.
fn inline_comment_lines(tag: Option<&str>, text: &str, inner_width: usize) -> Vec<Line<'static>> {
    let marker = comment_marker(tag);
    let indent = comment_marker_continuation(tag);
    let base = Style::default().bg(COMMENT_BG);
    let chunks = wrap_comment(text, comment_text_width(tag, inner_width));
    chunks
        .into_iter()
        .enumerate()
        .map(|(i, chunk)| {
            let lead = if i == 0 { marker.clone() } else { indent.clone() };
            let spans = vec![
                Span::styled(lead, base.fg(tag_color(tag)).add_modifier(Modifier::BOLD)),
                Span::styled(chunk, base.fg(Color::White).add_modifier(Modifier::ITALIC)),
            ];
            Line::from(spans)
        })
        .collect()
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

/// Width of the source view's line-number field. The diff gutter renders
/// two 4-wide numbers, a separator and a trailing space, then a 1-column
/// origin marker (11 columns before the code); the source view spends the
/// same 11 on `{n:>8}` + two spaces + the one-space spacer span, so code
/// starts in the same column whichever view you're in.
const SOURCE_NUMBER_WIDTH: usize = 8;

/// Highlight a whole source file into display-ready rows, one per line.
///
/// Unlike the diff (highlighted hunk by hunk, each hunk mixing two file
/// states), the source view feeds one `HighlightLines` the entire file top
/// to bottom — the highlighter sees exactly the text the parser expects, so
/// multi-line strings, block comments and nested blocks all come out right.
///
/// Row layout is `[gutter, one-space spacer, content…]`, deliberately at
/// least three spans: `draw_diff` pins the first two spans of any row with
/// three or more when panning horizontally, which keeps the line numbers
/// on screen exactly as the diff view's gutter+marker do. Blank lines get
/// an empty content span so they hit that rule too.
fn highlight_source_rows(
    hl: &'static Highlighter,
    path: &str,
    lines: &[String],
) -> Vec<Line<'static>> {
    let syntax = syntax_for_path(hl, path);
    let mut state = HighlightLines::new(syntax, &hl.theme);
    let mut out = Vec::with_capacity(lines.len());

    for (i, text) in lines.iter().enumerate() {
        let mut spans = vec![
            Span::styled(
                format!("{:>width$}  ", i + 1, width = SOURCE_NUMBER_WIDTH),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw(" "),
        ];

        // Same newline dance as the diff highlighter: syntect needs the
        // trailing newline for state tracking, and it must not be rendered.
        let mut fed = text.clone();
        fed.push('\n');
        let ranges = state.highlight_line(&fed, &hl.syntax_set).unwrap_or_default();
        for (style, chunk) in ranges {
            let chunk = chunk.strip_suffix('\n').unwrap_or(chunk);
            if chunk.is_empty() {
                continue;
            }
            let fg = Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b);
            spans.push(Span::styled(chunk.to_string(), Style::default().fg(fg)));
        }
        if spans.len() == 2 {
            // Blank line: still needs a content span to keep the gutter pinned.
            spans.push(Span::raw(""));
        }
        out.push(Line::from(spans));
    }
    out
}

/// The single row shown instead of source for files that have none.
fn source_placeholder_line(reason: &str) -> Line<'static> {
    Line::styled(format!("(no source view: {reason})"), Style::default().fg(Color::DarkGray))
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

    // Always emit a content span, even for blank lines: pan/clip pins the
    // gutter+marker only on rows with 3+ spans, so a 2-span blank changed
    // line would have its line numbers consumed by horizontal panning.
    if spans.len() == 2 {
        let mut span_style = Style::default();
        if let Some(bg) = bg {
            span_style = span_style.bg(bg);
        }
        spans.push(Span::styled(String::new(), span_style));
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

    /// Concatenate a rendered `Line`'s spans back into plain text, for
    /// asserting on content without caring about styling.
    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
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
        let mut state = DiffViewState { scroll: 0, cursor: 0, hscroll: 0 };

        // From the first header, next jumps the CURSOR to the second header;
        // a further next is a no-op.
        state.next_hunk(&hunks);
        assert_eq!(state.cursor, 4);
        state.next_hunk(&hunks);
        assert_eq!(state.cursor, 4);

        // From a row between headers, prev jumps back to the nearest header before it.
        state.cursor = 6;
        state.prev_hunk(&hunks);
        assert_eq!(state.cursor, 4);
        state.prev_hunk(&hunks);
        assert_eq!(state.cursor, 0);
        // No header before row 0: no-op.
        state.prev_hunk(&hunks);
        assert_eq!(state.cursor, 0);
    }

    #[test]
    fn diff_view_cursor_clamps_at_both_ends() {
        let mut state = DiffViewState { scroll: 0, cursor: 0, hscroll: 0 };
        state.up(); // already at 0, saturating
        assert_eq!(state.cursor, 0);

        state.down(3); // row_count 3 -> max cursor index 2
        state.down(3);
        state.down(3);
        assert_eq!(state.cursor, 2);

        state.page_up(10);
        assert_eq!(state.cursor, 0);

        state.page_down(10, 3);
        assert_eq!(state.cursor, 2);

        state.bottom(3);
        assert_eq!(state.cursor, 2);
        state.top();
        assert_eq!(state.cursor, 0);
    }

    #[test]
    fn scroll_follows_cursor_in_display_space() {
        let no_comments = DispMap::new(vec![]);
        let vp = 10;

        // Below the viewport bottom: scroll advances so the cursor is the
        // last visible display row.
        let mut scroll = 0;
        for cursor in 0..=15 {
            scroll = follow_display(scroll, cursor, &no_comments, vp);
        }
        assert_eq!(scroll, 6); // 15 - 10 + 1

        // Inside the viewport: unchanged.
        assert_eq!(follow_display(6, 14, &no_comments, vp), 6);

        // Above the viewport top: scroll snaps up to the cursor.
        assert_eq!(follow_display(6, 5, &no_comments, vp), 5);
    }

    #[test]
    fn disp_map_shifts_rows_below_comment_anchors() {
        // Comments hang under base rows 3 (twice) and 7.
        let map = DispMap::new(vec![7, 3, 3]);
        assert_eq!(map.disp(0), 0);
        assert_eq!(map.disp(3), 3); // its own comments sit BELOW it
        assert_eq!(map.extra_at(3), 2);
        assert_eq!(map.disp(4), 6); // shifted by the two comments under row 3
        assert_eq!(map.disp(8), 11); // shifted by all three
        assert_eq!(map.total(10), 13);
    }

    fn lr(start: u32, end: u32) -> LineRange {
        LineRange { start, end }
    }

    #[test]
    fn fold_runs_complements_clamps_and_merges_focus_regions() {
        // Two regions in a 60-line file: fold before, between, and after.
        assert_eq!(
            fold_runs(&[lr(10, 15), lr(40, 45)], 60),
            vec![(0, 8), (15, 38), (45, 59)]
        );
        // Region order on the wire doesn't matter.
        assert_eq!(
            fold_runs(&[lr(40, 45), lr(10, 15)], 60),
            fold_runs(&[lr(10, 15), lr(40, 45)], 60)
        );
        // A region starting at line 1 leaves no leading fold; one ending at
        // the last line leaves no trailing fold.
        assert_eq!(fold_runs(&[lr(1, 5)], 20), vec![(5, 19)]);
        assert_eq!(fold_runs(&[lr(16, 20)], 20), vec![(0, 14)]);
        // Whole-file (or over-long) region folds nothing; end clamps.
        assert_eq!(fold_runs(&[lr(1, 999)], 20), Vec::<(usize, usize)>::new());
        // No regions at all folds nothing — Some([]) is the "clear" wire form.
        assert_eq!(fold_runs(&[], 20), Vec::<(usize, usize)>::new());
        // Degenerate and out-of-file regions are dropped, not errors; if
        // nothing survives, nothing folds (rather than folding everything).
        assert_eq!(fold_runs(&[lr(9, 5), lr(0, 4), lr(30, 40)], 20), Vec::<(usize, usize)>::new());
    }

    #[test]
    fn fold_runs_keeps_short_gaps_visible() {
        // Gap of 2 rows (lines 6-7) between regions: shorter than
        // MIN_FOLD_LINES, so it stays visible and the regions merge.
        assert_eq!(fold_runs(&[lr(1, 5), lr(8, 10)], 30), vec![(10, 29)]);
        // Gap of exactly MIN_FOLD_LINES rows folds.
        assert_eq!(fold_runs(&[lr(1, 5), lr(9, 10)], 30), vec![(5, 7), (10, 29)]);
        // Short leading/trailing gaps stay visible too.
        assert_eq!(fold_runs(&[lr(3, 28)], 30), Vec::<(usize, usize)>::new());
    }

    #[test]
    fn fold_aware_disp_map_maps_both_directions() {
        // 40 base rows; fold runs (5,14) and (30,39): heads 5 and 30 render
        // as pills, tails (6..=14) and (31..=39) have no display rows.
        let map = DispMap::with_hidden(vec![], vec![(6, 14), (31, 39)]);
        assert_eq!(map.disp(0), 0);
        assert_eq!(map.disp(5), 5); // the pill row itself
        // Rows inside a tail resolve to their pill.
        assert_eq!(map.disp(6), 5);
        assert_eq!(map.disp(14), 5);
        // First row after the fold sits right under the pill.
        assert_eq!(map.disp(15), 6);
        assert_eq!(map.disp(30), 21);
        assert_eq!(map.total(40), 40 - 9 - 9);
        // Inverse: display rows land on visible base rows; the pill rows
        // resolve to the fold heads.
        assert_eq!(map.base_at(5, 40), 5);
        assert_eq!(map.base_at(6, 40), 15);
        assert_eq!(map.base_at(21, 40), 30);
        for b in [0, 3, 5, 15, 20, 30] {
            assert_eq!(map.base_at(map.disp(b), 40), b, "round trip for base {b}");
        }
    }

    #[test]
    fn fold_aware_disp_map_composes_with_comment_rows() {
        // Fold tail (6,14); comments under visible rows 3 and 20; a comment
        // anchored inside the fold is the BUILDER's job to skip, so the map
        // never sees it.
        let map = DispMap::with_hidden(vec![3, 20], vec![(6, 14)]);
        assert_eq!(map.disp(3), 3);
        assert_eq!(map.extra_at(3), 1);
        assert_eq!(map.disp(4), 5); // +1 comment row
        assert_eq!(map.disp(5), 6); // the pill
        assert_eq!(map.disp(15), 7);
        assert_eq!(map.disp(21), 14); // 21 - 9 hidden + 2 comment rows
        assert_eq!(map.total(30), 30 - 9 + 2);
        assert_eq!(map.base_at(map.disp(21), 30), 21);
        // follow_display keeps working through the composed map: jumping the
        // cursor below the fold scrolls by DISPLAY rows, not base rows.
        assert_eq!(follow_display(0, 21, &map, 10), map.disp(21) + map.extra_at(21) + 1 - 10);
    }

    /// A 40-line file on disk plus the sample diff model, in source view
    /// with an agent-pushed focus on lines 10..=15 and 30..=35 — fold runs
    /// (0,8), (15,28), (35,39).
    fn focused_app(request: &ReviewRequest, model: &Result<DiffModel>) -> App<'static> {
        // Leaked so the test app can borrow 'static — test-only fixture.
        let request: &'static ReviewRequest = Box::leak(Box::new(request.clone()));
        let model: &'static Result<DiffModel> = Box::leak(Box::new(match model {
            Ok(m) => Ok(DiffModel { files: m.files.clone() }),
            Err(_) => unreachable!("fixture model is always Ok"),
        }));
        let mut app = App::new(request, model);
        app.focus = Focus::Diff;
        app.apply_goto(
            &GotoTarget {
                file: "src/lib.rs".into(),
                line: 10,
                view: Some("source".into()),
                focus: Some(vec![lr(10, 15), lr(30, 35)]),
            },
            Size::new(120, 40),
        );
        app
    }

    fn fold_fixture(tag: &str) -> (std::path::PathBuf, ReviewRequest, Result<DiffModel>) {
        let dir = std::env::temp_dir()
            .join(format!("herdr-annotator-fold-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).expect("temp dir");
        let body = (1..=40).map(|n| format!("line{n}();")).collect::<Vec<_>>().join("\n") + "\n";
        std::fs::write(dir.join("src/lib.rs"), body).expect("write source");
        let request = ReviewRequest {
            version: 1,
            working_dir: dir.to_string_lossy().into_owned(),
            baseline: None,
            note: None,
        };
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![sample_file()] });
        (dir, request, model)
    }

    #[test]
    fn focus_push_folds_the_source_view_and_lands_on_the_first_region() {
        let (dir, request, model) = fold_fixture("land");
        let app = focused_app(&request, &model);

        assert!(app.view == ViewMode::Source);
        assert_eq!(app.active_folds(), vec![(0, 8), (15, 28), (35, 39)]);
        assert_eq!(app.diff.cursor, 9, "line 10 = base row 9, the first focused row");
        // The display shrinks accordingly: 40 rows minus the fold tails.
        let map = app.disp_map(80);
        assert_eq!(map.total(40), 40 - 8 - 13 - 4);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn movement_skips_folded_tails_and_enter_expands_a_pill() {
        let (dir, request, model) = fold_fixture("move");
        let mut app = focused_app(&request, &model);
        let size = Size::new(120, 40);

        // Down from the last row of the first region: the pill head is a
        // real stop, the tail behind it is not.
        app.diff.cursor = 14;
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), size);
        assert_eq!(app.diff.cursor, 15, "the fold's pill row is a cursor stop");
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), size);
        assert_eq!(app.diff.cursor, 29, "down through the pill continues past the fold");
        app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE), size);
        assert_eq!(app.diff.cursor, 15, "up from below the fold stops on the pill");

        // G lands in the trailing fold's tail and falls back to its pill.
        app.handle_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE), size);
        assert_eq!(app.diff.cursor, 35, "bottom inside a trailing fold stops on its pill");

        // Enter on a pill reveals that run — and only that run.
        app.diff.cursor = 15;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), size);
        assert_eq!(app.active_folds(), vec![(0, 8), (35, 39)]);
        // The revealed rows are ordinary cursor stops again.
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), size);
        assert_eq!(app.diff.cursor, 16);

        // `c` on a pill is a no-op — a comment there would silently anchor
        // to the hidden line behind it.
        app.diff.cursor = 35;
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), size);
        assert!(app.input.is_none(), "no comment box may open on a fold pill");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn goto_into_a_folded_stretch_auto_expands_it() {
        let (dir, request, model) = fold_fixture("goto");
        let mut app = focused_app(&request, &model);
        let size = Size::new(120, 40);

        // Line 20 = base row 19, inside the (15,28) fold: the agent must
        // never point the reviewer at a hidden row.
        app.apply_goto(
            &GotoTarget { file: "src/lib.rs".into(), line: 20, view: None, focus: None },
            size,
        );
        assert_eq!(app.diff.cursor, 19);
        assert_eq!(app.active_folds(), vec![(0, 8), (35, 39)]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plain_goto_keeps_focus_and_line_zero_clears_without_moving_the_cursor() {
        let (dir, request, model) = fold_fixture("clear");
        let mut app = focused_app(&request, &model);
        let size = Size::new(120, 40);

        // A plain goto inside a focused region disturbs nothing.
        app.apply_goto(
            &GotoTarget { file: "src/lib.rs".into(), line: 12, view: None, focus: None },
            size,
        );
        assert_eq!(app.active_folds().len(), 3, "a focus-less goto leaves the folds alone");

        // Clearing with the line-0 sentinel unfolds everything and leaves
        // the cursor where the reviewer had it.
        app.diff.cursor = 12;
        app.apply_goto(
            &GotoTarget {
                file: "src/lib.rs".into(),
                line: 0,
                view: None,
                focus: Some(vec![]),
            },
            size,
        );
        assert!(app.active_folds().is_empty(), "Some([]) clears the focus");
        assert_eq!(app.diff.cursor, 12, "the line-0 sentinel must not move the cursor");

        // A fresh focus after hand-expanding starts fresh (expansions reset).
        app.apply_goto(
            &GotoTarget {
                file: "src/lib.rs".into(),
                line: 10,
                view: None,
                focus: Some(vec![lr(10, 15)]),
            },
            size,
        );
        assert_eq!(app.active_folds(), vec![(0, 8), (15, 39)]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clearing_focus_on_a_file_that_is_not_selected_does_not_steal_the_pane() {
        let (dir, request, model) = fold_fixture("clear-other");
        // A second file the reviewer never navigated to, carrying a stale
        // focus from an earlier push.
        let model = model.map(|mut m| {
            let mut other = m.files[0].clone();
            other.path = "src/other.rs".to_string();
            m.files.push(other);
            m
        });
        let mut app = focused_app(&request, &model);
        let size = Size::new(120, 40);
        app.focus_regions.insert(1, vec![lr(2, 5)]);
        app.diff.cursor = 12;

        // Agent clears the stale focus on src/other.rs (index 1) while the
        // reviewer is still on src/lib.rs (index 0). The line-0 sentinel
        // must only update src/other.rs's stored state — not switch the
        // pane to it, which would reset the reviewer's cursor.
        app.apply_goto(
            &GotoTarget { file: "src/other.rs".into(), line: 0, view: None, focus: Some(vec![]) },
            size,
        );

        assert_eq!(app.nav.selected, 0, "clearing another file's focus must not steal the pane");
        assert_eq!(app.diff.cursor, 12, "the reviewer's cursor must not reset");
        assert!(!app.focus_regions.contains_key(&1), "src/other.rs's stale focus must be cleared");
        assert_eq!(app.active_folds().len(), 3, "src/lib.rs's own focus, still selected, must remain");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn focus_lands_on_the_first_surviving_region_not_a_dropped_one() {
        // fold_runs DROPS a region past the file's end; the landing must
        // follow the same normalization. Landing on the dropped region's
        // start used to clamp the cursor to the last row — inside the
        // trailing fold the surviving region just created — and auto-expand
        // it away.
        let (dir, request, model) = fold_fixture("surviving");
        let mut app = focused_app(&request, &model);
        let size = Size::new(120, 40);

        app.apply_goto(
            &GotoTarget {
                file: "src/lib.rs".into(),
                line: 100, // what the tool would send: first LISTED region's start
                view: Some("source".into()),
                focus: Some(vec![lr(100, 110), lr(10, 15)]),
            },
            size,
        );
        assert_eq!(
            app.active_folds(),
            vec![(0, 8), (15, 39)],
            "the trailing fold must survive — landing must not auto-expand it"
        );
        assert_eq!(app.diff.cursor, 9, "land on the first region that intersects the file");

        // All regions miss the file: folds nothing (fold_runs) and moves
        // nothing (landing) — one consistent "ignored advisory" outcome.
        app.diff.cursor = 20;
        app.apply_goto(
            &GotoTarget {
                file: "src/lib.rs".into(),
                line: 100,
                view: Some("source".into()),
                focus: Some(vec![lr(100, 110)]),
            },
            size,
        );
        assert!(app.active_folds().is_empty());
        assert_eq!(app.diff.cursor, 20, "a fully out-of-file focus must not move the cursor");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn folded_annotation_anchors_leave_the_disp_map_but_count_on_the_pill() {
        let (dir, request, model) = fold_fixture("notes");
        let mut app = focused_app(&request, &model);

        // One annotation on a visible line (12), one inside the (15,28)
        // fold (line 20 = row 19).
        for (start, end) in [(12u32, 12u32), (20, 21)] {
            app.pending.push(PendingAnnotation {
                file_idx: 0,
                annotation: Annotation {
                    file: "src/lib.rs".into(),
                    lines: LineRange { start, end },
                    side: Side::New,
                    tag: None,
                    comment: "note".into(),
                },
            });
        }
        let map = app.disp_map(80);
        // Only the visible annotation weaves comment rows; the folded one is
        // represented by its pill's note badge instead.
        let visible_comment_rows =
            comment_height(None, "note", 80);
        assert_eq!(map.total(40), 40 - 8 - 13 - 4 + visible_comment_rows);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn indent_block_below_finds_the_body_and_trims_trailing_blanks() {
        let lines: Vec<String> = [
            "def outer():",      // 0
            "    a = 1",         // 1
            "    if a:",         // 2
            "        b = 2",     // 3
            "",                  // 4 (interior blank)
            "        c = 3",     // 5
            "    return a",      // 6
            "",                  // 7 (trailing blank)
            "def next_fn():",    // 8
            "    pass",          // 9
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        // The whole body under the def, interior blank included, trailing
        // blank trimmed.
        assert_eq!(indent_block_below(&lines, 0), Some((1, 6)));
        // A nested block: everything deeper than the `if`.
        assert_eq!(indent_block_below(&lines, 2), Some((3, 5)));
        // A leaf line heads no block.
        assert_eq!(indent_block_below(&lines, 1), None);
        // Blank lines and out-of-range rows head nothing.
        assert_eq!(indent_block_below(&lines, 4), None);
        assert_eq!(indent_block_below(&lines, 99), None);
        // The last def's body runs to the end of the file.
        assert_eq!(indent_block_below(&lines, 8), Some((9, 9)));
    }

    #[test]
    fn indent_block_below_carries_past_closing_delimiters_at_base_indent() {
        // The Codex-reported case: a multi-line signature's closing `):`
        // (or `) {` / `}`) sits at the HEADER's indent — it belongs to the
        // block, it doesn't end it.
        let python: Vec<String> = [
            "def fetch(",       // 0
            "    url,",         // 1
            "    attempts=3,",  // 2
            "):",               // 3 — closing delimiter at base indent
            "    body()",       // 4
            "    more()",       // 5
            "next_stmt()",      // 6
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(indent_block_below(&python, 0), Some((1, 5)));

        let rust: Vec<String> = [
            "fn fetch(",             // 0
            "    url: &str,",        // 1
            ") -> Result<()> {",     // 2
            "    body();",           // 3
            "}",                     // 4 — the whole body folds, brace included
            "fn next_fn() {}",       // 5
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(indent_block_below(&rust, 0), Some((1, 4)));

        // A closing delimiter SHALLOWER than the header closes an outer
        // scope — that ends the block, it doesn't extend it.
        let nested: Vec<String> = [
            "    if a {",   // 0 (base indent 4)
            "        b();", // 1
            "}",            // 2 — outer scope's brace, indent 0
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(indent_block_below(&nested, 0), Some((1, 1)));
    }

    #[test]
    fn merge_runs_unions_overlapping_and_adjacent_runs() {
        assert_eq!(merge_runs(vec![(10, 20), (0, 5)]), vec![(0, 5), (10, 20)]);
        assert_eq!(merge_runs(vec![(0, 8), (9, 12)]), vec![(0, 12)]); // adjacent
        assert_eq!(merge_runs(vec![(0, 8), (5, 12), (12, 20)]), vec![(0, 20)]);
        assert_eq!(merge_runs(vec![]), Vec::<(usize, usize)>::new());
    }

    #[test]
    fn manual_selection_fold_collapses_and_enter_expands_it() {
        let (dir, request, model) = fold_fixture("manual");
        let request: &'static ReviewRequest = Box::leak(Box::new(request.clone()));
        let model: &'static Result<DiffModel> = Box::leak(Box::new(match &model {
            Ok(m) => Ok(DiffModel { files: m.files.clone() }),
            Err(_) => unreachable!(),
        }));
        let mut app = App::new(request, model);
        app.focus = Focus::Diff;
        let size = Size::new(120, 40);
        // Into source view with NO agent focus: manual folding stands alone.
        app.apply_goto(
            &GotoTarget { file: "src/lib.rs".into(), line: 11, view: Some("source".into()), focus: None },
            size,
        );
        assert!(app.active_folds().is_empty());

        // v, five rows down, f: rows 10..=15 fold.
        app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE), size);
        for _ in 0..5 {
            app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), size);
        }
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE), size);
        assert_eq!(app.active_folds(), vec![(10, 15)]);
        assert_eq!(app.diff.cursor, 10, "the cursor lands on the new pill");
        assert!(app.visual_anchor.is_none(), "folding consumes the selection");

        // Movement treats it like any fold; Enter dismantles it for good.
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), size);
        assert_eq!(app.diff.cursor, 16);
        app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE), size);
        assert_eq!(app.diff.cursor, 10);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), size);
        assert!(app.active_folds().is_empty());
        assert!(app.manual_folds.is_empty(), "an expanded manual fold is deleted, not parked");

        // A too-short selection folds nothing (same MIN_FOLD_LINES floor as
        // the focus path); on this flat file `f` without a selection heads
        // no indent block, so it's a no-op too.
        app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE), size);
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), size);
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE), size);
        assert!(app.active_folds().is_empty());
        app.visual_anchor = None;
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE), size);
        assert!(app.active_folds().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn block_fold_hides_the_body_and_keeps_the_header_visible() {
        let dir = std::env::temp_dir()
            .join(format!("herdr-annotator-fold-block-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).expect("temp dir");
        let body = "def outer():\n    a = 1\n    b = 2\n    c = 3\n    return a\n\ndef next_fn():\n    pass\n";
        std::fs::write(dir.join("src/lib.rs"), body).expect("write source");
        let request: &'static ReviewRequest = Box::leak(Box::new(ReviewRequest {
            version: 1,
            working_dir: dir.to_string_lossy().into_owned(),
            baseline: None,
            note: None,
        }));
        let model: &'static Result<DiffModel> =
            Box::leak(Box::new(Ok(DiffModel { files: vec![sample_file()] })));
        let mut app = App::new(request, model);
        app.focus = Focus::Diff;
        let size = Size::new(120, 40);
        app.apply_goto(
            &GotoTarget { file: "src/lib.rs".into(), line: 1, view: Some("source".into()), focus: None },
            size,
        );

        // f on the def line: the body (rows 1..=4) folds, the header stays.
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE), size);
        assert_eq!(app.active_folds(), vec![(1, 4)]);
        assert_eq!(app.diff.cursor, 0, "the header line keeps the cursor");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unfold_all_clears_manual_folds_and_expands_the_focus_ones() {
        let (dir, request, model) = fold_fixture("unfoldall");
        let mut app = focused_app(&request, &model);
        let size = Size::new(120, 40);
        assert_eq!(app.active_folds(), vec![(0, 8), (15, 28), (35, 39)]);

        // Add a manual fold in the visible region: rows 10..=12.
        app.diff.cursor = 10;
        app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE), size);
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), size);
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), size);
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE), size);
        assert_eq!(app.active_folds(), vec![(0, 8), (10, 12), (15, 28), (35, 39)]);

        app.handle_key(KeyEvent::new(KeyCode::Char('F'), KeyModifiers::NONE), size);
        assert!(app.active_folds().is_empty(), "F reveals the whole file");
        assert!(
            app.focus_regions.contains_key(&0),
            "the stored focus survives — a fresh agent push starts clean"
        );
        // And a fresh push does re-fold.
        app.apply_goto(
            &GotoTarget {
                file: "src/lib.rs".into(),
                line: 10,
                view: None,
                focus: Some(vec![lr(10, 15)]),
            },
            size,
        );
        assert_eq!(app.active_folds(), vec![(0, 8), (15, 39)]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expanding_a_merged_manual_and_focus_run_dismantles_both() {
        let (dir, request, model) = fold_fixture("merged");
        let mut app = focused_app(&request, &model);
        let size = Size::new(120, 40);

        // Manual fold rows 9..=15 (six j's — the sixth stops on the (15,28)
        // pill head): adjacent to the (0,8) focus fold and touching the
        // (15,28) one — the display shows ONE merged pill spanning (0,28).
        app.diff.cursor = 9;
        app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE), size);
        for _ in 0..6 {
            app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), size);
        }
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE), size);
        assert_eq!(app.active_folds(), vec![(0, 28), (35, 39)]);
        assert_eq!(app.diff.cursor, 0, "cursor snaps to the merged pill's head");

        // Enter dismantles every constituent the merged run covers.
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), size);
        assert_eq!(app.active_folds(), vec![(35, 39)]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fold_run_helpers_classify_heads_and_tails() {
        let runs = vec![(5, 14), (30, 39)];
        assert_eq!(fold_run_containing(&runs, 5), Some((5, 14)));
        assert_eq!(fold_run_containing(&runs, 14), Some((5, 14)));
        assert_eq!(fold_run_containing(&runs, 15), None);
        assert!(is_fold_head(&runs, 5));
        assert!(is_fold_head(&runs, 30));
        assert!(!is_fold_head(&runs, 6));
        assert!(!is_fold_head(&runs, 4));
    }

    #[test]
    fn wrap_comment_wraps_at_words_and_hard_breaks_long_ones() {
        assert_eq!(wrap_comment("short note", 20), vec!["short note"]);
        assert_eq!(
            wrap_comment("use a constant here instead of the literal", 16),
            vec!["use a constant", "here instead of", "the literal"]
        );
        // A single overlong token is hard-broken, never lost.
        let chunks = wrap_comment("abcdefghijklmnop", 5);
        assert_eq!(chunks.concat(), "abcdefghijklmnop");
        assert!(chunks.iter().all(|c| c.chars().count() <= 5));
        // Empty text still occupies one row (the live preview's empty state).
        assert_eq!(wrap_comment("", 10), vec![""]);
    }

    #[test]
    fn comment_height_matches_rendered_line_count() {
        let text = "a fairly long review comment that will definitely need wrapping at narrow widths";
        for width in [20usize, 40, 80, 200] {
            let height = comment_height(Some("fix"), text, width);
            let lines = inline_comment_lines(Some("fix"), text, width);
            assert_eq!(height, lines.len(), "width {width}");
            // Reassembling the chunks loses only layout, not content.
            let joined: String = lines
                .iter()
                .map(|l| l.spans[1].content.as_ref())
                .collect::<Vec<_>>()
                .join(" ");
            assert_eq!(joined.split_whitespace().collect::<Vec<_>>(), text.split_whitespace().collect::<Vec<_>>());
        }
    }

    #[test]
    fn inline_comment_continuation_rows_align_under_the_first_row_marker() {
        // `comment_height_matches_rendered_line_count` only reads spans[1]
        // (the wrapped text), never spans[0] (the lead), so a break in the
        // `┃ [tag] ` marker or its continuation indent would still pass.
        // Pin the lead on both the first row and continuation rows, and
        // that they render at equal display width so continuation text
        // visually lines up under the first row's text.
        let tag = Some("fix");
        let text = "a fairly long review comment that will definitely need wrapping at a narrow width";
        let width = 30;
        let lines = inline_comment_lines(tag, text, width);
        assert!(lines.len() >= 2, "expected wrapping at width {width}, got {} row(s)", lines.len());

        let marker = comment_marker(tag);
        let continuation = comment_marker_continuation(tag);
        assert_eq!(lines[0].spans[0].content.as_ref(), marker, "first row must carry the tag marker");
        assert_eq!(
            str_cols(&marker),
            str_cols(&continuation),
            "marker and continuation lead must render at the same display width"
        );
        for (i, line) in lines.iter().enumerate().skip(1) {
            assert_eq!(
                line.spans[0].content.as_ref(),
                continuation,
                "continuation row {i} must carry the aligned indent, not the marker"
            );
        }
    }

    #[test]
    fn tail_fit_keeps_the_end_of_long_input_visible() {
        assert_eq!(tail_fit("short", 10), "short");
        assert_eq!(tail_fit("abcdefghij", 6), "\u{2026}fghij");
        assert!(tail_fit("abcdefghij", 6).chars().count() <= 6);
    }

    #[test]
    fn slim_footer_shows_the_context_and_the_help_hint_when_it_fits() {
        // The slimmed non-input footer: context first, then the four
        // always-on keys, `? help` included — this is what replaced the old
        // per-focus hint sausage that used to overflow narrow panes.
        let text = slim_footer_text("a.txt:1", 80);
        assert_eq!(text, " a.txt:1 \u{b7} a approve \u{b7} r request changes \u{b7} q cancel \u{b7} ? help");
        assert!(str_cols(&text) <= 80);
    }

    #[test]
    fn slim_footer_keeps_the_context_first_when_the_pane_is_too_narrow_for_hints() {
        // Position survives, key hints get cut — the same convention the
        // old (now-removed) `diff_focus_footer` used.
        let wide_enough_for_context_only = " a.txt:1".chars().count();
        let text = slim_footer_text("a.txt:1", wide_enough_for_context_only);
        assert_eq!(text, " a.txt:1");
        assert!(!text.contains("approve"));

        // Too narrow even for the bare context: tail_fit keeps its end.
        let text = slim_footer_text("src/very/long/nested/path/file.rs:123", 10);
        assert!(str_cols(&text) <= 10, "footer must never exceed the pane width: {text:?}");
    }

    #[test]
    fn help_overlay_notes_n_p_inactive_only_in_source_view() {
        // `handle_nav_key` makes n/p no-ops outside diff view (hunk jumps
        // don't mean anything there); the overlay's Diff section rows must
        // say so precisely when that's the active view, not otherwise.
        let request = sample_request();
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![sample_file()] });
        let mut app = App::new(&request, &model);
        app.focus = Focus::Diff;

        app.view = ViewMode::Diff;
        let diff_text: String =
            app.help_lines(200).iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(diff_text.contains("next / prev hunk"));
        assert!(!diff_text.contains("inactive"));

        app.view = ViewMode::Source;
        let source_text: String =
            app.help_lines(200).iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(source_text.contains("next / prev hunk \u{2014} inactive in source view"));
    }

    #[test]
    fn summary_footer_shows_the_hint_only_when_it_fits() {
        // Pin both sides of the width-dependent `<=` boundary that decides
        // whether the "⏎ send · esc cancel" hint shows at all.
        let label = " request changes \u{2014} summary: ";
        let suffix = " \u{23ce} send \u{b7} esc cancel";
        let buf = "short summary";

        // Exactly enough room for label + buf + suffix: the fitting path,
        // hint shown, full buffer visible.
        let exact_width = label.chars().count() + buf.chars().count() + suffix.chars().count();
        assert_eq!(summary_footer_text(buf, exact_width), format!("{label}{buf}{suffix}"));

        // One column short of that: the `<=` boundary tips over to the
        // non-fitting path — hint dropped, buffer tail-fit instead (and
        // since there's still ample room for the buffer alone, unchanged).
        let text = summary_footer_text(buf, exact_width - 1);
        assert_eq!(text, format!("{label}{buf}"));
        assert!(!text.contains("send"), "hint must be dropped once it no longer fits: {text:?}");

        // Too narrow even for the buffer: hint stays dropped, and the tail
        // of the buffer (where the caret is) is what tail_fit keeps —
        // exercised directly by `tail_fit_keeps_the_end_of_long_input_visible`.
        let long = "a much longer summary than the bar can show";
        let text = summary_footer_text(long, exact_width - 1);
        assert!(!text.contains("send"));
        assert_eq!(text, format!("{label}{}", tail_fit(long, exact_width - 1 - label.chars().count())));
    }

    #[test]
    fn summary_footer_measures_the_fit_in_display_columns_not_chars() {
        // The fit decision must compare display columns, not
        // `buf.chars().count()`, to the available width: a CJK buffer (2
        // display columns per char) can "fit" by char count while its real
        // rendered width already overflows the footer once the hint suffix
        // is appended.
        let label = " request changes \u{2014} summary: ";
        let suffix = " \u{23ce} send \u{b7} esc cancel";
        let cjk: String = "\u{56fd}".repeat(10); // 10 chars, 20 display columns

        // Sized so a char-count check (10 + suffix_chars <= avail) would
        // pass, but the real column width (20 + suffix_chars) does not.
        let avail_chars = cjk.chars().count() + suffix.chars().count() + 5;
        let width = label.chars().count() + avail_chars;

        let text = summary_footer_text(&cjk, width);
        assert!(
            !text.contains("send"),
            "hint must be dropped once the CJK buffer's real column width no longer fits: {text:?}"
        );
    }

    #[test]
    fn base_at_inverts_disp_and_resolves_comment_rows_to_their_line() {
        let map = DispMap::new(vec![7, 3, 3]);
        // Round trip through every base row.
        for b in 0..10 {
            assert_eq!(map.base_at(map.disp(b), 10), b);
        }
        // Display rows 4 and 5 are the two comments under base row 3;
        // clicking them selects the annotated line.
        assert_eq!(map.base_at(4, 10), 3);
        assert_eq!(map.base_at(5, 10), 3);
        // Display row 10 is the comment under base row 7.
        assert_eq!(map.base_at(10, 10), 7);
        // Past the end clamps to the last base row; empty file clamps to 0.
        assert_eq!(map.base_at(999, 10), 9);
        assert_eq!(DispMap::new(vec![]).base_at(5, 0), 0);
    }

    #[test]
    fn follow_keeps_comment_rows_under_cursor_visible() {
        // A comment hangs under base row 9; viewport of 10 starting at 0
        // shows display rows 0..=9, but row 9's comment is display row 10 —
        // follow must scroll by one so the annotation text stays on screen.
        let map = DispMap::new(vec![9]);
        assert_eq!(follow_display(0, 9, &map, 10), 1);
        // Without the comment, no scroll needed.
        assert_eq!(follow_display(0, 9, &DispMap::new(vec![]), 10), 0);
    }

    #[test]
    fn typing_in_the_comment_box_re_follows_the_caret_as_it_grows() {
        // The active-input arm of `handle_key` must run
        // `ensure_cursor_visible` before returning: growing the editing box
        // past the viewport bottom while typing must scroll to keep the
        // caret and bottom controls visible, like every other row-count
        // change does.
        let request = sample_request();
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![sample_file()] });
        let mut app = App::new(&request, &model);
        app.focus = Focus::Diff;
        // Narrow + short terminal: a small diff viewport and a narrow wrap
        // width so a modest amount of typed text wraps into several rows.
        let size = Size::new(30, 12);
        let viewport = diff_viewport_rows(size, app.show_navigator, app.nav_width);

        // Move the cursor to the LAST row (sample_file flattens to 8 rows:
        // 0..=7) so the comment box opens right at the viewport's bottom.
        for _ in 0..7 {
            app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), size);
        }
        assert_eq!(app.diff.cursor, 7);
        assert!(app
            .handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), size)
            .is_none());

        // Type enough text to wrap across several rows at this narrow width.
        let long = "this comment is long enough to wrap across several rows at this narrow width";
        for ch in long.chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), size);
        }

        // The box's bottom row must stay inside the viewport — the same
        // invariant `ensure_cursor_visible` maintains after every other key.
        let map = app.disp_map(diff_inner_width(size, app.show_navigator, app.nav_width));
        let dc = map.disp(app.diff.cursor);
        let tail = dc + map.extra_at(app.diff.cursor);
        assert!(
            tail < app.diff.scroll + viewport,
            "box bottom (tail={tail}) must stay within the viewport (scroll={}, viewport={viewport})",
            app.diff.scroll
        );
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
    fn drain_distinguishes_disconnect_from_an_empty_channel() {
        // An empty channel is the ordinary idle case; a DISCONNECTED one
        // means the server shut the socket and the verdict can no longer be
        // delivered — the pane must exit instead of reviewing into the void.
        let request = sample_request();
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![sample_file()] });
        let mut app = App::new(&request, &model);
        let size = Size::new(80, 24);

        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(GotoTarget { file: "src/lib.rs".into(), line: 12, view: None, focus: None }).unwrap();
        assert!(!drain_navigation(&mut app, &rx, size), "live channel: not a disconnect");
        assert_eq!(app.diff.cursor, 7, "the pending goto was applied while draining");

        drop(tx);
        assert!(drain_navigation(&mut app, &rx, size), "dropped sender must surface as disconnect");
    }

    #[test]
    fn goto_targeting_a_line_past_the_diffs_end_clamps_instead_of_wrapping_to_the_top() {
        // sample_file flattens to 8 rows (0..=7); row 7 carries the final
        // new-side line, new_no 12.
        let request = sample_request();
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![sample_file()] });
        let mut app = App::new(&request, &model);
        app.diff.cursor = 3; // away from both 0 and the target, so a
                              // wrap-to-top bug is visible either way.

        app.apply_goto(
            &GotoTarget { file: "src/lib.rs".into(), line: 999, view: None, focus: None },
            Size::new(80, 24),
        );

        assert_eq!(
            app.diff.cursor, 7,
            "a target past the last new-side line must clamp to it, not wrap to row 0"
        );
    }

    #[test]
    fn goto_with_a_view_request_switches_the_pane_before_landing() {
        // The agent can steer not just WHERE but HOW to look: view "source"
        // shows the full post-change file (line maps directly), "diff" the
        // hunks; omitted keeps the current view. Advisory like the rest.
        let dir = std::env::temp_dir()
            .join(format!("herdr-annotator-goto-view-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).expect("temp dir");
        std::fs::write(dir.join("src/lib.rs"), "fn main() {\n    setup();\n    run();\n}\n")
            .expect("write source");

        let request = ReviewRequest {
            version: 1,
            working_dir: dir.to_string_lossy().into_owned(),
            baseline: None,
            note: None,
        };
        let mut ghost = sample_file();
        ghost.path = "src/gone.rs".to_string(); // in the diff, never on disk
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![sample_file(), ghost] });
        let mut app = App::new(&request, &model);
        app.focus = Focus::Diff;
        let size = Size::new(120, 40);
        assert!(app.view == ViewMode::Diff);

        app.apply_goto(
            &GotoTarget { file: "src/lib.rs".into(), line: 2, view: Some("source".into()), focus: None },
            size,
        );
        assert!(app.view == ViewMode::Source, "explicit source view request must switch");
        assert_eq!(app.diff.cursor, 1, "source view maps line 2 to row index 1");

        // Unknown view strings are ignored (advisory), current view kept.
        app.apply_goto(
            &GotoTarget { file: "src/lib.rs".into(), line: 1, view: Some("hexdump".into()), focus: None },
            size,
        );
        assert!(app.view == ViewMode::Source);

        app.apply_goto(
            &GotoTarget { file: "src/lib.rs".into(), line: 12, view: Some("diff".into()), focus: None },
            size,
        );
        assert!(app.view == ViewMode::Diff, "explicit diff view request must switch back");

        // A live `v` selection must not survive an agent-pushed view switch:
        // its anchor indexes the old view's rows, and `c` would otherwise
        // save an unrelated range (same rule as the manual toggle).
        app.visual_anchor = Some(3);
        app.apply_goto(
            &GotoTarget { file: "src/lib.rs".into(), line: 2, view: Some("source".into()), focus: None },
            size,
        );
        assert!(app.visual_anchor.is_none(), "view switch must clear the visual anchor");

        // The same contract while ALREADY in source view: enter source on a
        // usable file, then steer to one whose source side never existed —
        // the pane must fall back to diff, not show the placeholder, and the
        // effective source→diff change clears a live selection.
        app.apply_goto(
            &GotoTarget { file: "src/lib.rs".into(), line: 2, view: Some("source".into()), focus: None },
            size,
        );
        assert!(app.view == ViewMode::Source);
        app.visual_anchor = Some(1);
        app.apply_goto(
            &GotoTarget { file: "src/gone.rs".into(), line: 2, view: Some("source".into()), focus: None },
            size,
        );
        assert!(
            app.view == ViewMode::Diff,
            "already-in-source steering to an unusable file must fall back to diff"
        );
        assert!(
            app.visual_anchor.is_none(),
            "the effective source→diff change must clear the selection"
        );

        // A source request for a file with no usable source is IGNORED —
        // the diff stays on screen, per the documented contract.
        app.apply_goto(
            &GotoTarget { file: "src/lib.rs".into(), line: 12, view: Some("diff".into()), focus: None },
            size,
        );
        std::fs::remove_file(dir.join("src/lib.rs")).expect("delete source");
        app.source_cache.clear(); // force a fresh load attempt
        app.apply_goto(
            &GotoTarget { file: "src/lib.rs".into(), line: 2, view: Some("source".into()), focus: None },
            size,
        );
        assert!(
            app.view == ViewMode::Diff,
            "unusable source must keep the diff visible, not show a placeholder"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_goto_pushed_while_typing_is_replayed_once_the_input_bar_closes() {
        let request = sample_request();
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![sample_file()] });
        let mut app = App::new(&request, &model);
        let size = Size::new(80, 24);
        app.diff.cursor = 1;
        // Reviewer is mid-comment: the input bar is open.
        app.input = Some(InputMode::Summary { buf: String::new() });

        // A goto arrives while typing — must not move the cursor or disturb
        // the open input, but must not be lost either.
        app.apply_goto(&GotoTarget { file: "src/lib.rs".into(), line: 12, view: None, focus: None }, size);
        assert_eq!(app.diff.cursor, 1, "an open input bar must not be disturbed");
        assert!(app.input.is_some(), "the input bar must stay open");
        assert_eq!(app.pending_gotos.len(), 1, "the target must be held, not dropped");

        // Esc cancels the summary prompt and closes the input bar; the held
        // goto should be applied as part of that close, not lost.
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), size);
        assert!(app.input.is_none(), "Esc must close the input bar");
        assert_eq!(
            app.diff.cursor, 7,
            "the deferred goto must be applied once the input bar closes"
        );
        assert!(app.pending_gotos.is_empty(), "the held target must be consumed, not replayed again");
    }

    #[test]
    fn a_pending_focus_survives_a_same_file_goto_queued_before_the_input_closes() {
        let request = sample_request();
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![sample_file()] });
        let mut app = App::new(&request, &model);
        let size = Size::new(80, 24);
        app.input = Some(InputMode::Summary { buf: String::new() });

        // The agent's focus push arrives first and is held...
        app.apply_goto(
            &GotoTarget { file: "src/lib.rs".into(), line: 10, view: None, focus: Some(vec![lr(10, 15)]) },
            size,
        );
        // ...then a plain goto for the SAME file lands before the input
        // closes. Its own focus is None ("don't touch"), so the earlier
        // held focus must survive — the queue replays both in order.
        app.apply_goto(
            &GotoTarget { file: "src/lib.rs".into(), line: 2, view: None, focus: None },
            size,
        );

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), size);
        assert!(app.input.is_none());
        assert_eq!(
            app.focus_regions.get(&0),
            Some(&vec![lr(10, 15)]),
            "the first push's focus must not be dropped by the queued goto"
        );
    }

    #[test]
    fn a_pending_focus_survives_a_cross_file_goto_queued_before_the_input_closes() {
        // The cross-file variant of the test above: focus for file A, then a
        // goto for file B, both held while an input is open. A latest-only
        // slot dropped A's focus entirely; the queue replays both in order,
        // so A's regions are stored AND the pane ends up where the last
        // push pointed.
        let request = sample_request();
        let mut other = sample_file();
        other.path = "src/other.rs".to_string();
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![sample_file(), other] });
        let mut app = App::new(&request, &model);
        let size = Size::new(80, 24);
        app.input = Some(InputMode::Summary { buf: String::new() });

        app.apply_goto(
            &GotoTarget { file: "src/lib.rs".into(), line: 10, view: None, focus: Some(vec![lr(10, 15)]) },
            size,
        );
        app.apply_goto(
            &GotoTarget { file: "src/other.rs".into(), line: 12, view: None, focus: None },
            size,
        );
        assert_eq!(app.pending_gotos.len(), 2, "both pushes must be held");

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), size);
        assert!(app.input.is_none());
        assert_eq!(
            app.focus_regions.get(&0),
            Some(&vec![lr(10, 15)]),
            "file A's focus must survive the cross-file goto"
        );
        assert_eq!(app.nav.selected, 1, "the pane follows the last held push");
        assert_eq!(app.diff.cursor, 7, "…to file B's target row");
    }

    #[test]
    fn blank_changed_lines_keep_their_gutter_under_panning() {
        // An empty added line must still carry a content span alongside
        // gutter + marker: with only two spans, the "3+ spans → pin 2" rule
        // fails and panning consumes the line numbers.
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
                lines: vec![line(Origin::Add, None, Some(2), "")],
            }],
        };
        let rows = highlight_file_rows(highlighter(), &file);
        let blank = &rows[1];
        assert!(blank.spans.len() >= 3, "blank line must still carry a content span");

        let mut panned = blank.clone();
        pan_and_clip(&mut panned, 16, 100, 2);
        assert_eq!(panned.spans[0].content.as_ref(), "        2 ");
        assert_eq!(panned.spans[1].content.as_ref(), "+");
    }

    #[test]
    fn panning_reaches_the_end_of_very_long_lines() {
        // Panning must have no fixed ceiling: a literal .min(1000) cap
        // would make columns past ~1000 permanently unreachable on
        // generated/minified files.
        let long = "x".repeat(1500);
        let request = ReviewRequest {
            version: 1,
            working_dir: "/tmp".to_string(),
            baseline: None,
            note: None,
        };
        let file = FileDiff {
            path: "min.js".to_string(),
            old_path: None,
            status: FileStatus::Added,
            binary: false,
            adds: 1,
            dels: 0,
            hunks: vec![Hunk {
                header: "@@ -0,0 +1,1 @@".to_string(),
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 1,
                lines: vec![line(Origin::Add, None, Some(1), &long)],
            }],
        };
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![file] });
        let mut app = App::new(&request, &model);
        app.focus = Focus::Diff;

        // Widest pannable row minus two: the ‹ marker (1 col) plus this
        // row's actual trailing glyph width (1 col — plain ASCII 'x', not
        // the worst-case double-width reservation) stay visible at max pan.
        assert_eq!(app.pan_cap(), 1498);

        let term = Size { width: 120, height: 30 };
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        for _ in 0..500 {
            app.handle_key(right, term);
        }
        assert_eq!(app.diff.hscroll, 1498, "pan must reach the line's end, past 1000");

        // At maximum pan the final character is genuinely inspectable: the
        // marker lands before it, not on it.
        let mut row = app.row_cache.get(&0).unwrap()[1].clone();
        pan_and_clip(&mut row, app.diff.hscroll, 120, 2);
        let tail: String =
            row.spans.iter().skip(2).map(|s| s.content.as_ref()).collect();
        assert_eq!(tail, "\u{2039}x", "final glyph visible past the ‹ marker, got {tail:?}");
    }

    #[test]
    fn narrow_panes_can_still_reach_the_final_ascii_character() {
        // pan_cap must not reserve a flat 3 columns (marker + a
        // HYPOTHETICAL double-width final glyph) when the actual trailing
        // glyph is single-width: that wastes a column of pan reach that
        // narrow panes cannot spare. A short ASCII line in a pane with only
        // two content columns after the pinned gutter must still be able to
        // reach pan_cap without its final character being swallowed by the
        // right-clip's "…" marker.
        let short = "abcde".to_string();
        let request = ReviewRequest {
            version: 1,
            working_dir: "/tmp".to_string(),
            baseline: None,
            note: None,
        };
        let file = FileDiff {
            path: "a.txt".to_string(),
            old_path: None,
            status: FileStatus::Added,
            binary: false,
            adds: 1,
            dels: 0,
            hunks: vec![Hunk {
                // Shorter than the content line so IT (not the header)
                // determines pan_limit — the scenario under test.
                header: "@@".to_string(),
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 1,
                lines: vec![line(Origin::Add, None, Some(1), &short)],
            }],
        };
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![file] });
        let mut app = App::new(&request, &model);
        app.focus = Focus::Diff;

        let term = Size { width: 16, height: 30 };
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        for _ in 0..10 {
            app.handle_key(right, term);
        }

        // 11 pinned gutter/marker columns in a 13-column pane leaves 2
        // content columns — the narrow case this test isolates.
        let mut row = app.row_cache.get(&0).unwrap()[1].clone();
        pan_and_clip(&mut row, app.diff.hscroll, 13, 2);
        let tail: String = row.spans.iter().skip(2).map(|s| s.content.as_ref()).collect();
        assert!(
            tail.contains('e'),
            "final character 'e' must be reachable even in a narrow pane, got {tail:?}"
        );
    }

    #[test]
    fn short_rows_stay_reachable_when_a_longer_row_sets_a_big_pan_cap() {
        // A fixed 8-column HSCROLL_STEP must not jump straight past a short
        // row's entire remaining content in one press when a much longer
        // row (elsewhere in the same file) supplies a big file-wide
        // pan_cap: a 6-char row's middle/final characters must stay
        // reachable, not skip from "showing the first couple of chars" to
        // "fully panned off, empty" without ever passing through the
        // offsets that would reveal the rest.
        let request = ReviewRequest {
            version: 1,
            working_dir: "/tmp".to_string(),
            baseline: None,
            note: None,
        };
        let file = FileDiff {
            path: "a.txt".to_string(),
            old_path: None,
            status: FileStatus::Added,
            binary: false,
            adds: 2,
            dels: 0,
            hunks: vec![Hunk {
                header: "@@".to_string(), // shorter than either content line
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 2,
                lines: vec![
                    line(Origin::Add, None, Some(1), "abcdef"),
                    line(Origin::Add, None, Some(2), &"y".repeat(200)),
                ],
            }],
        };
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![file] });
        let mut app = App::new(&request, &model);
        app.focus = Focus::Diff;
        assert!(
            app.pan_cap() > HSCROLL_STEP,
            "the long row must set a cap well past a single step"
        );

        let term = Size { width: 16, height: 30 };
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);

        // 11 pinned gutter/marker columns in a 14-column pane leaves 2
        // content columns — the same narrow case as the diff-row version of
        // this test. Every one of the short row's characters must surface
        // in some frame as we step.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..6 {
            app.handle_key(right, term);
            let mut row = app.row_cache.get(&0).unwrap()[1].clone();
            pan_and_clip(&mut row, app.diff.hscroll, 14, 2);
            let tail: String = row.spans.iter().skip(2).map(|s| s.content.as_ref()).collect();
            seen.extend(tail.chars());
        }
        for c in "cdef".chars() {
            assert!(seen.contains(&c), "{c:?} never became visible while panning, saw {seen:?}");
        }

        // Fine-stepping past the short row must not cripple navigation on
        // the long row: Right still reaches the file's real pan cap.
        for _ in 0..300 {
            app.handle_key(right, term);
        }
        assert_eq!(app.diff.hscroll, app.pan_cap(), "must still reach the file-wide pan cap");
    }

    #[test]
    fn source_view_short_rows_stay_reachable_when_a_longer_row_sets_a_big_pan_cap() {
        // `next_pan_stop`'s overshoot check must scan the rows of the
        // ACTIVE view, not always `row_cache` (the diff rows): a source
        // file's line widths have nothing to do with the diff's, so a short
        // SOURCE row's overshoot must be detected too, or Right jumps the
        // full step straight past it — the same reachability invariant
        // `short_rows_stay_reachable_when_a_longer_row_sets_a_big_pan_cap`
        // pins for diff rows, here pinned for source rows.
        let dir = std::env::temp_dir()
            .join(format!("herdr-annotator-source-pan-reach-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        // On disk: a short row and a much longer one — deliberately
        // DIFFERENT from the diff's own (uniformly wide) lines below, so a
        // next_pan_stop that checks diff rows instead of source rows would
        // see no short row at all and never fine-step.
        std::fs::write(dir.join("a.txt"), format!("abcdef\n{}\n", "y".repeat(200)))
            .expect("write source");

        let request = ReviewRequest {
            version: 1,
            working_dir: dir.to_string_lossy().into_owned(),
            baseline: None,
            note: None,
        };
        let file = FileDiff {
            path: "a.txt".to_string(),
            old_path: None,
            status: FileStatus::Added,
            binary: false,
            adds: 2,
            dels: 0,
            hunks: vec![Hunk {
                // Wide even as the (unpinned, single-span) header row: every
                // diff row here must be wide, or the header itself would be
                // the "short row" that (correctly, by accident) triggers
                // the overshoot check regardless of which rows are scanned.
                header: "@".repeat(200),
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 2,
                // Uniformly wide diff rows: if next_pan_stop wrongly checks
                // these instead of the source rows, it never detects an
                // overshoot and always jumps the full step.
                lines: vec![
                    line(Origin::Add, None, Some(1), &"z".repeat(200)),
                    line(Origin::Add, None, Some(2), &"z".repeat(200)),
                ],
            }],
        };
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![file] });
        let mut app = App::new(&request, &model);
        app.focus = Focus::Diff;
        // A WIDE pane, deliberately: a narrow one clamps `pan_step` itself
        // down to single columns (see `pan_step`'s adaptive narrow-pane
        // rule), which fine-steps regardless of the overshoot check and
        // would mask this bug. Only a pane wide enough for the full
        // `HSCROLL_STEP` makes the overshoot branch the thing deciding
        // whether the short row is ever revealed.
        let size = Size { width: 120, height: 40 };
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE), size);
        assert!(app.view == ViewMode::Source);
        assert_eq!(app.pan_step(size), HSCROLL_STEP, "must be wide enough for a full step");
        assert!(
            app.pan_cap() > HSCROLL_STEP,
            "the long source row must set a cap well past a single step"
        );

        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..6 {
            app.handle_key(right, size);
            let source = match app.source_cache.get(&0) {
                Some(Ok(source)) => source,
                _ => panic!("expected the source to load"),
            };
            let mut row = source.lines[0].clone();
            pan_and_clip(&mut row, app.diff.hscroll, 100, 2);
            let tail: String = row.spans.iter().skip(2).map(|s| s.content.as_ref()).collect();
            seen.extend(tail.chars());
        }
        for c in "cdef".chars() {
            assert!(
                seen.contains(&c),
                "{c:?} never became visible while panning source view, saw {seen:?}"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn short_rows_stay_reachable_at_an_exact_step_boundary() {
        // `next_pan_stop`'s overshoot check must include a row whose
        // pannable width lands EXACTLY on the step boundary (an 8-char row
        // against the default HSCROLL_STEP=8), not only `cols < target`: a
        // strict-less-than check lets the first Right press jump straight
        // from offset 0 to offset 8 — where that row is already fully
        // panned off, empty — skipping every offset that would have
        // revealed its middle/tail characters.
        let request = ReviewRequest {
            version: 1,
            working_dir: "/tmp".to_string(),
            baseline: None,
            note: None,
        };
        let file = FileDiff {
            path: "a.txt".to_string(),
            old_path: None,
            status: FileStatus::Added,
            binary: false,
            adds: 2,
            dels: 0,
            hunks: vec![Hunk {
                // Empty: a nonempty header (even a short one) would itself
                // fall strictly inside (0, 8) and trigger fine-stepping on
                // its own, masking the exact-8 boundary this test isolates.
                header: String::new(),
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 2,
                lines: vec![
                    line(Origin::Add, None, Some(1), "abcdefgh"), // exactly HSCROLL_STEP cols
                    line(Origin::Add, None, Some(2), &"y".repeat(200)),
                ],
            }],
        };
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![file] });
        let mut app = App::new(&request, &model);
        app.focus = Focus::Diff;

        let term = Size { width: 16, height: 30 };
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..8 {
            app.handle_key(right, term);
            let mut row = app.row_cache.get(&0).unwrap()[1].clone();
            pan_and_clip(&mut row, app.diff.hscroll, 14, 2);
            let tail: String = row.spans.iter().skip(2).map(|s| s.content.as_ref()).collect();
            seen.extend(tail.chars());
        }
        for c in "cdefgh".chars() {
            assert!(seen.contains(&c), "{c:?} never became visible while panning, saw {seen:?}");
        }
    }

    #[test]
    fn pan_cap_protects_a_trailing_flag_pair_whole() {
        // trailing_cell_width must size the reserve from the row's whole
        // trailing CLUSTER, not only its last scalar. For a row ending in a
        // regional-indicator flag, the last scalar alone is one RI (1 col)
        // — but a flag is an atomic 2-scalar pair (protected elsewhere in
        // this file). Reserving for only the last scalar lets pan_cap land
        // the boundary cleanly BEFORE the flag, where the marker-
        // replacement step (which only ever protects trailing zero-width
        // marks on the char it overwrites, not a whole second cluster
        // scalar) eats the flag's first RI and leaves the second standing
        // alone.
        let content = "abcdef\u{1f1fa}\u{1f1f8}"; // 6 letters + US flag (2 RI scalars)
        let request = ReviewRequest {
            version: 1,
            working_dir: "/tmp".to_string(),
            baseline: None,
            note: None,
        };
        let file = FileDiff {
            path: "a.txt".to_string(),
            old_path: None,
            status: FileStatus::Added,
            binary: false,
            adds: 1,
            dels: 0,
            hunks: vec![Hunk {
                header: "@@".to_string(),
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 1,
                lines: vec![line(Origin::Add, None, Some(1), content)],
            }],
        };
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![file] });
        let app = App::new(&request, &model);

        // 8 pannable cols total (6 letters + 2 RI scalars); the reserve
        // must protect the whole 2-col pair, not just its last scalar.
        assert_eq!(app.pan_cap(), 5, "reserve must be 1 (marker) + 2 (whole flag pair)");

        let mut row = app.row_cache.get(&0).unwrap()[1].clone();
        pan_and_clip(&mut row, app.pan_cap(), 100, 2);
        let visible = row.spans[2].content.as_ref();
        assert!(
            visible.contains('\u{1f1fa}') && visible.contains('\u{1f1f8}'),
            "the whole flag must survive together at max pan, got {visible:?}"
        );
    }

    #[test]
    fn narrow_panes_pan_by_fine_steps() {
        // In a pane exposing only a couple of code columns, whole 8-column
        // jumps must not skip offsets that were never on screen — that
        // would hide short rows' middles forever. The step caps at half the
        // visible code columns (min 1).
        let request = ReviewRequest {
            version: 1,
            working_dir: "/tmp".to_string(),
            baseline: None,
            note: None,
        };
        let rows = vec![
            line(Origin::Add, None, Some(1), "abcdefghi"), // 9 cols
            line(Origin::Add, None, Some(2), &"x".repeat(120)),
        ];
        let file = FileDiff {
            path: "a.py".to_string(),
            old_path: None,
            status: FileStatus::Added,
            binary: false,
            adds: 2,
            dels: 0,
            hunks: vec![Hunk {
                header: "@@ -0,0 +1,2 @@".to_string(),
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 2,
                lines: rows,
            }],
        };
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![file] });
        let mut app = App::new(&request, &model);
        app.focus = Focus::Diff;
        app.show_navigator = false;

        // Stacked/narrow: 16-wide terminal → 14 inner → 2 code columns.
        let narrow = Size { width: 16, height: 24 };
        assert_eq!(app.pan_step(narrow), 1);
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        app.handle_key(right, narrow);
        assert_eq!(app.diff.hscroll, 1, "narrow panes advance one column at a time");

        // Wide terminal: full step (modulo the short-row fine-step rule).
        let wide = Size { width: 120, height: 30 };
        assert_eq!(app.pan_step(wide), HSCROLL_STEP);
    }

    #[test]
    fn focusing_files_from_a_collapsed_navigator_reveals_it() {
        // h/Tab must reveal the navigator before focusing it: focusing a
        // hidden navigator would let j/k switch files invisibly and leave
        // diff keys dead.
        let request = ReviewRequest {
            version: 1,
            working_dir: "/tmp".to_string(),
            baseline: None,
            note: None,
        };
        let file = FileDiff {
            path: "a.py".to_string(),
            old_path: None,
            status: FileStatus::Added,
            binary: false,
            adds: 1,
            dels: 0,
            hunks: vec![Hunk {
                header: "@@ -0,0 +1,1 @@".to_string(),
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 1,
                lines: vec![line(Origin::Add, None, Some(1), "x")],
            }],
        };
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![file] });
        let mut app = App::new(&request, &model);
        app.focus = Focus::Diff;
        app.show_navigator = false;

        let term = Size { width: 120, height: 30 };
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), term);
        assert!(app.show_navigator, "focusing the files must reveal them");
        assert!(matches!(app.focus, Focus::Navigator));
    }

    #[test]
    fn pan_and_clip_counts_display_columns_not_chars() {
        // Panning and clipping must count display columns, not chars: a
        // chars().count() measure would let CJK/emoji content pan double
        // the columns and evade right-clipping.
        let mk = |content: &str| {
            Line::from(vec![
                Span::raw("   1    2  "),
                Span::raw("+"),
                Span::raw(content.to_string()),
            ])
        };

        // "你好世界" = 4 chars but 8 display columns.
        // Pan 2 columns: exactly the first wide char goes; the ‹ marker
        // replaces the next wide char's cell as "‹ " to keep alignment.
        let mut line = mk("\u{4f60}\u{597d}\u{4e16}\u{754c}");
        pan_and_clip(&mut line, 2, 100, 2);
        assert_eq!(line.spans[2].content.as_ref(), "\u{2039} \u{4e16}\u{754c}");
        assert_eq!(str_cols(line.spans[2].content.as_ref()), 6); // 8 - 2

        // Pan 1 column: the first wide char straddles the boundary — it is
        // dropped whole and a pad keeps columns aligned; marker takes the pad.
        let mut line = mk("\u{4f60}\u{597d}\u{4e16}\u{754c}");
        pan_and_clip(&mut line, 1, 100, 2);
        assert_eq!(str_cols(line.spans[2].content.as_ref()), 7); // 8 - 1
        assert!(line.spans[2].content.starts_with('\u{2039}'));

        // Right clip in columns: gutter+marker (12) + "abc你好" (7) = 19
        // display columns; width 16 keeps 15 columns + the … marker, and a
        // wide char never straddles past the budget.
        let mut line = mk("abc\u{4f60}\u{597d}");
        pan_and_clip(&mut line, 0, 16, 2);
        let total: usize = line.spans.iter().map(|s| str_cols(s.content.as_ref())).sum();
        assert!(total <= 16, "rendered {total} cols > width 16");
        assert_eq!(line.spans.last().unwrap().content.as_ref(), "\u{2026}");

        // Decomposed text: "a" + combining acute + "b" renders as 2 cells.
        // Panning one column drops the accented cell WITH its mark (no
        // orphaned combining char attaching to the marker), and the ‹
        // replaces the b cell: exactly 1 column remains.
        let mut line = mk("a\u{301}b");
        pan_and_clip(&mut line, 1, 100, 2);
        assert_eq!(line.spans[2].content.as_ref(), "\u{2039}");
        assert_eq!(str_cols(line.spans[2].content.as_ref()), 1);

        // Marker replaces a whole cell even when the surviving first cell
        // carries its own combining mark.
        let mut line = mk("a\u{301}e\u{301}b");
        pan_and_clip(&mut line, 1, 100, 2);
        assert_eq!(line.spans[2].content.as_ref(), "\u{2039}b");

        // ZWJ emoji sequence (family): ONE grapheme cluster, whose width is
        // whatever unicode-width 0.2 says ratatui will render it as. A pan
        // boundary can only ever drop it whole — no partial family, no
        // dangling joiner — and the remaining columns follow exactly.
        let family = "\u{1f469}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}";
        let content = format!("{family}abcdefgh");
        let total_before = str_cols(&content);
        let mut line = mk(&content);
        pan_and_clip(&mut line, 4, 100, 2);
        let visible = line.spans[2].content.as_ref();
        assert!(!visible.contains('\u{200d}'), "no joiner may survive a pan");
        assert!(
            visible.contains(family) || !visible.contains('\u{1f469}'),
            "family must be whole or gone, never partial: {visible:?}"
        );
        assert_eq!(str_cols(visible), total_before - 4);

        // Right clip that lands after a ZWJ trims the dangling joiner.
        let mut line = mk(&content);
        pan_and_clip(&mut line, 0, 15, 2); // 12 pinned + 3 → cuts inside the cluster
        let visible: String = line.spans.iter().skip(2).map(|s| s.content.as_ref()).collect();
        assert!(!visible.trim_end_matches('\u{2026}').ends_with('\u{200d}'));

        // A right clip must never render a partial family (a COUPLE is a
        // genuinely different emoji). Under the render model the whole
        // family is one narrow cluster, so it either fits entirely or drops
        // entirely — assert exactly that invariant at a budget that cuts
        // within the following text, and at one too small for the cluster
        // at all.
        let mut line = mk(&content);
        pan_and_clip(&mut line, 0, 17, 2);
        let visible = line.spans[2].content.as_ref().to_string();
        assert!(
            visible.contains(family) || !visible.contains('\u{1f469}'),
            "family must be whole or gone, never a couple: {visible:?}"
        );
        let total: usize = line.spans.iter().map(|s| str_cols(s.content.as_ref())).sum();
        assert!(total <= 17, "rendered {total} cols > width 17");
        assert_eq!(line.spans.last().unwrap().content.as_ref(), "\u{2026}");

        // Budget of a single column cannot hold the 2-col family cluster:
        // it drops whole, no couple, no dangling joiner.
        let mut line = mk(&content);
        pan_and_clip(&mut line, 0, 14, 2); // 12 pinned + 1 col budget
        let visible = line.spans[2].content.as_ref();
        assert!(visible.is_empty(), "cluster over budget must drop whole, got {visible:?}");
        assert_eq!(line.spans.last().unwrap().content.as_ref(), "\u{2026}");

        // Indic conjuncts: KA + virama + SSA is ONE cluster (UAX #29 GB9c).
        // A zero-width heuristic that drops the virama but keeps SSA
        // renders "‹ष" — a bare consonant instead of the source conjunct.
        // The marker must consume the conjunct whole.
        let conjunct = "\u{915}\u{94d}\u{937}"; // क्ष
        let mut line = mk(&format!("a{conjunct}b"));
        pan_and_clip(&mut line, 1, 100, 2); // pan off the 'a'
        let visible = line.spans[2].content.as_ref();
        assert!(
            visible.contains(conjunct) || !visible.contains('\u{937}'),
            "conjunct must be whole or gone, never a bare piece: {visible:?}"
        );
        assert!(visible.starts_with('\u{2039}'));
    }

    #[test]
    fn pan_and_clip_keeps_regional_indicator_flags_atomic() {
        // A flag is exactly two regional-indicator scalars with NO joiner
        // between them (unlike every other cluster this file protects) —
        // pairing is purely positional. A pan or clip boundary landing
        // between the two must drop both, not leave one to render alone as
        // an orphaned boxed letter instead of the source flag.
        let mk = |content: &str| {
            Line::from(vec![
                Span::raw("   1    2  "), // 11-col mock gutter (pinned)
                Span::raw("+"),           // 1-col mock marker (pinned)
                Span::raw(content.to_string()),
            ])
        };
        let flag = "\u{1f1fa}\u{1f1f8}"; // US flag: 2 RI scalars, 1 col each
        let content = format!("{flag}abcdefgh");

        // Pan boundary lands inside the cluster ahead of the flag (a CJK
        // char carrying a trailing ZWJ — per UAX #29 the ZWJ does NOT glue
        // it to the following flag, matching how terminals render it). The
        // CJK cluster drops whole with a pad; the flag must survive INTACT:
        // the invariant is that a flag never splits, wherever the boundary
        // lands.
        let bridged = format!("\u{4f60}\u{200d}{content}"); // CJK+ZWJ cluster, then flag + text
        let mut line = mk(&bridged);
        pan_and_clip(&mut line, 1, 100, 2);
        let visible = line.spans[2].content.as_ref();
        let intact = visible.contains(flag);
        let absent = !visible.contains('\u{1f1fa}') && !visible.contains('\u{1f1f8}');
        assert!(intact || absent, "flag must be whole or gone, never split: {visible:?}");
        assert!(intact, "flag is a separate cluster and must survive this pan: {visible:?}");
        assert!(visible.ends_with("abcdefgh"), "trailing text intact, got {visible:?}");

        // Right clip lands between the two RI scalars (first kept, second
        // dropped by the budget): the walk-back must drop the retained
        // first RI too, not leave it standing alone.
        let mut line = mk(&content);
        pan_and_clip(&mut line, 0, 14, 2); // 12 pinned + 1 → cuts inside the flag
        let visible = line.spans[2].content.as_ref();
        assert!(visible.is_empty(), "no lone RI from the cut flag may survive, got {visible:?}");
        assert_eq!(line.spans.last().unwrap().content.as_ref(), "\u{2026}");

        // A clean cut exactly AT the flag boundary (not inside it) is
        // unaffected: the whole flag pans off normally, nothing orphaned.
        // The ‹ marker replaces the first surviving cell ('a'), as usual.
        let mut line = mk(&content);
        pan_and_clip(&mut line, 2, 100, 2);
        let visible = line.spans[2].content.as_ref();
        assert!(!visible.contains('\u{1f1fa}') && !visible.contains('\u{1f1f8}'));
        assert!(visible.ends_with("bcdefgh"), "got {visible:?}");
    }

    #[test]
    fn pan_marker_replaces_the_whole_leading_cluster_not_just_its_first_scalar() {
        // The pan boundary can land immediately before an intact
        // multi-scalar cluster (not cut through it — that atomicity is
        // handled elsewhere). The marker-replacement step must swap the
        // whole leading cluster for "‹", not just its first SCALAR (plus
        // trailing zero-width marks): swapping only the first scalar splits
        // a cluster the pan logic upstream deliberately kept whole. Example:
        // panning "abcdefgh🇺🇸xyz" by 8 lands right at the flag, so the
        // marker must consume both regional indicators, not orphan the
        // second one as "‹🇸xyz".
        let mk = |content: &str| {
            Line::from(vec![
                Span::raw("   1    2  "),
                Span::raw("+"),
                Span::raw(content.to_string()),
            ])
        };
        let mut line = mk("abcdefgh\u{1f1fa}\u{1f1f8}xyz");
        pan_and_clip(&mut line, 8, 100, 2);
        let visible = line.spans[2].content.as_ref();
        assert_eq!(visible, "\u{2039} xyz", "must replace the WHOLE flag pair, got {visible:?}");
        assert_eq!(str_cols(visible), 5); // marker(1) + pad(1) + xyz(3) = 5

        // Clean-boundary case with a ZWJ cluster too: panning off exactly
        // up to a family emoji must mark the whole family, not just its
        // first pictograph.
        let family = "\u{1f469}\u{200d}\u{1f469}\u{200d}\u{1f467}\u{200d}\u{1f466}"; // 8 cols
        let mut line = mk(&format!("abcdefgh{family}xyz"));
        pan_and_clip(&mut line, 8, 100, 2);
        let visible = line.spans[2].content.as_ref();
        assert!(!visible.contains('\u{200d}'), "no joiner may survive, got {visible:?}");
        assert!(!visible.contains('\u{1f469}'), "no partial family may render, got {visible:?}");
        assert!(visible.ends_with("xyz"), "trailing text intact, got {visible:?}");
    }

    #[test]
    fn navigator_toggle_reflows_the_viewport() {
        // 'b' must run ensure_cursor_visible before returning: re-showing
        // the navigator in a stacked (narrow) layout can shrink the diff
        // viewport, and without that call the cursor would be stranded
        // off-screen until the next navigation key.
        let request = ReviewRequest {
            version: 1,
            working_dir: "/tmp".to_string(),
            baseline: None,
            note: None,
        };
        let rows: Vec<DiffLine> =
            (1..=40).map(|i| line(Origin::Add, None, Some(i), "x")).collect();
        let file = FileDiff {
            path: "a.py".to_string(),
            old_path: None,
            status: FileStatus::Added,
            binary: false,
            adds: 40,
            dels: 0,
            hunks: vec![Hunk {
                header: "@@ -0,0 +1,40 @@".to_string(),
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 40,
                lines: rows,
            }],
        };
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![file] });
        let mut app = App::new(&request, &model);
        app.focus = Focus::Diff;
        app.show_navigator = false;

        // 40 cols → stacked layout once the navigator returns.
        let term = Size { width: 40, height: 24 };
        app.diff.cursor = 30;
        app.ensure_cursor_visible(term);

        let key = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE);
        assert!(app.handle_key(key, term).is_none());
        assert!(app.show_navigator);

        let map = app.disp_map(diff_inner_width(term, true, 0));
        let viewport = diff_viewport_rows(term, true, 0);
        let cursor_disp = map.disp(app.diff.cursor);
        assert!(
            cursor_disp >= app.diff.scroll && cursor_disp < app.diff.scroll + viewport,
            "cursor display row {cursor_disp} outside viewport [{}, {})",
            app.diff.scroll,
            app.diff.scroll + viewport
        );
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
        let (nav, diff) = body_split(Rect::new(0, 0, 40, 30), true, 0);
        assert_eq!(nav.width, 40);
        assert_eq!(diff.width, 40);
        assert!(nav.height >= 4 && nav.height <= 10);
        assert_eq!(nav.y + nav.height, diff.y);

        // Wide pane keeps the horizontal split with the 24-col floor.
        let (nav, diff) = body_split(Rect::new(0, 0, 120, 30), true, 0);
        assert_eq!(nav.height, 30);
        assert_eq!(nav.width, 36);
        assert_eq!(nav.x + nav.width, diff.x);

        // Collapsed navigator: the diff takes the whole body at any width.
        let (nav, diff) = body_split(Rect::new(0, 0, 40, 30), false, 0);
        assert_eq!(nav.width, 0);
        assert_eq!(diff, Rect::new(0, 0, 40, 30));
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE }
    }

    #[test]
    fn dragging_the_divider_resizes_the_navigator() {
        let request = sample_request();
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![sample_file()] });
        let mut app = App::new(&request, &model);
        let size = Size::new(120, 40);
        // Auto width 36 at 120 cols: the divider is the diff's left border
        // (x == 36) and the navigator's right border (x == 35).

        // Grabbing the divider starts a resize, NOT a selection drag.
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 36, 10), size);
        assert!(app.resizing_navigator);
        assert!(app.drag_origin.is_none(), "a divider grab must not seed a selection");

        // The boundary follows the mouse column, through the same clamps
        // as the bracket keys.
        app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 25, 10), size);
        assert_eq!(app.nav_width, 25);
        app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 3, 10), size);
        assert_eq!(app.nav_width, NAV_MIN_WIDTH);
        app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 119, 10), size);
        assert_eq!(app.nav_width, nav_max_width(120));

        // Release ends the resize; a later drag is back to selecting.
        app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 119, 10), size);
        assert!(!app.resizing_navigator);
        let width_after = app.nav_width;
        app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 60, 10), size);
        assert_eq!(app.nav_width, width_after, "a drag without a divider grab must not resize");

        // A plain click in the diff still clicks (cursor + drag origin),
        // never resizes.
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 90, 10), size);
        assert!(!app.resizing_navigator);
        assert!(app.drag_origin.is_some());
        app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 90, 10), size);

        // Stacked layout: no vertical divider exists to grab.
        let narrow = Size::new(50, 40);
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 20, 10), narrow);
        assert!(!app.resizing_navigator);
    }

    #[test]
    fn body_split_honors_and_clamps_a_navigator_width_preference() {
        let area = Rect::new(0, 0, 120, 30);
        // An explicit preference replaces the 30% auto width.
        let (nav, diff) = body_split(area, true, 20);
        assert_eq!(nav.width, 20);
        assert_eq!(nav.width + diff.width, 120);
        // Preferences clamp: never narrower than NAV_MIN_WIDTH, never so
        // wide the code pane drops below its readable floor.
        assert_eq!(body_split(area, true, 2).0.width, NAV_MIN_WIDTH);
        // 120-col area, 40-col diff floor: pin the concrete boundary so a
        // regression in DIFF_READABLE_FLOOR or the formula is caught here.
        assert_eq!(body_split(area, true, 200).0.width, 80);
        // 0 = auto: unchanged 30%/min-24 behavior.
        assert_eq!(body_split(area, true, 0).0.width, 36);
        // The stacked layout ignores width preferences entirely.
        let (nav, _) = body_split(Rect::new(0, 0, 40, 30), true, 20);
        assert_eq!(nav.width, 40, "stacked layout keeps full-width rows");
    }

    #[test]
    fn bracket_keys_resize_the_navigator_and_respect_the_clamps() {
        let request = sample_request();
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![sample_file()] });
        let mut app = App::new(&request, &model);
        let size = Size::new(120, 40);

        // First press starts from the RENDERED auto width (36 at 120 cols).
        app.handle_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE), size);
        assert_eq!(app.nav_width, 36 + NAV_RESIZE_STEP);
        app.handle_key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE), size);
        app.handle_key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE), size);
        assert_eq!(app.nav_width, 36 - NAV_RESIZE_STEP);

        // Shrinking bottoms out at NAV_MIN_WIDTH...
        for _ in 0..20 {
            app.handle_key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE), size);
        }
        assert_eq!(app.nav_width, NAV_MIN_WIDTH);
        // ...and widening tops out where the code pane stays readable.
        for _ in 0..40 {
            app.handle_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE), size);
        }
        assert_eq!(app.nav_width, 80);

        // Hidden navigator: the keys are inert, the preference untouched.
        let before = app.nav_width;
        app.show_navigator = false;
        app.handle_key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE), size);
        assert_eq!(app.nav_width, before);
        app.show_navigator = true;

        // Stacked layout (narrow pane): no width to trade, keys inert.
        let narrow = Size::new(50, 40);
        app.handle_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE), narrow);
        assert_eq!(app.nav_width, before);
    }

    #[test]
    fn pan_and_clip_pins_gutter_and_marks_both_edges() {
        let mk = || {
            Line::from(vec![
                Span::raw("   1    2  "),        // gutter (pinned)
                Span::raw("+"),                  // marker (pinned)
                Span::raw("let alpha = "),       // content…
                Span::raw("beta + gamma;"),
            ])
        };

        // Pan 4: gutter+marker intact, first 4 content chars gone, and the
        // ‹ marker REPLACES the first visible char (keeping column widths).
        let mut line = mk();
        pan_and_clip(&mut line, 4, 100, 2);
        assert_eq!(line.spans[0].content.as_ref(), "   1    2  ");
        assert_eq!(line.spans[1].content.as_ref(), "+");
        assert_eq!(line.spans[2].content.as_ref(), "\u{2039}lpha = ");
        assert_eq!(line.spans[3].content.as_ref(), "beta + gamma;");

        // No pan, narrow width: right edge clipped with a … marker and the
        // rendered width never exceeds the pane.
        let mut line = mk();
        pan_and_clip(&mut line, 0, 20, 2);
        let total: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
        assert_eq!(total, 20);
        assert_eq!(line.spans.last().unwrap().content.as_ref(), "\u{2026}");

        // Fits: untouched, no markers.
        let mut line = mk();
        pan_and_clip(&mut line, 0, 100, 2);
        assert_eq!(line.spans.len(), 4);
        assert_eq!(line.spans[2].content.as_ref(), "let alpha = ");

        // Pan past the end of content: everything unpinned empties, no panic.
        let mut line = mk();
        pan_and_clip(&mut line, 500, 100, 2);
        assert!(line.spans[2].content.is_empty() && line.spans[3].content.is_empty());
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

    #[test]
    fn question_mark_opens_the_help_overlay_and_question_mark_or_esc_close_it() {
        let request = sample_request();
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![sample_file()] });
        let mut app = App::new(&request, &model);
        let size = Size::new(120, 40);
        assert!(!app.help_open);

        assert!(app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE), size).is_none());
        assert!(app.help_open, "`?` must open the overlay");

        assert!(app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE), size).is_none());
        assert!(!app.help_open, "`?` must close the overlay again");

        app.help_open = true;
        assert!(app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), size).is_none());
        assert!(!app.help_open, "Esc must close the overlay");
    }

    #[test]
    fn q_closes_the_overlay_without_cancelling_but_cancels_normally_once_closed() {
        let request = sample_request();
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![sample_file()] });
        let mut app = App::new(&request, &model);
        let size = Size::new(120, 40);

        app.help_open = true;
        let outcome = app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), size);
        assert!(outcome.is_none(), "q inside the overlay must not produce an outcome");
        assert!(!app.help_open, "q inside the overlay must close it");

        // The SAME key, now that the overlay is closed, cancels as usual.
        let outcome = app
            .handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), size)
            .expect("q cancels once the overlay is closed");
        assert_eq!(outcome.verdict, Verdict::Cancelled);
    }

    #[test]
    fn question_mark_types_into_an_open_comment_input_instead_of_opening_the_overlay() {
        let request = sample_request();
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![sample_file()] });
        let mut app = App::new(&request, &model);
        app.focus = Focus::Diff;
        app.input = Some(InputMode::Comment {
            buf: String::new(),
            tag: None,
            editing: None,
            row_start: 0,
            row_end: 0,
        });
        let size = Size::new(120, 40);

        assert!(app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE), size).is_none());
        assert!(!app.help_open, "`?` must not open the overlay while an input bar is open");
        assert!(
            matches!(&app.input, Some(InputMode::Comment { buf, .. }) if buf == "?"),
            "expected the literal `?` to land in the still-open comment buffer"
        );
    }

    #[test]
    fn help_overlay_rows_fit_the_overlay_width_at_a_narrow_terminal_size() {
        let request = sample_request();
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![sample_file()] });
        let app = App::new(&request, &model);
        let term_size = Size::new(40, 24);

        let (width, _) = help_overlay_geometry(term_size, 0);
        let inner_width = width.saturating_sub(2).max(1) as usize;
        for line in app.help_lines(inner_width) {
            let text = line_text(&line);
            assert!(
                str_cols(&text) <= inner_width,
                "row {text:?} ({} cols) exceeds the overlay's inner width ({inner_width})",
                str_cols(&text)
            );
        }
    }

    #[test]
    fn diff_focus_footer_text_carries_the_position_and_the_help_hint() {
        let request = sample_request();
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![sample_file()] });
        let mut app = App::new(&request, &model);
        app.focus = Focus::Diff; // App::new defaults to Focus::Navigator
        let width = 80usize;

        let context = app.cursor_position().expect("cursor sits on a line");
        let text = slim_footer_text(&context, width);
        assert!(text.contains("? help"), "footer must advertise the new help overlay: {text:?}");
        assert!(text.contains(&context), "footer must still show the position: {text:?}");
        assert!(str_cols(&text) <= width, "footer must fit the pane: {text:?}");
    }

    #[test]
    fn editing_box_height_matches_woven_row_count() {
        let long = "a fairly long note that will wrap across several lines at a narrow width";
        let cases: [(&str, usize); 6] = [
            ("", 10),
            ("", 40),
            ("short", 40),
            ("short", 6),
            (long, 20),
            (long, 60),
        ];
        for (buf, width) in cases {
            let height = editing_box_height(buf, width);
            let lines = editing_box_lines(buf, Some(Tag::Fix), width);
            assert_eq!(height, lines.len(), "buf={buf:?} width={width}");
            // Always at least the two rules plus one content row (even for
            // an empty buffer, which still renders a caret-only row).
            assert!(height >= 3);
        }
    }

    #[test]
    fn editing_box_pads_by_display_columns_not_chars_for_wide_glyphs() {
        // Content-row padding must measure display columns, not
        // `chars().count()`: counting chars would pad a row of CJK text (2
        // display columns per glyph, 1 char) as if every glyph were 1
        // column wide, undercounting the row's real rendered width and
        // pushing the closing `┆` border past the box's actual width,
        // violating the documented never-overflow invariant.
        let cjk: String = "\u{56fd}".repeat(20); // 20x '国', 2 display columns each
        let width = 40;
        for (i, line) in editing_box_lines(&cjk, Some(Tag::Fix), width).into_iter().enumerate() {
            let cols: usize = line.spans.iter().map(|s| str_cols(&s.content)).sum();
            assert_eq!(cols, width, "row {i} must render at exactly the box width, got {cols}");
        }
    }

    #[test]
    fn editing_box_never_exceeds_a_pathologically_narrow_pane() {
        // The box must never be forced to a minimum width: forcing it to 4
        // would render it wider than panes with fewer inner columns, and
        // even at exactly 4 the forced one-column wrap width plus prefix,
        // caret, and closing border would make each content row 5 columns
        // wide — both would violate the documented never-overflow
        // invariant.
        for width in 1..=6usize {
            for buf in ["", "hi", "a longer note than the pane can hold"] {
                for (i, line) in editing_box_lines(buf, Some(Tag::Fix), width).into_iter().enumerate() {
                    let cols: usize = line.spans.iter().map(|s| str_cols(&s.content)).sum();
                    assert!(
                        cols <= width,
                        "width={width} buf={buf:?} row={i}: rendered {cols} cols, wider than the pane"
                    );
                }
            }
        }
    }

    #[test]
    fn editing_annotation_idx_matches_the_comment_input_mode() {
        // "Which pending annotation is being edited" must be computed in
        // one place, not duplicated in App::disp_map and draw_diff: a
        // single method keeps the rendered rows and the scroll/mouse
        // display map from drifting apart from each other.
        let request = sample_request();
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![sample_file()] });
        let mut app = App::new(&request, &model);

        assert_eq!(app.editing_annotation_idx(), None, "no input open: nothing is being edited");

        app.input = Some(InputMode::Comment {
            buf: String::new(),
            tag: None,
            editing: None,
            row_start: 0,
            row_end: 0,
        });
        assert_eq!(
            app.editing_annotation_idx(),
            None,
            "a fresh comment (editing: None) isn't editing a saved annotation"
        );

        app.input = Some(InputMode::Comment {
            buf: String::new(),
            tag: None,
            editing: Some(2),
            row_start: 0,
            row_end: 0,
        });
        assert_eq!(app.editing_annotation_idx(), Some(2));

        app.input = Some(InputMode::Summary { buf: String::new() });
        assert_eq!(app.editing_annotation_idx(), None, "the summary bar is a different input mode entirely");
    }

    #[test]
    fn disp_map_skips_edited_annotation_and_counts_the_box_instead() {
        let request = sample_request();
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![sample_file()] });
        let mut app = App::new(&request, &model);
        app.pending.push(PendingAnnotation {
            file_idx: 0,
            annotation: Annotation {
                file: "src/lib.rs".to_string(),
                lines: LineRange { start: 2, end: 2 },
                side: Side::New,
                tag: Some("fix".to_string()),
                comment: "a saved note".to_string(),
            },
        });

        let inner_width = 40;
        // The annotation carries no display anchor of its own: new-side line
        // 2 is the add row, flattened row 2 (header, context, add, …).
        assert_eq!(app.pending_anchor(0), Some((2, 2)));

        // Not editing: disp_map counts the saved annotation's own rows.
        let saved_h = comment_height(Some("fix"), "a saved note", inner_width);
        let map = app.disp_map(inner_width);
        assert_eq!(map.extra_at(2), saved_h);

        // Open edit mode on that same annotation (at its anchor row).
        app.input = Some(InputMode::Comment {
            buf: "editing now, with a much longer replacement comment".to_string(),
            tag: Some(Tag::Fix),
            editing: Some(0),
            row_start: 2,
            row_end: 2,
        });

        // The saved annotation's rows are gone from the map; only the
        // editing box's rows remain at the same anchor.
        let box_h = editing_box_height(
            "editing now, with a much longer replacement comment",
            inner_width,
        );
        let map = app.disp_map(inner_width);
        assert_eq!(map.extra_at(2), box_h);
        assert_eq!(map.total(app.diff_rows().len()), app.diff_rows().len() + box_h);
    }

    // --- annotation anchoring (protocol lines -> display rows) ----------

    /// An annotation on `sample_file`, spelled by side and line range.
    fn anno(side: Side, start: u32, end: u32) -> Annotation {
        Annotation {
            file: "src/lib.rs".to_string(),
            lines: LineRange { start, end },
            side,
            tag: None,
            comment: "why".to_string(),
        }
    }

    #[test]
    fn annotation_rows_new_side_range_spans_add_and_context_rows() {
        let file = sample_file();
        let rows = flatten_rows(&file);
        // New-side lines 2..=3 are the add row (row 2) and the context row
        // after it (row 3) — the range covers both, and the header above
        // them is not a Line row so it never anchors.
        assert_eq!(annotation_rows(&anno(Side::New, 2, 3), &rows), Some((2, 3)));
        // A single line still yields a degenerate (min, max).
        assert_eq!(annotation_rows(&anno(Side::New, 12, 12), &rows), Some((7, 7)));
    }

    #[test]
    fn annotation_rows_old_side_anchors_to_the_remove_row() {
        let file = sample_file();
        let rows = flatten_rows(&file);
        // Old-side line 10 exists only on the removed line (row 5); the add
        // row next to it carries new_no 11, which the Old side ignores.
        assert_eq!(annotation_rows(&anno(Side::Old, 10, 10), &rows), Some((5, 5)));
    }

    #[test]
    fn annotation_rows_returns_none_when_no_row_matches() {
        let file = sample_file();
        let rows = flatten_rows(&file);
        // New-side lines 4..=10 fall between the two hunks: not in the diff
        // at all, so there is nothing to dot or weave under.
        assert!(annotation_rows(&anno(Side::New, 4, 10), &rows).is_none());
        // Old-side line 3 likewise: the old file's line 3 is not shown.
        assert!(annotation_rows(&anno(Side::Old, 3, 3), &rows).is_none());
    }

    #[test]
    fn source_annotation_rows_are_line_numbers_clamped_to_the_file() {
        // Lines 3..=5 of a 10-line file are rows 2..=4.
        assert_eq!(source_annotation_rows(&anno(Side::New, 3, 5), 10), Some((2, 4)));
        // Shorter file: the range clamps to its last row instead of vanishing.
        assert_eq!(source_annotation_rows(&anno(Side::New, 3, 5), 4), Some((2, 3)));
        assert_eq!(source_annotation_rows(&anno(Side::New, 3, 5), 3), Some((2, 2)));
        // Old-side annotations have no place in the source view, and neither
        // does anything when there is no source (deleted/binary/unreadable).
        assert!(source_annotation_rows(&anno(Side::Old, 3, 5), 10).is_none());
        assert!(source_annotation_rows(&anno(Side::New, 3, 5), 0).is_none());
    }

    #[test]
    fn cursor_remaps_between_diff_rows_and_source_lines() {
        let file = sample_file();
        let rows = flatten_rows(&file);

        // Diff -> source: the row's new-side line, zero-based.
        assert_eq!(diff_row_to_source_row(&rows, 2), 1); // add, new line 2
        assert_eq!(diff_row_to_source_row(&rows, 7), 11); // context, new line 12
        // Rows with no new side (hunk header, removed line) fall to the top.
        assert_eq!(diff_row_to_source_row(&rows, 0), 0);
        assert_eq!(diff_row_to_source_row(&rows, 5), 0);
        assert_eq!(diff_row_to_source_row(&rows, 99), 0);

        // Source -> diff: the first row showing that line.
        assert_eq!(source_row_to_diff_row(&rows, 1), 2); // line 2 -> add row
        assert_eq!(source_row_to_diff_row(&rows, 11), 7); // line 12 -> last row
        assert_eq!(source_row_to_diff_row(&rows, 0), 1); // line 1 -> first context
        // A line outside every hunk isn't in the diff: fall to the top.
        assert_eq!(source_row_to_diff_row(&rows, 5), 0);
    }

    // --- source view ----------------------------------------------------

    #[test]
    fn source_rows_carry_gutter_spacer_and_content_spans() {
        let lines: Vec<String> = ["fn main() {", "", "    let x = 1;"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let rows = highlight_source_rows(highlighter(), "src/lib.rs", &lines);
        assert_eq!(rows.len(), 3);

        for (i, row) in rows.iter().enumerate() {
            // [gutter, spacer, content…] — at least three spans on EVERY row
            // (blank lines included) so `draw_diff`'s `spans.len() >= 3 -> pin
            // 2` rule keeps the gutter and spacer put while panning.
            assert!(row.spans.len() >= 3, "row {i} has {} spans", row.spans.len());
            let gutter = row.spans[0].content.as_ref();
            assert_eq!(gutter.trim(), (i + 1).to_string(), "row {i} gutter");
            assert_eq!(gutter.chars().count(), SOURCE_NUMBER_WIDTH + 2);
            assert_eq!(row.spans[1].content.as_ref(), " ");
            let content: String = row.spans[2..].iter().map(|s| s.content.as_ref()).collect();
            assert_eq!(content, lines[i], "row {i} content");
        }

        // The pin actually holds: panning past the whole line leaves the
        // gutter and spacer untouched.
        let mut row = rows[0].clone();
        let pinned = if row.spans.len() >= 3 { 2 } else { 0 };
        pan_and_clip(&mut row, 500, 80, pinned);
        assert_eq!(row.spans[0].content.trim(), "1");
        assert_eq!(row.spans[1].content.as_ref(), " ");
    }

    #[test]
    fn source_view_toggles_and_annotates_by_line_number() {
        // A real file on disk: the source view reads the worktree, which IS
        // the new side of the diff whatever the baseline was.
        let dir = std::env::temp_dir()
            .join(format!("herdr-annotator-source-view-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).expect("temp dir");
        std::fs::write(
            dir.join("src/lib.rs"),
            "fn main() {\n    setup();\n    run();\n}\n",
        )
        .expect("write source");

        let request = ReviewRequest {
            version: 1,
            working_dir: dir.to_string_lossy().into_owned(),
            baseline: None,
            note: None,
        };
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![sample_file()] });
        let mut app = App::new(&request, &model);
        app.focus = Focus::Diff;
        let size = Size::new(120, 40);

        // `t` switches to source and loads the file: 4 rows, one per line.
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE), size);
        assert!(app.view == ViewMode::Source);
        assert_eq!(app.view_row_count(), 4);

        // Comment on source row 2, i.e. new-side line 3.
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), size);
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), size);
        assert_eq!(app.diff.cursor, 2);
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), size);
        for ch in "here".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), size);
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), size);

        assert_eq!(app.pending.len(), 1);
        let saved = &app.pending[0].annotation;
        assert_eq!(saved.side, Side::New);
        assert_eq!((saved.lines.start, saved.lines.end), (3, 3));
        assert_eq!(saved.file, "src/lib.rs");
        assert_eq!(app.pending_anchor(0), Some((2, 2)));

        // Back to the diff: the SAME annotation re-anchors to the diff row
        // showing new-side line 3, and the cursor follows the same line.
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE), size);
        assert!(app.view == ViewMode::Diff);
        assert_eq!(app.pending_anchor(0), Some((3, 3)));
        assert_eq!(app.diff.cursor, 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn editing_across_views_keeps_the_original_range_not_the_current_views_anchor() {
        // Saving an edit must keep the annotation's original protocol
        // range, not re-derive it from the CURRENT view's row span. An
        // annotation created in source view can cover lines that are only
        // PARTLY present in the diff (most of the file isn't in any hunk);
        // re-deriving the range from diff view would narrow lines 4..=10
        // down to whatever subset the diff actually shows — silently
        // corrupting the range even when the edit changes nothing but hits
        // Enter.
        let dir = std::env::temp_dir()
            .join(format!("herdr-annotator-cross-view-edit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        // 10 lines on disk so a source-view selection can span lines 4..=10.
        let source = (1..=10).map(|n| format!("line{n}")).collect::<Vec<_>>().join("\n") + "\n";
        std::fs::write(dir.join("a.txt"), source).expect("write source");

        // The diff only shows new-side lines 5 and 6 — everything else in
        // the file (including most of the 4..=10 range below) is far
        // context, outside every hunk.
        let file = FileDiff {
            path: "a.txt".to_string(),
            old_path: None,
            status: FileStatus::Modified,
            binary: false,
            adds: 2,
            dels: 0,
            hunks: vec![Hunk {
                header: "@@ -5,2 +5,2 @@".to_string(),
                old_start: 5,
                old_count: 2,
                new_start: 5,
                new_count: 2,
                lines: vec![
                    line(Origin::Add, None, Some(5), "line5"),
                    line(Origin::Add, None, Some(6), "line6"),
                ],
            }],
        };
        let request = ReviewRequest {
            version: 1,
            working_dir: dir.to_string_lossy().into_owned(),
            baseline: None,
            note: None,
        };
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![file] });
        let mut app = App::new(&request, &model);
        app.focus = Focus::Diff;
        let size = Size::new(120, 40);

        // Switch to source view and select source rows 3..=9 (lines 4..=10).
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE), size);
        assert!(app.view == ViewMode::Source);
        app.diff.cursor = 3;
        app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE), size);
        for _ in 0..6 {
            app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), size);
        }
        assert_eq!(app.diff.cursor, 9);
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), size);
        for ch in "note".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), size);
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), size);

        assert_eq!(app.pending.len(), 1);
        let original = (app.pending[0].annotation.lines.start, app.pending[0].annotation.lines.end);
        assert_eq!(original, (4, 10), "source selection must save as the full line range");

        // Back to diff view: this annotation only anchors to rows 1..=2
        // there (lines 5 and 6 are all the diff shows).
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE), size);
        assert!(app.view == ViewMode::Diff);
        assert_eq!(app.pending_anchor(0), Some((1, 2)), "diff view only shows part of the range");

        // Reopen the SAME annotation for editing from diff view and save
        // again with NO changes to the text — the range alone must survive.
        app.diff.cursor = 1;
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), size);
        match &app.input {
            Some(InputMode::Comment { editing, buf, .. }) => {
                assert_eq!(*editing, Some(0));
                assert_eq!(buf, "note");
            }
            other => panic!(
                "expected comment input in edit mode, got a different state (variant present: {})",
                other.is_some()
            ),
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), size);

        assert_eq!(app.pending.len(), 1, "editing must not duplicate the annotation");
        let after_edit =
            (app.pending[0].annotation.lines.start, app.pending[0].annotation.lines.end);
        assert_eq!(
            after_edit, original,
            "editing from a different view must not change the saved range"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn source_view_pan_cap_is_computed_from_source_rows_not_diff_rows() {
        // The diff's widest row is a short one-liner (see `sample_file`), but
        // the file on disk has a much longer line — a change elsewhere in
        // the same file that never shows up in the diff's hunks. Source
        // view's pan cap must reflect ITS OWN widest row, not the diff's.
        let dir = std::env::temp_dir()
            .join(format!("herdr-annotator-source-pan-cap-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).expect("temp dir");
        let long_line = "x".repeat(300);
        std::fs::write(
            dir.join("src/lib.rs"),
            format!("fn main() {{\n    setup();\n    {long_line}\n}}\n"),
        )
        .expect("write source");

        let request = ReviewRequest {
            version: 1,
            working_dir: dir.to_string_lossy().into_owned(),
            baseline: None,
            note: None,
        };
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![sample_file()] });
        let mut app = App::new(&request, &model);
        app.focus = Focus::Diff;
        let size = Size::new(120, 40);

        // Diff view: the cap comes from the diff's own (short) rows,
        // precomputed once in `App::new`.
        let diff_cap = app.pan_cap();
        let diff_rows = app.row_cache.get(&0).cloned().unwrap_or_default();
        assert_eq!(diff_cap, pan_cap_for_rows(&diff_rows));

        // `t`: load the source and compute ITS OWN cap, lazily.
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE), size);
        assert!(app.view == ViewMode::Source);
        let source_rows = match app.source_cache.get(&0) {
            Some(Ok(source)) => source.lines.clone(),
            _ => panic!("expected the source to load"),
        };
        let source_cap = app.pan_cap();
        assert_eq!(source_cap, pan_cap_for_rows(&source_rows));
        assert!(
            source_cap > diff_cap,
            "the long line on disk must widen the source cap past the diff cap \
             ({source_cap} vs {diff_cap})"
        );

        // Back to diff view: the ORIGINAL diff cap returns, not the source
        // one left over from the view we just left.
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE), size);
        assert!(app.view == ViewMode::Diff);
        assert_eq!(app.pan_cap(), diff_cap);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scroll_row_u16_clamps_past_the_line_cap_margin_instead_of_wrapping() {
        // MAX_SOURCE_LINES's margin bounds the source's BASE rows, but
        // saved comments and the open editing box add unbounded extra
        // display rows on top — a source near the line cap with enough
        // wrapped comments can still push `scroll` past `u16::MAX`. A bare
        // `as u16` cast would wrap that back down to a small offset and
        // render unrelated earlier rows while the cursor/footer still
        // report the true, later position; clamping instead just stops
        // scrolling further, which is the safe failure.
        assert_eq!(scroll_row_u16(0), 0);
        assert_eq!(scroll_row_u16(65_535), 65_535, "must reach the real max exactly");
        assert_eq!(scroll_row_u16(65_536), 65_535, "one past the max clamps, not wraps");
        assert_eq!(scroll_row_u16(200_000), 65_535, "far past the max still clamps to it");
    }

    #[test]
    fn missing_source_falls_back_to_a_placeholder_row() {
        // working_dir points nowhere: the file can't be read, so the source
        // view is a single placeholder row and annotating there is inert.
        let request = ReviewRequest {
            version: 1,
            working_dir: std::env::temp_dir()
                .join("herdr-annotator-does-not-exist")
                .to_string_lossy()
                .into_owned(),
            baseline: None,
            note: None,
        };
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![sample_file()] });
        let mut app = App::new(&request, &model);
        app.focus = Focus::Diff;
        let size = Size::new(120, 40);

        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE), size);
        assert_eq!(app.view_row_count(), 1);
        assert_eq!(app.source_line_count(), 0);
        let lines = app.view_lines();
        assert_eq!(lines.len(), 1);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.starts_with("(no source view: "), "got {text:?}");

        // `c` + Enter saves nothing: there are no source lines to anchor to.
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), size);
        for ch in "nope".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), size);
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), size);
        assert!(app.pending.is_empty());
    }

    #[test]
    fn source_view_refuses_a_file_that_changed_since_the_review_started() {
        // The diff is a snapshot of the moment the review began, but
        // source view reads the worktree fresh the first time `t` is
        // pressed — which can be much later. If something touches the file
        // in between, the two views must not silently show different
        // revisions: a source-view annotation's line numbers would no
        // longer describe the diff the agent receives.
        let dir = std::env::temp_dir()
            .join(format!("herdr-annotator-source-drift-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let full = dir.join("a.txt");
        std::fs::write(&full, "original\n").expect("write source");

        let request = ReviewRequest {
            version: 1,
            working_dir: dir.to_string_lossy().into_owned(),
            baseline: None,
            note: None,
        };
        let mut file = sample_file();
        file.path = "a.txt".to_string();
        let model: Result<DiffModel> = Ok(DiffModel { files: vec![file] });
        // `App::new` stats the file NOW — this is the review-start baseline.
        let mut app = App::new(&request, &model);
        app.focus = Focus::Diff;
        let size = Size::new(120, 40);

        // Simulate something touching the file after the review started:
        // set its mtime an hour back, deterministically (no reliance on
        // filesystem mtime resolution or wall-clock sleeps).
        let file = std::fs::OpenOptions::new().write(true).open(&full).expect("reopen");
        let drifted = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        file.set_modified(drifted).expect("backdate mtime");

        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE), size);
        assert!(app.view == ViewMode::Source);
        let text: String = app.view_lines()[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("changed"), "expected a drift placeholder, got {text:?}");

        // An UNTOUCHED file still loads normally — this isn't refusing
        // every file, only ones that actually drifted.
        std::fs::write(dir.join("b.txt"), "steady\n").expect("write steady file");
        let mut steady_file = sample_file();
        steady_file.path = "b.txt".to_string();
        let model2: Result<DiffModel> = Ok(DiffModel { files: vec![steady_file] });
        let mut app2 = App::new(&request, &model2);
        app2.focus = Focus::Diff;
        app2.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE), size);
        assert_eq!(app2.source_line_count(), 1, "an untouched file must still load");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bottom_rule_shows_chips_when_roomy_and_never_overflows_when_narrow() {
        let wide = bottom_rule_line(Some(Tag::Fix), 80);
        let text: String = wide.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("commit"), "expected a commit chip, got {text:?}");
        assert!(text.contains("esc"), "expected a cancel chip, got {text:?}");
        let rendered: usize = wide.spans.iter().map(|s| s.content.chars().count()).sum();
        assert_eq!(rendered, 80);

        for w in [1usize, 3, 6, 10, 15, 24] {
            let line = bottom_rule_line(None, w);
            let rendered: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
            assert!(rendered <= w, "width {rendered} exceeds available {w}");
        }
    }

    #[test]
    fn bottom_rule_drops_the_tag_chip_before_the_cancel_chip() {
        // Pins the precedence `bottom_rule_line`'s own docstring states:
        // chips drop right-to-priority as the pane narrows (tag first, then
        // cancel, commit never), so a medium-width pane keeps commit+cancel
        // and drops the only visible `Ctrl-T` discoverability. Covers the
        // constrained-width case in between the "everything fits" and
        // "nothing fits" ends `bottom_rule_shows_chips_when_roomy_and_never_overflows_when_narrow`
        // already exercises.
        let contains = |w: usize, needle: &str| {
            let line = bottom_rule_line(Some(Tag::Fix), w);
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            text.contains(needle)
        };
        let first_width_with =
            |needle: &str| (1..=100).find(|&w| contains(w, needle)).expect("must fit by width 100");
        let cancel_at = first_width_with("esc"); // only in the cancel chip
        let tag_at = first_width_with("^T"); // only in the tag chip
        assert!(
            cancel_at < tag_at,
            "cancel (first fits at {cancel_at}) must become visible at a narrower width than \
             tag (first fits at {tag_at}) — cancel is the higher-priority chip"
        );
        // And once the pane is wide enough for the tag chip, cancel is
        // still there — commit+cancel is never traded away for tag.
        assert!(contains(tag_at, "esc"));
    }
}
