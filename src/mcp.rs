//! MCP stdio server exposing the review tools.
//!
//! Deliberately hand-rolled: the server speaks newline-delimited JSON-RPC 2.0 on
//! stdin/stdout (the MCP stdio transport) and implements only what this server
//! needs — initialize, tools/list, tools/call, ping. Logs go to stderr; stdout
//! carries protocol frames only.
//!
//! Two review modes share one handoff socket setup (`prepare_handoff`):
//!
//! - `review_changes` is blocking: it opens the review pane and does not
//!   return until the human delivers a verdict (or the pane dies, which maps
//!   to a Cancelled result).
//! - `show_changes` / `goto` / `collect_review` are the non-blocking
//!   "guided review" trio: `show_changes` opens the pane and returns
//!   immediately, `goto` pushes navigation to it while the agent narrates in
//!   chat, `focus` folds a long file down to the regions under discussion,
//!   and `collect_review` polls for the verdict whenever the agent is ready
//!   to check.
//!
//! At most one review — blocking or non-blocking — is in flight at a time;
//! that's tracked by the `active: Option<ActiveReview>` state threaded
//! through the stdin loop.

use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::config::{self, Config};
use crate::herdr;
use crate::protocol::{
    GotoTarget, Handoff, LineRange, OpenReview, ReviewRequest, ReviewResult, Verdict,
    VerdictNotify, PROTOCOL_VERSION,
};

const REVIEW_CHANGES: &str = "review_changes";
const SHOW_CHANGES: &str = "show_changes";
const GOTO: &str = "goto";
const FOCUS: &str = "focus";
const COLLECT_REVIEW: &str = "collect_review";

/// A non-blocking review in flight: the open handoff (write half for
/// navigation, mailbox for the verdict), plus enough bookkeeping to answer
/// `show_changes`'s own return value and `collect_review`'s pending status.
struct ActiveReview {
    open: OpenReview,
    working_dir: String,
    opened_at: Instant,
    /// True while a `collect_review` call is checking or waiting for the
    /// verdict. The mailbox thread's nudge callback reads it: an agent
    /// already polling gets the verdict as that call's return value, so
    /// typing a "call collect_review" prompt at it too would be noise.
    collector_waiting: Arc<AtomicBool>,
}

pub fn run() -> Result<()> {
    let config = config::load();
    let stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    let mut active: Option<ActiveReview> = None;
    eprintln!("herdr-annotator mcp: ready (pid {})", std::process::id());

    for line in stdin.lines() {
        let line = line.context("reading stdin")?;
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(err) => {
                eprintln!("herdr-annotator mcp: dropping malformed frame: {err}");
                continue;
            }
        };
        let id = msg.get("id").filter(|v| !v.is_null()).cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or_default();
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        let response = match (method, id) {
            ("initialize", Some(id)) => Some(result_frame(id, initialize_result(&params))),
            ("ping", Some(id)) => Some(result_frame(id, json!({}))),
            ("tools/list", Some(id)) => {
                Some(result_frame(id, json!({ "tools": tool_descriptors(&config) })))
            }
            ("tools/call", Some(id)) => {
                Some(result_frame(id, handle_tool_call(&params, &config, &mut active)))
            }
            (_, Some(id)) => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("method not found: {method}") },
            })),
            // Notifications (initialized, cancelled, …) need no reply.
            (_, None) => None,
        };
        if let Some(frame) = response {
            let mut out = serde_json::to_string(&frame)?;
            out.push('\n');
            stdout.write_all(out.as_bytes())?;
            stdout.flush()?;
        }
    }
    Ok(())
}

fn result_frame(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn initialize_result(params: &Value) -> Value {
    // Echo the client's protocol revision; every revision we rely on predates all of them.
    let protocol_version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or("2025-06-18");
    json!({
        "protocolVersion": protocol_version,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "herdr-annotator",
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

/// Input schema shared by `review_changes` and `show_changes`: they open the
/// same kind of review, one blocking and one not.
fn review_input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "baseline": {
                "type": "string",
                "description": "Git rev to diff against (e.g. a commit hash captured before you started). Omit for all uncommitted changes vs HEAD."
            },
            "note": {
                "type": "string",
                "description": "Short message shown to the reviewer, e.g. what to focus on."
            },
            "working_dir": {
                "type": "string",
                "description": "Absolute path of the git repository to review. Defaults to the server's working directory."
            }
        },
        "required": []
    })
}

