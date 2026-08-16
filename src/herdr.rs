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

/// Toggle zoom on the pane this process runs in (the review pane calls this
/// for its `z` key). Best-effort: a failure is the caller's to ignore — zoom
/// is a convenience, never a correctness matter.
pub fn zoom_toggle_current() -> Result<()> {
    let output = Command::new(herdr_bin())
        .args(["pane", "zoom", "--current", "--toggle"])
        .output()
        .context("spawning herdr CLI for zoom")?;
    if !output.status.success() {
        bail!("`herdr pane zoom` failed: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    /// Serializes the HERDR_BIN_PATH mutation: these tests swap the binary
    /// the whole module resolves, so they must not overlap each other.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn fake_herdr(dir: &Path, exit_code: i32) -> (PathBuf, PathBuf) {
        std::fs::create_dir_all(dir).unwrap();
        let log = dir.join("argv.log");
        let script = dir.join("herdr-fake.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\nprintf '%s\\n' \"$*\" > '{}'\nexit {}\n", log.display(), exit_code),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        (script, log)
    }

    #[test]
    fn zoom_invokes_the_documented_herdr_cli_contract() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("annot-zoom-ok-{}", std::process::id()));
        let (script, log) = fake_herdr(&dir, 0);
        std::env::set_var("HERDR_BIN_PATH", &script);
        let result = zoom_toggle_current();
        std::env::remove_var("HERDR_BIN_PATH");

        assert!(result.is_ok(), "zero exit must be Ok: {result:?}");
        let argv = std::fs::read_to_string(&log).unwrap();
        assert_eq!(argv.trim(), "pane zoom --current --toggle");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn zoom_surfaces_a_nonzero_exit_as_an_error() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("annot-zoom-err-{}", std::process::id()));
        let (script, _log) = fake_herdr(&dir, 1);
        std::env::set_var("HERDR_BIN_PATH", &script);
        let result = zoom_toggle_current();
        std::env::remove_var("HERDR_BIN_PATH");

        assert!(result.is_err(), "nonzero exit must be Err so the caller can log it");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
