//! Structured diff model: what the pane renders and what M3 annotations anchor to.
//!
//! Line identity matters more than presentation here: every `DiffLine` carries
//! its old/new file line numbers so a future annotation ("comment on new-side
//! lines 112–118 of src/portal.rs") can be derived directly from the cursor
//! position without re-parsing anything.

use std::process::Command;

use anyhow::{bail, Context, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Modified,
    Added,
    Deleted,
    Renamed,
    /// Not yet tracked by git; shown as an all-added pseudo-diff.
    Untracked,
}

impl FileStatus {
    /// One-column marker for the navigator, matching git status vocabulary.
    pub fn marker(self) -> &'static str {
        match self {
            FileStatus::Modified => "M",
            FileStatus::Added => "A",
            FileStatus::Deleted => "D",
            FileStatus::Renamed => "R",
            FileStatus::Untracked => "?",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Context,
    Add,
    Remove,
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub origin: Origin,
    /// Line number in the old file (None for added lines).
    pub old_no: Option<u32>,
    /// Line number in the new file (None for removed lines).
    pub new_no: Option<u32>,
    /// Content without the leading +/-/space marker.
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct Hunk {
    /// The full `@@ -a,b +c,d @@ …` header line as emitted by git.
    pub header: String,
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone)]
pub struct FileDiff {
    /// New path (post-change); for deletions, the old path.
    pub path: String,
    /// Old path when renamed.
    pub old_path: Option<String>,
    pub status: FileStatus,
    /// Binary files carry no hunks; render a placeholder instead.
    pub binary: bool,
    pub hunks: Vec<Hunk>,
    pub adds: u32,
    pub dels: u32,
}

#[derive(Debug, Clone, Default)]
pub struct DiffModel {
    pub files: Vec<FileDiff>,
}

/// Load the full review model for a repo: `git diff` vs the baseline (or all
/// uncommitted changes when None), plus untracked files rendered as all-added
/// pseudo-diffs so new files are reviewable, not invisible.
pub fn load(working_dir: &str, baseline: Option<&str>) -> Result<DiffModel> {
    let _ = (working_dir, baseline);
    bail!("diff loader not implemented yet"); // M2 task: parser + loader
}

/// Parse `git diff --no-color` unified output into file diffs.
pub fn parse_unified(text: &str) -> Result<Vec<FileDiff>> {
    let _ = text;
    bail!("diff parser not implemented yet"); // M2 task: parser + loader
}

#[allow(dead_code)]
fn git(working_dir: &str, args: &[&str]) -> Result<std::process::Output> {
    Command::new("git")
        .arg("-C")
        .arg(working_dir)
        .args(args)
        .output()
        .context("running git")
}
