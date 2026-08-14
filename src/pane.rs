//! The review pane herdr spawns beside the agent.
//!
//! M1 scope: connect to the handoff socket, show the requested diff in a
//! scrollable colored view, and return a verdict (approve / request changes /
//! cancel) — with an optional one-line summary when requesting changes.
//! Line-anchored annotations arrive in M3.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute, queue,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{self, ClearType},
};

use crate::protocol::{PaneConnection, ReviewRequest, ReviewResult, Verdict, PROTOCOL_VERSION, SOCKET_ENV};

pub fn run() -> Result<()> {
    let socket_path = match std::env::var(SOCKET_ENV) {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            eprintln!("herdr-annotator pane: {SOCKET_ENV} is not set.");
            eprintln!("This binary is meant to be opened by the annotator MCP server, not by hand.");
            std::thread::sleep(Duration::from_secs(8));
            anyhow::bail!("{SOCKET_ENV} not set");
        }
    };
    let mut conn = PaneConnection::connect(&socket_path)?;
    let request = conn.receive_request()?;
    let diff = load_diff(&request);

    let outcome = review_ui(&request, &diff)?;
    let result = ReviewResult {
        version: PROTOCOL_VERSION,
        verdict: outcome.verdict,
        summary: outcome.summary,
        annotations: Vec::new(),
    };
    conn.send_result(&result)?;
    println!(
        "verdict sent: {}",
        serde_json::to_string(&result.verdict).unwrap_or_default()
    );
    Ok(())
}

struct Outcome {
    verdict: Verdict,
    summary: Option<String>,
}

/// Diff text plus a marker for "nothing changed".
struct DiffView {
    lines: Vec<String>,
    error: bool,
}

fn load_diff(request: &ReviewRequest) -> DiffView {
    let run = |args: &[&str]| -> Result<std::process::Output> {
        Command::new("git")
            .arg("-C")
            .arg(&request.working_dir)
            .args(args)
            .output()
            .context("running git")
    };
    let output = match &request.baseline {
        Some(rev) => run(&["diff", rev]),
        // No explicit baseline: everything uncommitted. Fall back for unborn HEAD.
        None => match run(&["diff", "HEAD"]) {
            Ok(out) if out.status.success() => Ok(out),
            _ => run(&["diff"]),
        },
    };
    let mut view = match output {
        Ok(out) if out.status.success() => DiffView {
            lines: String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect(),
            error: false,
        },
        Ok(out) => DiffView {
            lines: format!(
                "git diff failed ({}):\n{}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            )
            .lines()
            .map(str::to_string)
            .collect(),
            error: true,
        },
        Err(err) => DiffView { lines: vec![format!("{err:#}")], error: true },
    };
    // Untracked files never show in `git diff` — list them so new files aren't invisible.
    if !view.error {
        if let Ok(out) = run(&["ls-files", "--others", "--exclude-standard"]) {
            let untracked: Vec<&str> =
                std::str::from_utf8(&out.stdout).unwrap_or("").lines().collect();
            if !untracked.is_empty() {
                view.lines.push(String::new());
                view.lines.push(format!("untracked files ({}):", untracked.len()));
                view.lines.extend(untracked.iter().map(|f| format!("?? {f}")));
            }
        }
        if view.lines.is_empty() {
            view.lines.push("(no changes vs baseline)".to_string());
        }
    }
    view
}

/// RAII guard so a panic can't leave the terminal in raw/alternate mode.
struct TermGuard;
impl TermGuard {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(io::stdout(), terminal::EnterAlternateScreen, cursor::Hide)?;
        Ok(TermGuard)
    }
}
impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), cursor::Show, terminal::LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

fn review_ui(request: &ReviewRequest, diff: &DiffView) -> Result<Outcome> {
    let _guard = TermGuard::enter()?;
    let mut scroll: usize = 0;
    loop {
        draw(request, diff, scroll)?;
        match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                let (_, rows) = terminal::size()?;
                let page = rows.saturating_sub(3).max(1) as usize;
                let max_scroll = diff.lines.len().saturating_sub(page);
                match (key.code, key.modifiers) {
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                        return Ok(Outcome {
                            verdict: Verdict::Cancelled,
                            summary: Some("reviewer cancelled".into()),
                        })
                    }
                    (KeyCode::Char('q'), _) | (KeyCode::Esc, _) => {
                        return Ok(Outcome {
                            verdict: Verdict::Cancelled,
                            summary: Some("reviewer cancelled".into()),
                        })
                    }
                    (KeyCode::Char('a'), _) => {
                        return Ok(Outcome { verdict: Verdict::Approve, summary: None })
                    }
                    (KeyCode::Char('r'), _) => {
                        if let Some(summary) = prompt_line("request changes — summary: ")? {
                            return Ok(Outcome {
                                verdict: Verdict::RequestChanges,
                                summary: if summary.is_empty() { None } else { Some(summary) },
                            });
                        } // Esc during input returns to the diff.
                    }
                    (KeyCode::Char('j'), _) | (KeyCode::Down, _) => {
                        scroll = (scroll + 1).min(max_scroll)
                    }
                    (KeyCode::Char('k'), _) | (KeyCode::Up, _) => scroll = scroll.saturating_sub(1),
                    (KeyCode::Char('d'), _) | (KeyCode::PageDown, _) => {
                        scroll = (scroll + page).min(max_scroll)
                    }
                    (KeyCode::Char('u'), _) | (KeyCode::PageUp, _) => {
                        scroll = scroll.saturating_sub(page)
                    }
                    (KeyCode::Char('g'), _) => scroll = 0,
                    (KeyCode::Char('G'), _) => scroll = max_scroll,
                    _ => {}
                }
            }
            Event::Resize(..) => {}
            _ => {}
        }
    }
}

