//! Thin wrapper around the herdr CLI.
//!
//! The MCP server runs inside a herdr-managed pane (spawned by the agent, which
//! itself lives in one), so it inherits HERDR_* env vars. We use HERDR_BIN_PATH
//! when herdr provides it and fall back to `herdr` on PATH.

use std::process::Command;

use anyhow::{bail, Context, Result};

pub const PLUGIN_ID: &str = "jonasbaeumer.file-annotator";
pub const PANE_ENTRYPOINT: &str = "review";

fn herdr_bin() -> String {
    std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".to_string())
}

pub fn inside_herdr() -> bool {
    std::env::var("HERDR_ENV").as_deref() == Ok("1")
}

/// Open the review pane beside the calling agent's pane, injecting the handoff
/// socket path. herdr CLI server errors arrive as JSON on stderr with exit 1.
pub fn open_review_pane(socket_path: &str) -> Result<()> {
    let workspace = std::env::var("HERDR_WORKSPACE_ID")
        .context("HERDR_WORKSPACE_ID is not set — the agent does not appear to run inside herdr")?;
    let mut cmd = Command::new(herdr_bin());
    cmd.args([
        "plugin",
        "pane",
        "open",
        "--plugin",
        PLUGIN_ID,
        "--entrypoint",
        PANE_ENTRYPOINT,
        "--workspace",
        &workspace,
        "--direction",
        "right",
        "--focus",
        "--env",
    ]);
    cmd.arg(format!("{}={}", crate::protocol::SOCKET_ENV, socket_path));
    // Split beside the agent's own pane when we know it; otherwise herdr picks.
    if let Ok(pane_id) = std::env::var("HERDR_PANE_ID") {
        cmd.args(["--target-pane", &pane_id]);
    }
    let output = cmd.output().context("spawning herdr CLI")?;
    if !output.status.success() {
        bail!(
            "`herdr plugin pane open` failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}
