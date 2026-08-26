//! Structured diff model: what the pane renders and what M3 annotations anchor to.
//!
//! Line identity matters more than presentation here: every `DiffLine` carries
//! its old/new file line numbers so a future annotation ("comment on new-side
//! lines 112–118 of src/portal.rs") can be derived directly from the cursor
//! position without re-parsing anything.

use std::io::Read as _;
use std::path::Path;
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

/// Display tab width for source text. Terminals treat `\t` as a jump to the
/// next tab stop, and ratatui writes cell contents through verbatim, so a
/// raw tab in a rendered line makes the terminal place every later cell
/// further right than ratatui's buffer believes it did. Its diff-based
/// redraw then never repairs those cells, and stale text from earlier
/// frames piles up on screen (tab-indented Go is the worst case). Expand
/// tabs before any text reaches the render path so buffer and screen agree.
pub const TAB_WIDTH: usize = 4;

/// Replace each tab with the spaces needed to reach the next multiple of
/// [`TAB_WIDTH`] columns, counting the column by characters seen so far
/// (good enough for the ASCII indentation tabs actually occur in).
pub fn expand_tabs(text: &str) -> String {
    if !text.contains('\t') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + TAB_WIDTH * 4);
    let mut col = 0usize;
    for ch in text.chars() {
        if ch == '\t' {
            let n = TAB_WIDTH - (col % TAB_WIDTH);
            out.extend(std::iter::repeat(' ').take(n));
            col += n;
        } else {
            out.push(ch);
            col += 1;
        }
    }
    out
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
    // The numeric hunk coordinates aren't read yet: the UI renders the raw
    // header and per-line numbers instead. M3 annotation anchoring reads them.
    #[allow(dead_code)]
    pub old_start: u32,
    #[allow(dead_code)]
    pub old_count: u32,
    #[allow(dead_code)]
    pub new_start: u32,
    #[allow(dead_code)]
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
    let tracked_output = match baseline {
        Some(rev) => {
            let out = git(working_dir, &["diff", "--no-color", "-M", rev])?;
            if !out.status.success() {
                bail!(String::from_utf8_lossy(&out.stderr).into_owned());
            }
            out
        }
        None => {
            let out = git(working_dir, &["diff", "--no-color", "-M", "HEAD"])?;
            if out.status.success() {
                out
            } else {
                // Likely an unborn HEAD (no commits yet); fall back to diffing
                // the index/worktree against nothing in particular.
                let fallback = git(working_dir, &["diff", "--no-color", "-M"])?;
                if !fallback.status.success() {
                    bail!(String::from_utf8_lossy(&fallback.stderr).into_owned());
                }
                fallback
            }
        }
    };

    let tracked_text = String::from_utf8_lossy(&tracked_output.stdout).into_owned();
    let mut files = parse_unified(&tracked_text)?;

    let ls_files_output = git(
        working_dir,
        &["ls-files", "--others", "--exclude-standard"],
    )?;
    if ls_files_output.status.success() {
        let listing = String::from_utf8_lossy(&ls_files_output.stdout).into_owned();
        for path in listing.lines().filter(|l| !l.is_empty()) {
            let out = match git(
                working_dir,
                &["diff", "--no-color", "--no-index", "--", "/dev/null", path],
            ) {
                Ok(out) => out,
                Err(_) => continue,
            };
            // `--no-index` exits 1 when a diff was produced (the expected
            // case here); anything else (missing file, permission error)
            // means we should just skip this one file rather than fail the
            // whole load.
            let code = out.status.code();
            if code != Some(1) && code != Some(0) {
                continue;
            }
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            let parsed = match parse_unified(&text) {
                Ok(parsed) => parsed,
                Err(_) => continue,
            };
            for mut fd in parsed {
                fd.status = FileStatus::Untracked;
                files.push(fd);
            }
        }
    }

    Ok(DiffModel { files })
}

/// Source view refuses to load a file larger than this: reading, copying
/// every line, and syntax-highlighting the whole thing runs synchronously on
/// the render thread (this is a single-threaded TUI event loop, not an async
/// one), so an unbounded read of a large generated or minified file — cheap
/// to review as a small DIFF — would freeze the pane for however long that
/// takes. 2 MiB comfortably covers real source files while still catching
/// the generated/minified/vendored case the diff itself never has to pay for.
const MAX_SOURCE_BYTES: u64 = 2 * 1024 * 1024;