/// Takes the config because show_changes' description promises the verdict
/// nudge — a promise that must disappear when `notify_on_verdict = false`,
/// or the agent waits for a prompt that never comes.
fn tool_descriptors(config: &Config) -> Vec<Value> {
    let nudge_promise = if config.notify_on_verdict {
        " If the reviewer finishes while you are not polling, the server sends a short '[herdr-annotator] The reviewer finished…' prompt into your chat — when you see it, call collect_review and act on the feedback."
    } else {
        ""
    };
    vec![
        json!({
            "name": REVIEW_CHANGES,
            "description": "Open a review pane beside this agent inside herdr and BLOCK until the human reviews the diff and returns a verdict with line-anchored annotations. Call this at checkpoints (e.g. before marking a task complete). The diff shown is the working tree vs `baseline` (a git rev); omit `baseline` to show all uncommitted changes. The plugin's `review_timeout` config (if set) caps how long this call can block, returning a cancelled verdict past that point; unset means it blocks forever. Only one review may be open at a time: if a non-blocking review is already open (via show_changes), this fails — call collect_review first.",
            "inputSchema": review_input_schema(),
        }),
        json!({
            "name": SHOW_CHANGES,
            "description": format!("Open a review pane beside this agent and return immediately — for a guided walkthrough where you explain the diff in chat while the human reads it at their own pace. Typical flow: call show_changes to open the pane, call goto once per point you want to highlight as you narrate it (and focus to fold a long file down to just the regions you are discussing), then call collect_review to get the verdict and annotations once you're done (or whenever you want to check). Same arguments as review_changes: baseline, note, working_dir. Unlike review_changes this never blocks and has no timeout — nothing here is waiting on the human, so there's nothing to time out; the review stays open until the reviewer finishes in the pane or you call collect_review.{nudge_promise} Only one review may be open at a time: fails if one is already open."),
            "inputSchema": review_input_schema(),
        }),
        json!({
            "name": GOTO,
            "description": "Push navigation to the review pane opened by show_changes, so it jumps to a file and line while you explain it in chat. Call this once per point you want the pane to follow, any time between show_changes and collect_review. `file` is a repo-relative path on the new (post-change) side; `line` is a 1-based line number on that side. Navigation is advisory: an unknown file or an out-of-range line is ignored (or clamped) by the pane, never an error here. Requires an open review — call show_changes first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Repo-relative path, new/post-change side."
                    },
                    "line": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "1-based line number on the new side."
                    },
                    "view": {
                        "type": ["string", "null"],
                        "enum": ["diff", "source", null],
                        "description": "Optionally switch the pane's view before landing: \"diff\" (the changes) or \"source\" (the full post-change file — use when walking through whole files or lines outside the hunks). Omitted or null keeps the current view."
                    }
                },
                "required": ["file", "line"]
            },
        }),
        json!({
            "name": FOCUS,
            "description": "Fold the review pane's source view of one file down to just the line regions you name — for guided walkthroughs of long files where only a few parts matter. Everything between the regions collapses into '⋯ N lines folded ⋯' pills the reviewer can expand with Enter or a click; the pane switches to that file's source view and lands on the first region. Regions are 1-based inclusive {start, end} ranges on the new (post-change) side, listed in the order you'll discuss them; gaps of one or two lines between regions stay visible. Call again with new regions to refocus, or with an empty regions array to clear the agent's focus (cursor left where the reviewer had it). This does not touch folds the reviewer made by hand with f — those stay in place until the reviewer expands them or presses F. A later goto into a folded stretch expands it automatically, so you can never strand the reviewer behind a fold. Advisory like goto: unknown files and out-of-range regions are normalized or ignored by the pane, never an error here. Requires an open review — call show_changes first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Repo-relative path, new/post-change side."
                    },
                    "regions": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "start": {
                                    "type": "integer",
                                    "minimum": 1,
                                    "description": "First visible line of the region (1-based, inclusive)."
                                },
                                "end": {
                                    "type": "integer",
                                    "minimum": 1,
                                    "description": "Last visible line of the region (inclusive)."
                                }
                            },
                            "required": ["start", "end"]
                        },
                        "description": "Line regions to keep visible, in narrative order. An empty array clears the focus."
                    }
                },
                "required": ["file", "regions"]
            },
        }),
        json!({
            "name": COLLECT_REVIEW,
            "description": "Check whether the reviewer has delivered a verdict in the review pane opened by show_changes. With `wait_seconds` omitted or 0 (the default), checks once and returns immediately. With `wait_seconds` > 0 (up to 120), polls for up to that long before giving up. While no verdict has landed yet this returns `{\"status\": \"pending\", \"open_for_secs\": N}` — that is not an error, just call collect_review again later. Once a verdict lands, this clears the open review and returns the same verdict+annotations JSON review_changes returns; annotations work exactly the same way in both modes. A pane the reviewer closed without deciding also surfaces here, as a normal cancelled verdict — not a failure. Requires an open review — call show_changes first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "wait_seconds": {
                        "type": ["integer", "null"],
                        "minimum": 0,
                        "maximum": 120,
                        "default": 0,
                        "description": "How long to poll for a verdict before reporting pending. 0 = single check, no wait. Omitted or null both mean the default — some MCP clients serialize omitted optionals as null."
                    }
                },
                "required": []
            },
        }),
    ]
}

