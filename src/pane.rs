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

    let model = crate::diff::load(&request.working_dir, request.baseline.as_deref());
    let outcome = crate::ui::run(&request, model)?;

    let result = ReviewResult {
        version: PROTOCOL_VERSION,
        verdict: outcome.verdict,
        summary: outcome.summary,
        annotations: outcome.annotations,
    };
    conn.send_result(&result)?;
    println!(
        "verdict sent: {}",
        serde_json::to_string(&result.verdict).unwrap_or_default()
    );
    Ok(())
}