/// Source view also refuses a file with more rows than this, independent of
/// the byte cap above: a file well under 2 MiB can still hold tens of
/// thousands of very short lines. The ratatui `Paragraph::scroll` the pane
/// renders through takes a `u16` row offset, so a source view with more than
/// `u16::MAX` rows could need a scroll position that silently wraps when
/// narrowed to `u16` — the pane would then render unrelated earlier rows
/// while the cursor and footer, which track the real `usize` position,
/// still report the true (later) line. 50,000 leaves a wide margin under
/// `u16::MAX` (65,535) for the extra display rows woven-in annotation
/// comments add on top of the base row count.
const MAX_SOURCE_LINES: usize = 50_000;

/// Read a file's post-change contents as lines, for the UI's source view.
///
/// No git plumbing needed regardless of baseline: the worktree IS the new
/// side of the diff, so the file on disk is exactly what the annotations'
/// `Side::New` line numbers refer to. Binary files (`read_to_string` rejects
/// non-UTF-8), deleted files, and unreadable ones come back as `Err`; the UI
/// turns that into a placeholder row rather than failing the review.
///
/// Symlinks are refused rather than followed: git's diff for a symlink shows
/// the link's target STRING as the file's one-line content, but
/// `read_to_string` would dereference the link and render whatever it points
/// at instead — including files well outside the repo (`/etc/passwd`, an SSH
/// key) if a reviewed change plants a symlink pointing there. Checking
/// `symlink_metadata` (which does not follow the link) before ever opening
/// the target keeps that content out of the pane.
///
/// Files over `MAX_SOURCE_BYTES` are refused for the same reason binary
/// files are: there is nothing useful — or safe to the pane's
/// responsiveness — to render, so the UI shows a placeholder instead of
/// blocking on the read and the highlight pass that follows it. Files over
/// `MAX_SOURCE_LINES` are refused for the separate reason documented there.
///
/// The symlink and size checks above run against the PATH
/// (`symlink_metadata`), then the actual read opens that same path again —
/// two separate filesystem operations with a gap between them. Something
/// with write access to the reviewed worktree (the review pane's own
/// threat model: a reviewed CHANGE, not a trusted actor) could swap the
/// path for a symlink, or grow the file past the cap, in that gap, and
/// have the read follow whatever it now finds. Closing that requires
/// re-validating the OPENED FILE, not the path a second time: after
/// opening, `same_file` compares the handle's own device/inode against
/// what the pre-open check inspected — this is the one thing a
/// racing path-swap cannot fake, since a symlink swap produces a
/// different underlying file — and the read itself is hard-capped via
/// `Read::take` regardless of what any stat reported.
pub fn load_source(working_dir: &str, path: &str) -> Result<Vec<String>> {
    let full = Path::new(working_dir).join(path);
    let pre = std::fs::symlink_metadata(&full)
        .with_context(|| format!("reading {}", full.display()))?;
    if pre.file_type().is_symlink() {
        bail!("{} is a symlink; source view does not follow worktree symlinks", full.display());
    }
    if pre.len() > MAX_SOURCE_BYTES {
        bail!(
            "{} is {} bytes, over the {}-byte source view limit",
            full.display(),
            pre.len(),
            MAX_SOURCE_BYTES
        );
    }

    let file = std::fs::File::open(&full)
        .with_context(|| format!("reading {}", full.display()))?;
    let post = file.metadata().with_context(|| format!("reading {}", full.display()))?;
    #[cfg(unix)]
    if !same_file(&pre, &post) {
        bail!("{} changed while opening it; refusing a racy read", full.display());
    }
    if post.len() > MAX_SOURCE_BYTES {
        bail!(
            "{} is {} bytes, over the {}-byte source view limit",
            full.display(),
            post.len(),
            MAX_SOURCE_BYTES
        );
    }

    // However large `post.len()` claims to be, never actually read more
    // than the cap allows — a size that changes again after this last
    // stat is caught here rather than trusted.
    let mut buf = String::new();
    file.take(MAX_SOURCE_BYTES + 1)
        .read_to_string(&mut buf)
        .with_context(|| format!("reading {}", full.display()))?;
    if buf.len() as u64 > MAX_SOURCE_BYTES {
        bail!("{} grew past the {}-byte source view limit while reading", full.display(), MAX_SOURCE_BYTES);
    }

    let lines: Vec<String> = buf.lines().map(expand_tabs).collect();
    if lines.len() > MAX_SOURCE_LINES {
        bail!(
            "{} has {} lines, over the {}-line source view limit",
            full.display(),
            lines.len(),
            MAX_SOURCE_LINES
        );
    }
    Ok(lines)
}

/// Whether two `Metadata` values describe the SAME underlying file (same
/// device, same inode) rather than merely files that happen to look alike.
/// The one check a racing symlink-swap or file-replace cannot fake: even a
/// same-sized, same-permissions replacement file gets a fresh inode.
#[cfg(unix)]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

