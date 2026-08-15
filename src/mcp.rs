//! MCP stdio server exposing the `review_changes` tool.
//!
//! Deliberately hand-rolled: the server speaks newline-delimited JSON-RPC 2.0 on
//! stdin/stdout (the MCP stdio transport) and implements only what a single-tool
//! server needs — initialize, tools/list, tools/call, ping. Logs go to stderr;
//! stdout carries protocol frames only.
//!
//! `review_changes` is intentionally blocking: it binds a unix socket, asks herdr
//! to open the review pane beside the agent, and does not return until the human
//! delivers a verdict (or the pane dies, which maps to a Cancelled result). While
//! a review is in flight no other frames are read — one review at a time is the
//! product, not a limitation.

use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::config::{self, Config};
use crate::herdr;
use crate::protocol::{Handoff, ReviewRequest, ReviewResult, PROTOCOL_VERSION};

const TOOL_NAME: &str = "review_changes";

pub fn run() -> Result<()> {
    let config = config::load();
    let stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
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
                Some(result_frame(id, json!({ "tools": [tool_descriptor()] })))
            }
            ("tools/call", Some(id)) => Some(result_frame(id, handle_tool_call(&params, &config))),
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

fn tool_descriptor() -> Value {
    json!({
        "name": TOOL_NAME,
        "description": "Open a review pane beside this agent inside herdr and BLOCK until the human reviews the diff and returns a verdict with line-anchored annotations. Call this at checkpoints (e.g. before marking a task complete). The diff shown is the working tree vs `baseline` (a git rev); omit `baseline` to show all uncommitted changes.",
        "inputSchema": {
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
        }
    })
}

fn handle_tool_call(params: &Value, config: &Config) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
    if name != TOOL_NAME {
        return tool_error(format!("unknown tool: {name}"));
    }
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    match run_review(&args, config) {
        Ok(result) => {
            let text = serde_json::to_string_pretty(&result)
                .unwrap_or_else(|e| format!("{{\"error\":\"serialize: {e}\"}}"));
            json!({ "content": [{ "type": "text", "text": text }], "isError": false })
        }
        Err(err) => tool_error(format!("{err:#}")),
    }
}

fn tool_error(message: String) -> Value {
    eprintln!("herdr-annotator mcp: tool error: {message}");
    json!({ "content": [{ "type": "text", "text": message }], "isError": true })
}

fn run_review(args: &Value, config: &Config) -> Result<ReviewResult> {
    if !herdr::inside_herdr() {
        bail!("not running inside a herdr session (HERDR_ENV != 1); review_changes needs the agent to live in a herdr pane");
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
        working_dir,
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
        eprintln!("herdr-annotator mcp: pane connected, waiting for verdict…");
        handoff.exchange(&request, config.review_timeout)
    });
    let _ = std::fs::remove_file(&socket_path);
    let result = outcome?;
    eprintln!("herdr-annotator mcp: verdict received: {:?}", result.verdict);
    Ok(result)
}