fn handle_tool_call(params: &Value, config: &Config, active: &mut Option<ActiveReview>) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    match name {
        REVIEW_CHANGES => handle_review_changes(&args, config, active),
        SHOW_CHANGES => handle_show_changes(&args, config, active),
        GOTO => handle_goto(&args, active),
        FOCUS => handle_focus(&args, active),
        COLLECT_REVIEW => handle_collect_review(&args, active),
        other => tool_error(format!("unknown tool: {other}")),
    }
}

fn handle_review_changes(args: &Value, config: &Config, active: &mut Option<ActiveReview>) -> Value {
    if active.is_some() {
        return tool_error("a non-blocking review is open — collect_review first".to_string());
    }
    match run_review(args, config) {
        Ok(result) => review_result_response(&result),
        Err(err) => tool_error(format!("{err:#}")),
    }
}

fn handle_show_changes(args: &Value, config: &Config, active: &mut Option<ActiveReview>) -> Value {
    if active.is_some() {
        return tool_error(
            "a review pane is already open — finish it there or call collect_review first".to_string(),
        );
    }
    match run_show(args, config) {
        Ok(new_active) => {
            let text = serde_json::to_string_pretty(&json!({
                "opened": true,
                "working_dir": new_active.working_dir,
            }))
            .unwrap_or_else(|e| format!("{{\"error\":\"serialize: {e}\"}}"));
            *active = Some(new_active);
            json!({ "content": [{ "type": "text", "text": text }], "isError": false })
        }
        Err(err) => tool_error(format!("{err:#}")),
    }
}

fn handle_goto(args: &Value, active: &mut Option<ActiveReview>) -> Value {
    let Some(active_review) = active.as_mut() else {
        return tool_error("no open review — call show_changes first".to_string());
    };

    let file = match args.get("file").and_then(Value::as_str) {
        Some(f) if !f.is_empty() => f.to_string(),
        _ => return tool_error("goto requires a non-empty \"file\" argument".to_string()),
    };
    let line = match args
        .get("line")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
    {
        Some(n) if n >= 1 => n,
        _ => return tool_error("goto requires \"line\" to be an integer >= 1".to_string()),
    };
    // Same lenient-null / strict-otherwise contract as collect_review's
    // wait_seconds: absent and null both mean "keep the current view".
    let view = match args.get("view") {
        None | Some(Value::Null) => None,
        Some(Value::String(v)) if v == "diff" || v == "source" => Some(v.clone()),
        _ => {
            return tool_error(
                "goto requires \"view\" to be \"diff\" or \"source\" when given".to_string(),
            )
        }
    };

    let view_note = view.as_deref().map(|v| format!(" in {v} view")).unwrap_or_default();
    match active_review.open.goto(&GotoTarget { file: file.clone(), line, view, focus: None }) {
        Ok(()) => {
            let text = format!(
                "navigated the review pane to {file}:{line}{view_note} (advisory — if the pane doesn't recognize that file, it ignores the push)"
            );
            json!({ "content": [{ "type": "text", "text": text }], "isError": false })
        }
        Err(err) => tool_error(format!(
            "could not push navigation ({err:#}) — the review pane may be gone; call collect_review to check for a verdict"
        )),
    }
}