/// Parse `git diff --no-color` unified output into file diffs.
///
/// Limitation: quoted paths (git wraps a path in double quotes when it
/// contains spaces or other "unusual" characters) are unquoted by stripping
/// the surrounding `"` only; full C-style backslash-escape decoding (as git
/// applies to paths with tabs, newlines, or non-ASCII bytes) is not
/// implemented.
pub fn parse_unified(text: &str) -> Result<Vec<FileDiff>> {
    let mut files = Vec::new();
    if text.trim().is_empty() {
        return Ok(files);
    }

    // Split into per-file sections at `diff --git ` lines, keeping that
    // line as part of each section so the header parsing below can see it
    // if ever needed.
    let mut sections: Vec<Vec<&str>> = Vec::new();
    for line in text.lines() {
        if line.starts_with("diff --git ") {
            sections.push(Vec::new());
        }
        if let Some(last) = sections.last_mut() {
            last.push(line);
        }
        // Lines before the first `diff --git ` (shouldn't normally happen)
        // are silently dropped.
    }

    for section in sections {
        if let Some(fd) = parse_file_section(&section) {
            files.push(fd);
        }
    }

    Ok(files)
}

fn strip_path_prefix(raw: &str) -> String {
    let raw = raw.trim();
    let unquoted = if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
        &raw[1..raw.len() - 1]
    } else {
        raw
    };
    unquoted
        .strip_prefix("a/")
        .or_else(|| unquoted.strip_prefix("b/"))
        .unwrap_or(unquoted)
        .to_string()
}

/// Best-effort extraction of the old/new paths from a `diff --git a/X b/Y`
/// header line. This is the only path source for binary sections, which
/// carry no `---`/`+++` lines. Quoted tokens (`"a/has space"`) are parsed
/// exactly; unquoted tokens are ambiguous in general (a bare space
/// separates "a/X" from "b/Y", but X or Y may themselves contain spaces),
/// so for the common non-rename case (old path == new path) this scans for
/// a split point where both halves match, which correctly handles paths
/// containing spaces as git actually emits them. A malformed or
/// pathological line degrades to a naive split at the first " b/".
fn parse_diff_git_line(line: &str) -> (Option<String>, Option<String>) {
    let rest = match line.strip_prefix("diff --git ") {
        Some(r) => r,
        None => return (None, None),
    };
    let rest = rest.trim_end_matches('\t');

    if let Some(after_quote) = rest.strip_prefix('"') {
        let Some(end_a) = after_quote.find('"') else {
            return (None, None);
        };
        let a_tok = &rest[..end_a + 2]; // includes both quote chars
        let remainder = rest[end_a + 2..].trim_start();
        let b_tok = if let Some(after_quote_b) = remainder.strip_prefix('"') {
            match after_quote_b.find('"') {
                Some(end_b) => &remainder[..end_b + 2],
                None => remainder,
            }
        } else {
            remainder
        };
        return (
            Some(strip_path_prefix(a_tok)),
            Some(strip_path_prefix(b_tok)),
        );
    }

    // Unquoted: for the common non-rename case old == new, so scan for a
    // " b/" split point where both halves (after stripping the a/ / b/
    // prefixes) are identical.
    let mut search_from = 0;
    while let Some(rel_idx) = rest[search_from..].find(" b/") {
        let idx = search_from + rel_idx;
        let left = &rest[..idx];
        let right = &rest[idx + 1..]; // starts at "b/..."
        let left_body = left.strip_prefix("a/").unwrap_or(left);
        let right_body = right.strip_prefix("b/").unwrap_or(right);
        if left_body == right_body {
            return (Some(left_body.to_string()), Some(right_body.to_string()));
        }
        search_from = idx + 3;
        if search_from > rest.len() {
            break;
        }
    }

    // Fall back to a naive split at the first " b/" (covers renames not
    // accompanied by explicit rename headers, which git does not actually
    // produce, but keeps this permissive rather than failing outright).
    if let Some(idx) = rest.find(" b/") {
        let left = rest[..idx].strip_prefix("a/").unwrap_or(&rest[..idx]);
        let right = rest[idx + 1..]
            .strip_prefix("b/")
            .unwrap_or(&rest[idx + 1..]);
        return (Some(left.to_string()), Some(right.to_string()));
    }

    (None, None)
}

