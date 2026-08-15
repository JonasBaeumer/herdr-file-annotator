//! Thin wrapper around the herdr CLI.
//!
//! The MCP server runs inside a herdr-managed pane (spawned by the agent, which
//! itself lives in one), so it inherits HERDR_* env vars. We use HERDR_BIN_PATH
//! when herdr provides it and fall back to `herdr` on PATH.

use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::config::{Config, Placement, SplitDirection};

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
///
/// Flag shape matters (verified against herdr 0.8.0): a split open takes
/// `--placement split --target-pane <id> --direction <dir>` and must NOT also
/// pass `--workspace` — combining them makes the server reject the request with
/// "split and zoomed plugin panes target an existing pane; use target_pane_id".
/// `--workspace` belongs to tab placement only (same split reviewr uses).
pub fn open_review_pane(socket_path: &str, config: &Config) -> Result<()> {
    let mut cmd = Command::new(herdr_bin());
    cmd.args(["plugin", "pane", "open", "--plugin", PLUGIN_ID, "--entrypoint", PANE_ENTRYPOINT]);

    let pane_id = std::env::var("HERDR_PANE_ID").ok();
    match (config.placement, pane_id) {
        // Split beside the agent's own pane — only possible with a pane id.
        (Placement::Split, Some(pane_id)) => {
            let direction = match config.direction {
                SplitDirection::Right => "right",
                SplitDirection::Down => "down",
            };
            cmd.args(["--placement", "split", "--target-pane", &pane_id, "--direction", direction]);
        }
        // Tab placement, or split requested but no pane context (e.g. agent
        // launched outside a managed pane): fall back to a tab in the
        // agent's workspace.
        (_, _) => {
            let workspace = std::env::var("HERDR_WORKSPACE_ID").context(
                "neither HERDR_PANE_ID nor HERDR_WORKSPACE_ID is set — the agent does not appear to run inside herdr",
            )?;
            cmd.args(["--placement", "tab", "--workspace", &workspace]);
        }
    }
    cmd.arg(if config.focus { "--focus" } else { "--no-focus" });
    cmd.arg("--env");
    cmd.arg(format!("{}={}", crate::protocol::SOCKET_ENV, socket_path));
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