/// The focus tool rides the same navigation channel as goto: it sends one
/// GotoTarget whose `focus` carries the regions. A non-empty focus lands on
/// the first listed region in source view; an empty one clears the focus
/// with the line-0 "don't move the cursor" sentinel and leaves the current
/// view alone.
fn handle_focus(args: &Value, active: &mut Option<ActiveReview>) -> Value {
    let Some(active_review) = active.as_mut() else {
        return tool_error("no open review — call show_changes first".to_string());
    };
    let file = match args.get("file").and_then(Value::as_str) {
        Some(f) if !f.is_empty() => f.to_string(),
        _ => return tool_error("focus requires a non-empty \"file\" argument".to_string()),
    };
    let Some(raw) = args.get("regions").and_then(Value::as_array) else {
        return tool_error(
            "focus requires \"regions\": an array of {start, end} objects (empty clears the focus)"
                .to_string(),
        );
    };
    let mut regions = Vec::with_capacity(raw.len());
    for r in raw {
        let (Some(start), Some(end)) = (
            r.get("start").and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok()),
            r.get("end").and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok()),
        ) else {
            return tool_error(
                "each focus region needs integer \"start\" and \"end\" line numbers >= 1"
                    .to_string(),
            );
        };
        if start < 1 || end < start {
            return tool_error(format!(
                "invalid focus region {start}-{end}: needs 1 <= start <= end"
            ));
        }
        regions.push(LineRange { start, end });
    }
    let (line, view) = match regions.first() {
        Some(first) => (first.start, Some("source".to_string())),
        None => (0, None),
    };
    let summary = if regions.is_empty() {
        format!(
            "cleared the focus on {file} — cursor untouched (folds the reviewer made by hand with f, if any, are unaffected)"
        )
    } else {
        let list = regions
            .iter()
            .map(|r| format!("{}-{}", r.start, r.end))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "focused {file} on lines {list} in source view — everything between folds into pills the reviewer can expand"
        )
    };
    match active_review.open.goto(&GotoTarget { file, line, view, focus: Some(regions) }) {
        Ok(()) => json!({
            "content": [{ "type": "text", "text": format!("{summary} (advisory — an unknown file is ignored by the pane)") }],
            "isError": false
        }),
        Err(err) => tool_error(format!(
            "could not push the focus ({err:#}) — the review pane may be gone; call collect_review to check for a verdict"
        )),
    }
}

