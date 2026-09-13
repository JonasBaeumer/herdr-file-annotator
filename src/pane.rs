//! The review pane herdr spawns beside the agent.
//!
//! Connects to the handoff socket, loads the diff, hands off to the ratatui
//! UI (`ui::run`) for the actual review, and reports the reviewer's verdict
//! and any line-anchored annotations back over the socket.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;

use crate::protocol::{PaneConnection, ReviewResult, PROTOCOL_VERSION, SOCKET_ENV};

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
    // Split the socket: verdict goes back on the write half; the read half
    // becomes a live stream of agent-pushed navigation (guided walkthroughs).
    let (mut channel, goto_rx) = conn.into_channel();

    let model = crate::diff::load(&request.working_dir, request.baseline.as_deref());
    // The pane reads the same config file as the MCP server; only the
    // display keys matter here (placement/timeouts are the server's side).
    let config = crate::config::load();

    // Show up in herdr's agent view as blocked while the review is pending,
    // so a reviewer working in another tab sees the attention dot. Best
    // effort: status display must never stop a review from opening.
    let repo = std::path::Path::new(&request.working_dir)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| request.working_dir.clone());
    if let Err(err) = crate::herdr::mark_pane_blocked(&format!("review pending: {repo}")) {
        eprintln!("herdr-annotator pane: could not report blocked status: {err:#}");
    }

    // On the error path the process exits and herdr drops the agent-view
    // entry with the pane, so `?` here cannot leak a stale blocked dot.
    let outcome = crate::ui::run(&request, model, goto_rx, config)?;
    if let Err(err) = crate::herdr::release_pane_agent() {
        eprintln!("herdr-annotator pane: could not release the agent-view entry: {err:#}");
    }

    let result = ReviewResult {
        version: PROTOCOL_VERSION,
        verdict: outcome.verdict,
        summary: outcome.summary,
        annotations: outcome.annotations,
    };
    // A failed send is expected on the disconnect path (the server already
    // shut the socket — timeout or agent exit); the review is discarded, not
    // a pane crash.
    if let Err(err) = channel.send_result(&result) {
        eprintln!("herdr-annotator pane: verdict could not be delivered (agent gone): {err:#}");
        return Ok(());
    }
    println!(
        "verdict sent: {}",
        serde_json::to_string(&result.verdict).unwrap_or_default()
    );
    Ok(())
}