fn parse_file_section(lines: &[&str]) -> Option<FileDiff> {
    let mut status = FileStatus::Modified;
    let mut old_path: Option<String> = None;
    let mut new_path: Option<String> = None;
    let mut rename_from: Option<String> = None;
    let mut rename_to: Option<String> = None;
    let mut binary = false;
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut adds: u32 = 0;
    let mut dels: u32 = 0;

    // Binary sections carry no `---`/`+++` lines at all, so the `diff --git
    // a/X b/Y` line is the only path source available for them; parse it
    // up front as a fallback (lower priority than `---`/`+++`/rename
    // headers, which are more precise when present).
    let (fallback_old, fallback_new) = match lines.first() {
        Some(first) if first.starts_with("diff --git ") => parse_diff_git_line(first),
        _ => (None, None),
    };

    let mut i = 0;
    // Walk extended header lines (and binary markers) until we hit the
    // `---`/`+++` pair or the first hunk.
    while i < lines.len() {
        let line = lines[i];
        if line.starts_with("@@ ") || line.starts_with("@@\t") {
            break;
        }
        if line.starts_with("--- ") {
            let rest = &line[4..];
            if rest.trim() != "/dev/null" {
                old_path = Some(strip_path_prefix(rest));
            }
            i += 1;
            continue;
        }
        if line.starts_with("+++ ") {
            let rest = &line[4..];
            if rest.trim() != "/dev/null" {
                new_path = Some(strip_path_prefix(rest));
            }
            i += 1;
            continue;
        }
        if line.starts_with("new file mode") {
            status = FileStatus::Added;
        } else if line.starts_with("deleted file mode") {
            status = FileStatus::Deleted;
        } else if let Some(rest) = line.strip_prefix("rename from ") {
            rename_from = Some(strip_path_prefix(rest));
            status = FileStatus::Renamed;
        } else if let Some(rest) = line.strip_prefix("rename to ") {
            rename_to = Some(strip_path_prefix(rest));
            status = FileStatus::Renamed;
        } else if line.starts_with("similarity index")
            || line.starts_with("index ")
            || line.starts_with("old mode")
            || line.starts_with("new mode")
            || line.starts_with("diff --git ")
        {
            // Recognized-but-ignored headers.
        } else if line.starts_with("Binary files ") && line.ends_with(" differ") {
            binary = true;
        } else if line.starts_with("GIT binary patch") {
            binary = true;
        }
        // Any other unknown header line inside a file section is skipped
        // permissively, per spec.
        i += 1;
    }

    // Parse hunks, if any (binary sections have none).
    while i < lines.len() {
        let line = lines[i];
        if !line.starts_with("@@") {
            i += 1;
            continue;
        }
        let (hunk, consumed, hunk_adds, hunk_dels) = match parse_hunk(&lines[i..]) {
            Some(v) => v,
            None => {
                i += 1;
                continue;
            }
        };
        adds += hunk_adds;
        dels += hunk_dels;
        hunks.push(hunk);
        i += consumed;
    }

    // Prefer the rename-specific paths when this is a rename; otherwise
    // derive path from +++ b/... , falling back to --- a/... for deletions,
    // and finally to the `diff --git` line's paths for binary sections
    // (which have neither --- nor +++).
    let path = if status == FileStatus::Renamed {
        rename_to
            .or_else(|| new_path.clone())
            .or_else(|| fallback_new.clone())
    } else {
        new_path
            .clone()
            .or_else(|| old_path.clone())
            .or_else(|| fallback_new.clone())
            .or_else(|| fallback_old.clone())
    };
    let path = path?;

    let old_path_final = if status == FileStatus::Renamed {
        rename_from.or(old_path).or_else(|| fallback_old.clone())
    } else {
        None
    };

    Some(FileDiff {
        path,
        old_path: old_path_final,
        status,
        binary,
        hunks,
        adds,
        dels,
    })
}

/// Parse a single hunk starting at `lines[0]` (a `@@ ... @@` header).
/// Returns the hunk, the number of lines consumed (including the header),
/// and the (adds, dels) counts contributed by this hunk.
fn parse_hunk(lines: &[&str]) -> Option<(Hunk, usize, u32, u32)> {
    let header = lines[0];
    let (old_start, old_count, new_start, new_count) = parse_hunk_header(header)?;

    let mut old_no = old_start;
    let mut new_no = new_start;
    let mut body: Vec<DiffLine> = Vec::new();
    let mut adds = 0u32;
    let mut dels = 0u32;

    let mut consumed = 1usize;
    for &line in &lines[1..] {
        if line.starts_with("@@") || line.starts_with("diff --git ") {
            break;
        }
        if line == "\\ No newline at end of file" {
            consumed += 1;
            continue;
        }
        let (origin, content) = if let Some(rest) = line.strip_prefix(' ') {
            (Origin::Context, rest)
        } else if let Some(rest) = line.strip_prefix('+') {
            (Origin::Add, rest)
        } else if let Some(rest) = line.strip_prefix('-') {
            (Origin::Remove, rest)
        } else if line.is_empty() {
            // A blank line in the body is a context line with empty content
            // (git emits a bare space prefix, but trailing-whitespace
            // stripping upstream can leave a truly empty line).
            (Origin::Context, line)
        } else {
            // Unrecognized body line; stop consuming this hunk's body here
            // and let the outer loop resume scanning from this line.
            break;
        };
        let content = expand_tabs(content);

        match origin {
            Origin::Context => {
                body.push(DiffLine {
                    origin,
                    old_no: Some(old_no),
                    new_no: Some(new_no),
                    content: content.clone(),
                });
                old_no += 1;
                new_no += 1;
            }
            Origin::Remove => {
                body.push(DiffLine {
                    origin,
                    old_no: Some(old_no),
                    new_no: None,
                    content: content.clone(),
                });
                old_no += 1;
                dels += 1;
            }
            Origin::Add => {
                body.push(DiffLine {
                    origin,
                    old_no: None,
                    new_no: Some(new_no),
                    content: content.clone(),
                });
                new_no += 1;
                adds += 1;
            }
        }
        consumed += 1;
    }

    Some((
        Hunk {
            header: expand_tabs(header),
            old_start,
            old_count,
            new_start,
            new_count,
            lines: body,
        },
        consumed,
        adds,
        dels,
    ))
}