fn handle_collect_review(args: &Value, active: &mut Option<ActiveReview>) -> Value {
    if active.is_none() {
        return tool_error("no open review".to_string());
    }
    // Absent or explicit null means "use the documented default" (0, single
    // check); anything else that isn't a 0..=120 integer is a malformed
    // argument, not a value to silently coerce — matching goto's strict
    // rejection of an invalid "line" rather than guessing at intent.
    let wait_seconds = match args.get("wait_seconds") {
        None | Some(Value::Null) => 0,
        Some(v) => match v.as_u64() {
            Some(n) if n <= 120 => n,
            _ => {
                return tool_error(
                    "collect_review requires \"wait_seconds\" to be an integer between 0 and 120"
                        .to_string(),
                )
            }
        },
    };
    let deadline = Instant::now() + Duration::from_secs(wait_seconds);

    // While this call is checking, a verdict that lands belongs to this
    // call's return value — flag the mailbox thread off the nudge. The flag
    // comes back down only on the pending path; once the verdict is taken
    // the review is gone and there is nothing left to nudge about.
    if let Some(a) = active.as_ref() {
        a.collector_waiting.store(true, Ordering::SeqCst);
    }

    loop {
        let taken = active.as_ref().and_then(|a| a.open.try_take());
        match taken {
            Some(Ok(result)) => {
                *active = None;
                return review_result_response(&result);
            }
            Some(Err(err)) => {
                // A broken channel, not a verdict — clear the slot (there's
                // nothing left to collect) and surface it as a real error
                // rather than silently reporting a cancelled review.
                *active = None;
                return tool_error(format!("{err:#}"));
            }
            None => {}
        }
        if wait_seconds == 0 || Instant::now() >= deadline {
            #[cfg(test)]
            tests::pending_path_test_hook();
            if let Some(a) = active.as_ref() {
                a.collector_waiting.store(false, Ordering::SeqCst);
            }
            // Look one last time *after* lowering the flag. A verdict
            // deposited since the loop's check was flagged off the nudge by
            // the raised flag; if it isn't returned here it is stranded with
            // no continuation signal at all. Rechecking after the store
            // guarantees every verdict gets exactly one of: returned by this
            // call, or nudged.
            match active.as_ref().and_then(|a| a.open.try_take()) {
                Some(Ok(result)) => {
                    *active = None;
                    return review_result_response(&result);
                }
                Some(Err(err)) => {
                    *active = None;
                    return tool_error(format!("{err:#}"));
                }
                None => {}
            }
            let open_for_secs = active
                .as_ref()
                .map(|a| a.opened_at.elapsed().as_secs())
                .unwrap_or(0);
            let text = serde_json::to_string_pretty(&json!({
                "status": "pending",
                "open_for_secs": open_for_secs,
            }))
            .unwrap_or_else(|e| format!("{{\"error\":\"serialize: {e}\"}}"));
            return json!({ "content": [{ "type": "text", "text": text }], "isError": false });
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn review_result_response(result: &ReviewResult) -> Value {
    let text = serde_json::to_string_pretty(result)
        .unwrap_or_else(|e| format!("{{\"error\":\"serialize: {e}\"}}"));
    json!({ "content": [{ "type": "text", "text": text }], "isError": false })
}

fn tool_error(message: String) -> Value {
    eprintln!("herdr-annotator mcp: tool error: {message}");
    json!({ "content": [{ "type": "text", "text": message }], "isError": true })
}

/// Result of the setup shared by `review_changes` and `show_changes`: the
/// handoff socket is bound, herdr has been asked to open the pane, and the
/// pane has connected. From here the two tools diverge — one blocks on
/// `Handoff::exchange`, the other calls `Handoff::open` and returns.
struct PreparedHandoff {
    handoff: Handoff,
    request: ReviewRequest,
    working_dir: String,
}

/// Shared setup for both review entry points: verify we're inside herdr,
/// resolve the working dir, build the request, bind the handoff socket, ask
/// herdr to open the pane, and wait for it to connect. The socket *file* is
/// removed as soon as the connection attempt is settled (success or
/// failure) — nothing past this point needs the filesystem path, only the
/// accepted stream, and leaving it around leaks it into the temp dir.
fn prepare_handoff(args: &Value, config: &Config) -> Result<PreparedHandoff> {
    if !herdr::inside_herdr() {
        bail!("not running inside a herdr session (HERDR_ENV != 1); review tools need the agent to live in a herdr pane");
    }
    let working_dir = match args.get("working_dir").and_then(Value::as_str) {
        Some(dir) => dir.to_string(),
        None => std::env::current_dir()
            .context("resolving current directory")?
            .to_string_lossy()
            .into_owned(),
    };
    let request = ReviewRequest {
        version: PROTOCOL_VERSION,
        working_dir: working_dir.clone(),
        baseline: args
            .get("baseline")
            .and_then(Value::as_str)
            .map(str::to_string),
        note: args.get("note").and_then(Value::as_str).map(str::to_string),
    };

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let socket_path = std::env::temp_dir().join(format!(
        "annot-{}-{}.sock",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&socket_path);
    let listener = std::os::unix::net::UnixListener::bind(&socket_path)
        .with_context(|| format!("binding handoff socket {}", socket_path.display()))?;

    eprintln!(
        "herdr-annotator mcp: opening review pane (socket {})",
        socket_path.display()
    );
    let opened = herdr::open_review_pane(&socket_path.to_string_lossy(), config);
    let outcome = opened.and_then(|()| {
        let handoff = Handoff::accept(listener, config.accept_timeout)?;
        eprintln!("herdr-annotator mcp: pane connected");
        Ok(handoff)
    });
    let _ = std::fs::remove_file(&socket_path);
    let handoff = outcome?;

    Ok(PreparedHandoff { handoff, request, working_dir })
}

fn run_review(args: &Value, config: &Config) -> Result<ReviewResult> {
    let prepared = prepare_handoff(args, config)?;
    eprintln!("herdr-annotator mcp: waiting for verdict…");
    let result = prepared.handoff.exchange(&prepared.request, config.review_timeout)?;
    eprintln!("herdr-annotator mcp: verdict received: {:?}", result.verdict);
    Ok(result)
}

fn run_show(args: &Value, config: &Config) -> Result<ActiveReview> {
    let prepared = prepare_handoff(args, config)?;
    let collector_waiting = Arc::new(AtomicBool::new(false));
    let notify: Option<VerdictNotify> = if config.notify_on_verdict {
        let waiting = Arc::clone(&collector_waiting);
        Some(Box::new(move |result: &ReviewResult| {
            if waiting.load(Ordering::SeqCst) {
                return;
            }
            let message = nudge_message(result);
            if let Err(err) = herdr::nudge_agent_pane(&message) {
                eprintln!("herdr-annotator mcp: could not deliver the verdict nudge ({err:#})");
            }
        }))
    } else {
        None
    };
    let open = prepared.handoff.open_with_notify(&prepared.request, notify)?;
    eprintln!("herdr-annotator mcp: review pane open, returning control to the agent");
    Ok(ActiveReview {
        open,
        working_dir: prepared.working_dir,
        opened_at: Instant::now(),
        collector_waiting,
    })
}

/// The line typed into the agent's pane when a verdict lands unasked. Carries
/// only the verdict name and the annotation count — the feedback itself stays
/// behind `collect_review`, so the verdict is consumed exactly once through
/// the normal tool path.
fn nudge_message(result: &ReviewResult) -> String {
    match result.verdict {
        // Deliberately neutral about what happens next: a cancellation can be
        // a deliberate human stop, so the nudge only says the review is over
        // and leaves the reaction to the agent's own operating instructions.
        Verdict::Cancelled => "[herdr-annotator] The review pane closed without a verdict. \
             Call collect_review to confirm the cancellation."
            .to_string(),
        verdict => {
            let verdict = serde_json::to_value(verdict)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_else(|| format!("{verdict:?}"));
            let annotations = match result.annotations.len() {
                0 => "no annotations".to_string(),
                1 => "1 annotation".to_string(),
                n => format!("{n} annotations"),
            };
            format!(
                "[herdr-annotator] The reviewer finished: {verdict} with {annotations}. \
                 Call collect_review to fetch the feedback and act on it."
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Annotation, LineRange, PaneConnection, Side, Verdict};
    use std::os::unix::net::UnixListener;

    thread_local! {
        /// Runs at the top of `handle_collect_review`'s pending path, inside
        /// the race window between the loop's last mailbox check and the
        /// flag store — lets a test deposit a verdict exactly there.
        static PENDING_PATH_HOOK: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
            const { std::cell::RefCell::new(None) };
    }

    pub(super) fn pending_path_test_hook() {
        PENDING_PATH_HOOK.with(|h| {
            if let Some(f) = h.borrow_mut().as_mut() {
                f();
            }
        });
    }

    /// Stand up a fake pane on a unix socket (same pattern as protocol.rs's
    /// tests) and hand back an `ActiveReview` wired to it, plus the pane's
    /// thread handle and a sender the test uses to release the verdict on
    /// its own schedule.
    fn open_fake_review() -> (ActiveReview, std::thread::JoinHandle<()>, std::sync::mpsc::Sender<()>) {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!("annot-mcp-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join(format!("collect-{}.sock", COUNTER.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).unwrap();

        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let sock_client = sock.clone();
        let pane = std::thread::spawn(move || {
            let mut conn = PaneConnection::connect(&sock_client).unwrap();
            let _req = conn.receive_request().unwrap();
            let (mut channel, _goto_rx) = conn.into_channel();
            // Hold the verdict until the test has confirmed the pending state.
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            channel
                .send_result(&ReviewResult {
                    version: PROTOCOL_VERSION,
                    verdict: Verdict::Approve,
                    summary: Some("looks good".into()),
                    annotations: vec![Annotation {
                        file: "src/lib.rs".into(),
                        lines: LineRange { start: 1, end: 2 },
                        side: Side::New,
                        tag: None,
                        comment: "nice".into(),
                    }],
                })
                .unwrap();
        });

        let handoff = Handoff::accept(listener, Duration::from_secs(5)).unwrap();
        let request = ReviewRequest {
            version: PROTOCOL_VERSION,
            working_dir: "/tmp/repo".into(),
            baseline: None,
            note: None,
        };
        let open = handoff.open(&request).unwrap();
        let _ = std::fs::remove_file(&sock);

        (
            ActiveReview {
                open,
                working_dir: "/tmp/repo".into(),
                opened_at: Instant::now(),
                collector_waiting: Arc::new(AtomicBool::new(false)),
            },
            pane,
            release_tx,
        )
    }

    #[test]
    fn collect_review_reports_pending_then_returns_the_verdict_and_clears_active() {
        let (active_review, pane, release_tx) = open_fake_review();
        let mut active = Some(active_review);

        // Nothing sent yet: a single check (wait_seconds omitted) must report
        // pending without blocking, and must not clear the active review.
        let pending = handle_collect_review(&json!({}), &mut active);
        assert_eq!(pending["isError"], false);
        let pending_json: Value =
            serde_json::from_str(pending["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(pending_json["status"], "pending");
        assert!(active.is_some(), "a pending collect must not clear the active review");

        // Let the fake pane deliver its verdict, then poll for it.
        release_tx.send(()).unwrap();
        let collected = handle_collect_review(&json!({ "wait_seconds": 5 }), &mut active);
        assert_eq!(collected["isError"], false);
        let result: ReviewResult =
            serde_json::from_str(collected["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(result.verdict, Verdict::Approve);
        assert_eq!(result.annotations.len(), 1);
        assert!(active.is_none(), "collecting a verdict must clear the active review");

        pane.join().unwrap();
    }

    #[test]
    fn a_pending_collect_lowers_the_collector_flag_so_a_later_verdict_still_nudges() {
        let (active_review, pane, release_tx) = open_fake_review();
        let flag = Arc::clone(&active_review.collector_waiting);
        let mut active = Some(active_review);

        let pending = handle_collect_review(&json!({}), &mut active);
        assert_eq!(pending["isError"], false);
        assert!(
            !flag.load(Ordering::SeqCst),
            "the flag must come back down on the pending path — a verdict landing after this call returned belongs to the nudge"
        );

        release_tx.send(()).unwrap();
        let _ = handle_collect_review(&json!({ "wait_seconds": 5 }), &mut active);
        pane.join().unwrap();
    }

    #[test]
    fn a_verdict_landing_inside_the_pending_window_is_returned_not_stranded() {
        let (active_review, pane, release_tx) = open_fake_review();
        let mut active = Some(active_review);

        // Deposit the verdict inside the race window: after the loop's last
        // mailbox check, while collector_waiting is still up (so a wired
        // nudge would be suppressed). The call must return the verdict —
        // returning "pending" here strands it with no continuation signal.
        PENDING_PATH_HOOK.with(|h| {
            *h.borrow_mut() = Some(Box::new(move || {
                let _ = release_tx.send(());
                // Give the pane and mailbox threads time to deposit.
                std::thread::sleep(Duration::from_millis(500));
            }));
        });
        let out = handle_collect_review(&json!({}), &mut active);
        PENDING_PATH_HOOK.with(|h| *h.borrow_mut() = None);

        assert_eq!(out["isError"], false);
        let result: ReviewResult =
            serde_json::from_str(out["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(
            result.verdict,
            Verdict::Approve,
            "a verdict deposited during the pending window must be returned, not stranded behind a suppressed nudge"
        );
        assert!(active.is_none(), "collecting a verdict must clear the active review");
        pane.join().unwrap();
    }

    #[test]
    fn show_changes_promises_the_nudge_only_when_the_config_delivers_it() {
        let on = Config::default();
        let with_nudge = tool_descriptors(&on);
        let desc = |tools: &[Value]| {
            tools
                .iter()
                .find(|t| t["name"] == SHOW_CHANGES)
                .and_then(|t| t["description"].as_str())
                .unwrap()
                .to_string()
        };
        assert!(
            desc(&with_nudge).contains("[herdr-annotator]"),
            "with notify_on_verdict on, the description must tell the agent about the nudge"
        );

        let off = Config { notify_on_verdict: false, ..Config::default() };
        let without_nudge = tool_descriptors(&off);
        assert!(
            !desc(&without_nudge).contains("[herdr-annotator]"),
            "with notify_on_verdict off, the description must not promise a prompt that never comes"
        );
    }

    #[test]
    fn nudge_messages_carry_the_verdict_and_count_but_never_the_feedback() {
        let annotation = Annotation {
            file: "src/lib.rs".into(),
            lines: LineRange { start: 1, end: 2 },
            side: Side::New,
            tag: Some("fix".into()),
            comment: "secret feedback".into(),
        };
        let request_changes = ReviewResult {
            version: PROTOCOL_VERSION,
            verdict: Verdict::RequestChanges,
            summary: Some("secret summary".into()),
            annotations: vec![annotation.clone(), annotation.clone(), annotation],
        };
        let msg = nudge_message(&request_changes);
        assert!(msg.contains("request_changes with 3 annotations"), "unexpected message: {msg}");
        assert!(msg.contains("collect_review"), "the nudge must point at the tool: {msg}");
        assert!(
            !msg.contains("secret"),
            "annotation bodies and summaries must stay behind collect_review: {msg}"
        );

        let approve = ReviewResult {
            version: PROTOCOL_VERSION,
            verdict: Verdict::Approve,
            summary: None,
            annotations: Vec::new(),
        };
        let msg = nudge_message(&approve);
        assert!(msg.contains("approve with no annotations"), "unexpected message: {msg}");

        let cancelled = ReviewResult::cancelled("review pane closed without a verdict");
        let msg = nudge_message(&cancelled);
        assert!(msg.contains("without a verdict"), "unexpected message: {msg}");
        assert!(msg.contains("collect_review"), "the nudge must point at the tool: {msg}");
    }

    #[test]
    fn goto_and_collect_require_an_open_review() {
        let mut active: Option<ActiveReview> = None;

        let goto_err = handle_goto(&json!({ "file": "src/a.rs", "line": 1 }), &mut active);
        assert_eq!(goto_err["isError"], true);
        assert_eq!(goto_err["content"][0]["text"], "no open review — call show_changes first");

        let collect_err = handle_collect_review(&json!({}), &mut active);
        assert_eq!(collect_err["isError"], true);
        assert_eq!(collect_err["content"][0]["text"], "no open review");
    }

    #[test]
    fn goto_rejects_a_non_positive_line() {
        let (active_review, pane, release_tx) = open_fake_review();
        let mut active = Some(active_review);

        let err = handle_goto(&json!({ "file": "src/a.rs", "line": 0 }), &mut active);
        assert_eq!(err["isError"], true);

        // Unblock and drain the fake pane so the test doesn't leak a thread.
        release_tx.send(()).unwrap();
        let _ = handle_collect_review(&json!({ "wait_seconds": 5 }), &mut active);
        pane.join().unwrap();
    }

    #[test]
    fn focus_pushes_regions_and_an_empty_list_clears_without_moving_the_cursor() {
        let (active_review, pane, release_tx) = open_fake_review();
        let mut active = Some(active_review);

        // Regions land in source view on the first listed region.
        let ok = handle_focus(
            &json!({ "file": "src/a.rs", "regions": [
                { "start": 10, "end": 22 }, { "start": 40, "end": 51 }
            ]}),
            &mut active,
        );
        assert_eq!(ok["isError"], false, "{ok}");
        let text = ok["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("10-22, 40-51"), "{text}");

        // Empty regions = clear; the reply says so rather than pretending a
        // region was focused.
        let cleared = handle_focus(&json!({ "file": "src/a.rs", "regions": [] }), &mut active);
        assert_eq!(cleared["isError"], false);
        assert!(
            cleared["content"][0]["text"].as_str().unwrap().contains("cleared"),
            "{cleared}"
        );

        release_tx.send(()).unwrap();
        let _ = handle_collect_review(&json!({ "wait_seconds": 5 }), &mut active);
        pane.join().unwrap();
    }

    // A reviewer-made manual fold (from `f`) isn't touched by clearing the
    // agent's focus, so the reply must not claim the whole file becomes
    // visible again — only that the agent's own focus is gone.
    #[test]
    fn focus_clear_message_does_not_overclaim_full_file_visibility() {
        let (active_review, pane, release_tx) = open_fake_review();
        let mut active = Some(active_review);

        let cleared = handle_focus(&json!({ "file": "src/a.rs", "regions": [] }), &mut active);
        let text = cleared["content"][0]["text"].as_str().unwrap();
        assert!(
            !text.contains("whole file is visible"),
            "manual folds from f survive a focus clear, so this overclaims: {text}"
        );

        release_tx.send(()).unwrap();
        let _ = handle_collect_review(&json!({ "wait_seconds": 5 }), &mut active);
        pane.join().unwrap();
    }

    #[test]
    fn focus_validates_its_arguments() {
        // No open review: same contract as goto.
        let mut none: Option<ActiveReview> = None;
        let err = handle_focus(&json!({ "file": "a.rs", "regions": [] }), &mut none);
        assert_eq!(err["isError"], true);

        let (active_review, pane, release_tx) = open_fake_review();
        let mut active = Some(active_review);

        // Missing regions, inverted region, and zero line are malformed —
        // errors here, not values silently normalized (normalization is the
        // PANE's job for semantically odd but well-formed input).
        for bad in [
            json!({ "file": "src/a.rs" }),
            json!({ "file": "src/a.rs", "regions": [{ "start": 9, "end": 5 }] }),
            json!({ "file": "src/a.rs", "regions": [{ "start": 0, "end": 5 }] }),
            json!({ "file": "src/a.rs", "regions": [{ "start": 3 }] }),
            json!({ "file": "", "regions": [] }),
        ] {
            let err = handle_focus(&bad, &mut active);
            assert_eq!(err["isError"], true, "should reject {bad}");
        }
        assert!(active.is_some(), "rejected arguments must not disturb the open review");

        release_tx.send(()).unwrap();
        let _ = handle_collect_review(&json!({ "wait_seconds": 5 }), &mut active);
        pane.join().unwrap();
    }

    #[test]
    fn collect_review_rejects_a_malformed_wait_seconds_instead_of_coercing_it() {
        let (active_review, pane, release_tx) = open_fake_review();
        let mut active = Some(active_review);

        // A negative value used to silently become 0 (single check, no
        // error) via as_u64() failing and unwrap_or(0) swallowing that.
        let negative = handle_collect_review(&json!({ "wait_seconds": -5 }), &mut active);
        assert_eq!(negative["isError"], true);
        assert!(active.is_some(), "a rejected argument must not disturb the open review");

        // Above the documented 0..=120 range used to silently clamp to 120
        // rather than telling the caller their argument was out of range.
        let too_large = handle_collect_review(&json!({ "wait_seconds": 999 }), &mut active);
        assert_eq!(too_large["isError"], true);

        // A non-integer is just as malformed as a negative one.
        let not_a_number = handle_collect_review(&json!({ "wait_seconds": "soon" }), &mut active);
        assert_eq!(not_a_number["isError"], true);

        // Omitted and explicit null are both legitimate ways to ask for the
        // documented default (0, single check) and must not error.
        release_tx.send(()).unwrap();
        let omitted = handle_collect_review(&json!({}), &mut active);
        assert_eq!(omitted["isError"], false);
        pane.join().unwrap();
    }
}