fn draw(request: &ReviewRequest, diff: &DiffView, scroll: usize) -> Result<()> {
    let mut out = io::stdout();
    let (cols, rows) = terminal::size()?;
    let width = cols as usize;
    let body_rows = rows.saturating_sub(3).max(1) as usize;

    queue!(out, terminal::Clear(ClearType::All), cursor::MoveTo(0, 0))?;

    // Header: what's being reviewed + the agent's note.
    let title = format!(
        " REVIEW  {}  (vs {})",
        request.working_dir,
        request.baseline.as_deref().unwrap_or("uncommitted")
    );
    queue!(
        out,
        SetAttribute(Attribute::Reverse),
        Print(pad(&title, width)),
        SetAttribute(Attribute::Reset)
    )?;
    queue!(out, cursor::MoveTo(0, 1))?;
    let note = request
        .note
        .as_deref()
        .map(|n| format!(" agent: {n}"))
        .unwrap_or_else(|| " agent is waiting for your review".to_string());
    queue!(out, SetForegroundColor(Color::Yellow), Print(pad(&note, width)), ResetColor)?;

    for (row, line) in diff.lines.iter().skip(scroll).take(body_rows).enumerate() {
        queue!(out, cursor::MoveTo(0, (row + 2) as u16))?;
        let color = line_color(line);
        if let Some(c) = color {
            queue!(out, SetForegroundColor(c))?;
        }
        queue!(out, Print(clip(line, width)), ResetColor)?;
    }

    let footer = format!(
        " a approve · r request changes · q cancel · j/k scroll   [{}/{}]",
        (scroll + body_rows).min(diff.lines.len()),
        diff.lines.len()
    );
    queue!(
        out,
        cursor::MoveTo(0, rows.saturating_sub(1)),
        SetAttribute(Attribute::Reverse),
        Print(pad(&footer, width)),
        SetAttribute(Attribute::Reset)
    )?;
    out.flush()?;
    Ok(())
}

fn line_color(line: &str) -> Option<Color> {
    if line.starts_with("+++") || line.starts_with("---") || line.starts_with("diff --git") {
        Some(Color::Blue)
    } else if line.starts_with('+') {
        Some(Color::Green)
    } else if line.starts_with('-') {
        Some(Color::Red)
    } else if line.starts_with("@@") {
        Some(Color::Cyan)
    } else if line.starts_with("??") {
        Some(Color::Magenta)
    } else {
        None
    }
}

fn clip(s: &str, width: usize) -> String {
    s.chars().take(width).collect()
}

fn pad(s: &str, width: usize) -> String {
    let mut clipped = clip(s, width);
    let len = clipped.chars().count();
    clipped.extend(std::iter::repeat(' ').take(width.saturating_sub(len)));
    clipped
}

/// One-line input on the bottom row. Enter submits, Esc aborts (returns None).
fn prompt_line(label: &str) -> Result<Option<String>> {
    let mut buf = String::new();
    let mut out = io::stdout();
    loop {
        let (cols, rows) = terminal::size()?;
        let width = cols as usize;
        let line = format!(" {label}{buf}");
        queue!(
            out,
            cursor::MoveTo(0, rows.saturating_sub(1)),
            SetAttribute(Attribute::Reverse),
            Print(pad(&line, width)),
            SetAttribute(Attribute::Reset)
        )?;
        out.flush()?;
        if let Event::Key(key) = event::read()? {
            if key.kind == KeyEventKind::Release {
                continue;
            }
            match key.code {
                KeyCode::Enter => return Ok(Some(buf.trim().to_string())),
                KeyCode::Esc => return Ok(None),
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => buf.push(c),
                _ => {}
            }
        }
    }
}