/// Parse `@@ -<old_start>[,<old_count>] +<new_start>[,<new_count>] @@[ context]`.
fn parse_hunk_header(header: &str) -> Option<(u32, u32, u32, u32)> {
    let rest = header.strip_prefix("@@ ")?;
    let end = rest.find(" @@")?;
    let ranges = &rest[..end];
    let mut parts = ranges.split_whitespace();
    let old_range = parts.next()?.strip_prefix('-')?;
    let new_range = parts.next()?.strip_prefix('+')?;

    let (old_start, old_count) = parse_range(old_range)?;
    let (new_start, new_count) = parse_range(new_range)?;
    Some((old_start, old_count, new_start, new_count))
}

fn parse_range(range: &str) -> Option<(u32, u32)> {
    if let Some((start, count)) = range.split_once(',') {
        Some((start.parse().ok()?, count.parse().ok()?))
    } else {
        Some((range.parse().ok()?, 1))
    }
}

fn git(working_dir: &str, args: &[&str]) -> Result<std::process::Output> {
    Command::new("git")
        .arg("-C")
        .arg(working_dir)
        .args(args)
        .output()
        .context("running git")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_tabs_aligns_to_tab_stops() {
        assert_eq!(expand_tabs("no tabs"), "no tabs");
        assert_eq!(expand_tabs("\tx"), "    x");
        assert_eq!(expand_tabs("\t\tx"), "        x");
        assert_eq!(expand_tabs("ab\tx"), "ab  x");
        assert_eq!(expand_tabs("abcd\tx"), "abcd    x");
        assert_eq!(expand_tabs("a\tb\tc"), "a   b   c");
    }

    #[test]
    fn parse_expands_tabs_in_body_and_hunk_header() {
        let raw = "diff --git a/main.go b/main.go\n--- a/main.go\n+++ b/main.go\n@@ -1,2 +1,2 @@ func\tmain() {\n \tif x {\n-\t\told()\n+\t\tnew()\n \t}\n";
        let files = parse_unified(raw).unwrap();
        let hunk = &files[0].hunks[0];
        assert_eq!(hunk.header, "@@ -1,2 +1,2 @@ func    main() {");
        let contents: Vec<&str> = hunk.lines.iter().map(|l| l.content.as_str()).collect();
        assert_eq!(contents, vec!["    if x {", "        old()", "        new()", "    }"]);
        assert!(contents.iter().all(|c| !c.contains('\t')));
    }

    #[test]
    fn modified_file_two_hunks() {
        let text = "\
diff --git a/src/lib.rs b/src/lib.rs
index 1111111..2222222 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,4 +1,5 @@
 fn one() {}
+fn inserted() {}
 fn two() {}
-fn three() {}
+fn three_renamed() {}
 fn four() {}
@@ -10,3 +11,3 @@
 fn ten() {}
-fn eleven() {}
+fn eleven_v2() {}
 fn twelve() {}
";
        let files = parse_unified(text).unwrap();
        assert_eq!(files.len(), 1);
        let fd = &files[0];
        assert_eq!(fd.path, "src/lib.rs");
        assert_eq!(fd.old_path, None);
        assert_eq!(fd.status, FileStatus::Modified);
        assert!(!fd.binary);
        assert_eq!(fd.hunks.len(), 2);

        let h0 = &fd.hunks[0];
        assert_eq!(h0.header, "@@ -1,4 +1,5 @@");
        assert_eq!(h0.old_start, 1);
        assert_eq!(h0.old_count, 4);
        assert_eq!(h0.new_start, 1);
        assert_eq!(h0.new_count, 5);
        assert_eq!(h0.lines.len(), 6);

        let expected0: Vec<(Origin, Option<u32>, Option<u32>, &str)> = vec![
            (Origin::Context, Some(1), Some(1), "fn one() {}"),
            (Origin::Add, None, Some(2), "fn inserted() {}"),
            (Origin::Context, Some(2), Some(3), "fn two() {}"),
            (Origin::Remove, Some(3), None, "fn three() {}"),
            (Origin::Add, None, Some(4), "fn three_renamed() {}"),
            (Origin::Context, Some(4), Some(5), "fn four() {}"),
        ];
        for (line, exp) in h0.lines.iter().zip(expected0.iter()) {
            assert_eq!(line.origin, exp.0);
            assert_eq!(line.old_no, exp.1);
            assert_eq!(line.new_no, exp.2);
            assert_eq!(line.content, exp.3);
        }

        let h1 = &fd.hunks[1];
        assert_eq!(h1.header, "@@ -10,3 +11,3 @@");
        assert_eq!(h1.old_start, 10);
        assert_eq!(h1.old_count, 3);
        assert_eq!(h1.new_start, 11);
        assert_eq!(h1.new_count, 3);

        let expected1: Vec<(Origin, Option<u32>, Option<u32>, &str)> = vec![
            (Origin::Context, Some(10), Some(11), "fn ten() {}"),
            (Origin::Remove, Some(11), None, "fn eleven() {}"),
            (Origin::Add, None, Some(12), "fn eleven_v2() {}"),
            (Origin::Context, Some(12), Some(13), "fn twelve() {}"),
        ];
        for (line, exp) in h1.lines.iter().zip(expected1.iter()) {
            assert_eq!(line.origin, exp.0);
            assert_eq!(line.old_no, exp.1);
            assert_eq!(line.new_no, exp.2);
            assert_eq!(line.content, exp.3);
        }

        // adds: inserted + three_renamed + eleven_v2 = 3
        // dels: three + eleven = 2
        assert_eq!(fd.adds, 3);
        assert_eq!(fd.dels, 2);
    }

    #[test]
    fn hunk_header_omitted_counts() {
        let text = "\
diff --git a/f.txt b/f.txt
index 1111111..2222222 100644
--- a/f.txt
+++ b/f.txt
@@ -1 +1,2 @@
 line one
+line two
";
        let files = parse_unified(text).unwrap();
        assert_eq!(files.len(), 1);
        let h = &files[0].hunks[0];
        assert_eq!(h.old_start, 1);
        assert_eq!(h.old_count, 1);
        assert_eq!(h.new_start, 1);
        assert_eq!(h.new_count, 2);
        assert_eq!(h.lines[0].old_no, Some(1));
        assert_eq!(h.lines[0].new_no, Some(1));
        assert_eq!(h.lines[1].old_no, None);
        assert_eq!(h.lines[1].new_no, Some(2));
    }

    #[test]
    fn added_and_deleted_files() {
        let text = "\
diff --git a/new.txt b/new.txt
new file mode 100644
index 0000000..1111111
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+hello
+world
diff --git a/old.txt b/old.txt
deleted file mode 100644
index 1111111..0000000
--- a/old.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-bye
-now
";
        let files = parse_unified(text).unwrap();
        assert_eq!(files.len(), 2);

        let added = &files[0];
        assert_eq!(added.path, "new.txt");
        assert_eq!(added.status, FileStatus::Added);
        assert_eq!(added.adds, 2);
        assert_eq!(added.dels, 0);

        let deleted = &files[1];
        assert_eq!(deleted.path, "old.txt");
        assert_eq!(deleted.status, FileStatus::Deleted);
        assert_eq!(deleted.adds, 0);
        assert_eq!(deleted.dels, 2);
    }

    #[test]
    fn rename_with_from_to() {
        let text = "\
diff --git a/old_name.rs b/new_name.rs
similarity index 95%
rename from old_name.rs
rename to new_name.rs
index 1111111..2222222 100644
--- a/old_name.rs
+++ b/new_name.rs
@@ -1,1 +1,1 @@
-fn old() {}
+fn new() {}
";
        let files = parse_unified(text).unwrap();
        assert_eq!(files.len(), 1);
        let fd = &files[0];
        assert_eq!(fd.status, FileStatus::Renamed);
        assert_eq!(fd.old_path, Some("old_name.rs".to_string()));
        assert_eq!(fd.path, "new_name.rs");
        assert_eq!(fd.adds, 1);
        assert_eq!(fd.dels, 1);
    }

    #[test]
    fn binary_file_section() {
        let text = "\
diff --git a/image.png b/image.png
index 1111111..2222222 100644
Binary files a/image.png and b/image.png differ
";
        let files = parse_unified(text).unwrap();
        assert_eq!(files.len(), 1);
        let fd = &files[0];
        assert!(fd.binary);
        assert!(fd.hunks.is_empty());
        assert_eq!(fd.path, "image.png");
    }

    #[test]
    fn no_newline_at_eof_skipped() {
        let text = "\
diff --git a/f.txt b/f.txt
index 1111111..2222222 100644
--- a/f.txt
+++ b/f.txt
@@ -1,1 +1,1 @@
-old content
\\ No newline at end of file
+new content
\\ No newline at end of file
";
        let files = parse_unified(text).unwrap();
        let h = &files[0].hunks[0];
        assert_eq!(h.lines.len(), 2);
        assert_eq!(h.lines[0].origin, Origin::Remove);
        assert_eq!(h.lines[0].old_no, Some(1));
        assert_eq!(h.lines[0].new_no, None);
        assert_eq!(h.lines[0].content, "old content");
        assert_eq!(h.lines[1].origin, Origin::Add);
        assert_eq!(h.lines[1].old_no, None);
        assert_eq!(h.lines[1].new_no, Some(1));
        assert_eq!(h.lines[1].content, "new content");
    }

    #[test]
    fn quoted_path_with_space() {
        let text = "\
diff --git \"a/has space.txt\" \"b/has space.txt\"
index 1111111..2222222 100644
--- \"a/has space.txt\"
+++ \"b/has space.txt\"
@@ -1,1 +1,1 @@
-old
+new
";
        let files = parse_unified(text).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "has space.txt");
    }

    #[test]
    fn empty_input_returns_empty_vec() {
        let files = parse_unified("").unwrap();
        assert!(files.is_empty());
    }

    // --- load() integration test -------------------------------------

    struct TempRepo {
        path: std::path::PathBuf,
    }

    impl TempRepo {
        fn new(name: &str) -> Self {
            let mut path = std::env::temp_dir();
            path.push(format!("herdr_diff_test_{}_{}", name, std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create temp repo dir");
            let repo = TempRepo { path };
            repo.git(&["init", "-q"]);
            repo.git(&["config", "user.email", "test@example.com"]);
            repo.git(&["config", "user.name", "Test"]);
            repo
        }

        fn git(&self, args: &[&str]) -> std::process::Output {
            Command::new("git")
                .arg("-C")
                .arg(&self.path)
                .args(args)
                .output()
                .expect("running git in temp repo")
        }

        fn write(&self, rel: &str, contents: &str) {
            std::fs::write(self.path.join(rel), contents).expect("write file");
        }

        fn commit_all(&self, message: &str) {
            self.git(&["add", "-A"]);
            self.git(&[
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=Test",
                "commit",
                "-q",
                "-m",
                message,
            ]);
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn load_integration_modified_and_untracked() {
        let repo = TempRepo::new("load");

        repo.write("tracked.txt", "line one\nline two\nline three\n");
        repo.commit_all("initial commit");

        // Modify the tracked file.
        repo.write("tracked.txt", "line one\nline TWO changed\nline three\n");

        // Add an untracked file.
        repo.write("new_file.txt", "brand new content\n");

        let wd = repo.path.to_str().expect("utf8 path").to_string();
        let model = load(&wd, None).expect("load should succeed");

        let modified: Vec<&FileDiff> = model
            .files
            .iter()
            .filter(|f| f.status == FileStatus::Modified)
            .collect();
        assert_eq!(modified.len(), 1, "expected exactly one modified file");
        let m = modified[0];
        assert_eq!(m.path, "tracked.txt");
        assert!(!m.binary);
        assert!(m.hunks.iter().any(|h| h
            .lines
            .iter()
            .any(|l| l.origin == Origin::Remove && l.content == "line two")));
        assert!(m.hunks.iter().any(|h| h
            .lines
            .iter()
            .any(|l| l.origin == Origin::Add && l.content == "line TWO changed")));

        let untracked: Vec<&FileDiff> = model
            .files
            .iter()
            .filter(|f| f.status == FileStatus::Untracked)
            .collect();
        assert_eq!(untracked.len(), 1, "expected exactly one untracked file");
        let u = untracked[0];
        assert_eq!(u.path, "new_file.txt");
        assert!(!u.hunks.is_empty());
        for h in &u.hunks {
            for line in &h.lines {
                assert_eq!(line.origin, Origin::Add);
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn same_file_detects_a_different_underlying_file_even_with_matching_size() {
        // load_source's symlink/size checks stat the PATH, then the read
        // opens that same path again — a gap a racing path-swap could
        // exploit (replace the path with a symlink, or a bigger file,
        // between the check and the open). same_file is the guard that
        // closes it: comparing the OPENED handle's own device/inode against
        // what the pre-open check inspected, which a swap — even to a
        // same-sized replacement — cannot fake.
        let repo = TempRepo::new("same_file_check");
        repo.write("a.txt", "hello");
        repo.write("b.txt", "hello"); // same size, different underlying file

        let meta_a = std::fs::symlink_metadata(repo.path.join("a.txt")).expect("stat a");
        let meta_a_again = std::fs::symlink_metadata(repo.path.join("a.txt")).expect("stat a again");
        let meta_b = std::fs::symlink_metadata(repo.path.join("b.txt")).expect("stat b");

        assert!(same_file(&meta_a, &meta_a_again), "the same path stat'd twice must match");
        assert!(!same_file(&meta_a, &meta_b), "two different files must not match, even same-sized");
    }

    #[test]
    fn load_source_refuses_a_symlink_instead_of_following_it_outside_the_repo() {
        // read_to_string follows symlinks. A reviewed change that plants a
        // symlink pointing outside the repo — a secret file, an SSH key —
        // must not have its target's content rendered in source view just
        // because the reviewer pressed `t`.
        let repo = TempRepo::new("symlink_source");

        let secret_dir = std::env::temp_dir()
            .join(format!("herdr_diff_test_secret_{}", std::process::id()));
        std::fs::create_dir_all(&secret_dir).expect("create secret dir");
        let secret_path = secret_dir.join("secret.txt");
        std::fs::write(&secret_path, "TOP SECRET, outside the repo\n").expect("write secret");

        #[cfg(unix)]
        std::os::unix::fs::symlink(&secret_path, repo.path.join("link.txt"))
            .expect("create symlink");

        let wd = repo.path.to_str().expect("utf8 path").to_string();
        let result = load_source(&wd, "link.txt");
        assert!(result.is_err(), "a symlink must be refused, not dereferenced: {result:?}");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("symlink"), "error should name the reason, got {msg:?}");

        let _ = std::fs::remove_dir_all(&secret_dir);
    }

    #[test]
    fn load_source_refuses_a_file_over_the_size_limit() {
        // Source view must not read, copy, and syntax-highlight a file of
        // unbounded size synchronously on the render thread. A
        // generated/minified file the diff itself never had to pay for
        // would freeze the pane just because the reviewer pressed `t`.
        let repo = TempRepo::new("oversized_source");
        let oversized = "x".repeat((MAX_SOURCE_BYTES + 1) as usize);
        repo.write("huge.txt", &oversized);

        let wd = repo.path.to_str().expect("utf8 path").to_string();
        let result = load_source(&wd, "huge.txt");
        assert!(result.is_err(), "an oversized file must be refused: {result:?}");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("limit"), "error should name the reason, got {msg:?}");

        // A file right at the limit still loads normally — this guards the
        // limit itself, not files that merely happen to be sizeable.
        let at_limit = "y".repeat(MAX_SOURCE_BYTES as usize);
        repo.write("at_limit.txt", &at_limit);
        assert!(load_source(&wd, "at_limit.txt").is_ok(), "a file at the limit must still load");
    }

    #[test]
    fn load_source_refuses_a_file_with_too_many_lines() {
        // ratatui's `Paragraph::scroll` takes a `u16` row offset. A file
        // well under the byte cap can still hold tens of thousands of very
        // short lines; scrolling into one would need an offset that
        // silently wraps when narrowed from `usize` to `u16`, rendering
        // unrelated earlier rows while the cursor/footer still report the
        // true (later, wrapped-away) line. Bytes alone don't catch this, so
        // the line count needs its own, separate cap.
        let repo = TempRepo::new("too_many_lines");
        // Short lines so this is nowhere near MAX_SOURCE_BYTES — this test
        // isolates the LINE cap, not the byte one.
        let many_lines = "x\n".repeat(MAX_SOURCE_LINES + 1);
        assert!(
            (many_lines.len() as u64) < MAX_SOURCE_BYTES,
            "test setup must stay under the byte cap to isolate the line cap"
        );
        repo.write("many_lines.txt", &many_lines);

        let wd = repo.path.to_str().expect("utf8 path").to_string();
        let result = load_source(&wd, "many_lines.txt");
        assert!(result.is_err(), "a file with too many lines must be refused: {result:?}");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("lines"), "error should name the reason, got {msg:?}");

        // A file at exactly the limit still loads.
        let at_limit = "x\n".repeat(MAX_SOURCE_LINES);
        repo.write("at_limit_lines.txt", &at_limit);
        assert!(
            load_source(&wd, "at_limit_lines.txt").is_ok(),
            "a file at the line limit must still load"
        );
    }
}
