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

    let repo = std::path::Path::new(&request.working_dir)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| request.working_dir.clone());
    let outcome =
        ui_under_blocked_status(&repo, || crate::ui::run(&request, model, goto_rx, config))?;

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

/// Run the UI bracketed by the agent-view lifecycle: report this pane as
/// blocked before the UI opens (so a reviewer in another tab sees the
/// attention dot while the review is pending) and release the entry after
/// the UI returns. Both status calls are best effort — status display must
/// never stop a review — so their failures are logged, not propagated.
///
/// A UI error propagates *without* releasing: the process exits and herdr
/// drops the agent-view entry with the pane, so `?` cannot leak a stale
/// blocked dot.
fn ui_under_blocked_status<T>(repo: &str, ui: impl FnOnce() -> Result<T>) -> Result<T> {
    if let Err(err) = crate::herdr::mark_pane_blocked(&format!("review pending: {repo}")) {
        eprintln!("herdr-annotator pane: could not report blocked status: {err:#}");
    }
    let outcome = ui()?;
    if let Err(err) = crate::herdr::release_pane_agent() {
        eprintln!("herdr-annotator pane: could not release the agent-view entry: {err:#}");
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::herdr::tests::{fake_herdr_appending, ENV_LOCK};
    use std::io::Write;

    fn mark_ui_ran(log: &std::path::Path) {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(log).unwrap();
        writeln!(f, "ui").unwrap();
    }

    #[test]
    fn ui_runs_between_the_blocked_report_and_the_release() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("annot-pane-lifecycle-{}", std::process::id()));
        let (script, log) = fake_herdr_appending(&dir, 0);
        std::env::set_var("HERDR_BIN_PATH", &script);
        std::env::set_var("HERDR_PANE_ID", "w1:p1");
        let result = ui_under_blocked_status("myrepo", || {
            mark_ui_ran(&log);
            Ok(42)
        });
        std::env::remove_var("HERDR_PANE_ID");
        std::env::remove_var("HERDR_BIN_PATH");

        assert_eq!(result.unwrap(), 42);
        let argv = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            argv,
            "pane report-agent w1:p1 --source jonasbaeumer.file-annotator \
             --agent annotator --state blocked --message review pending: myrepo\n\
             ui\n\
             pane release-agent w1:p1 --source jonasbaeumer.file-annotator --agent annotator\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_ui_error_propagates_and_skips_the_release() {
        // Deliberate: on the error path the process exits and herdr drops
        // the agent-view entry with the pane, so no release call is made.
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("annot-pane-ui-err-{}", std::process::id()));
        let (script, log) = fake_herdr_appending(&dir, 0);
        std::env::set_var("HERDR_BIN_PATH", &script);
        std::env::set_var("HERDR_PANE_ID", "w1:p1");
        let result: Result<u32> =
            ui_under_blocked_status("myrepo", || anyhow::bail!("terminal init failed"));
        std::env::remove_var("HERDR_PANE_ID");
        std::env::remove_var("HERDR_BIN_PATH");

        assert!(result.is_err(), "the UI error must reach pane::run's caller");
        let argv = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            argv,
            "pane report-agent w1:p1 --source jonasbaeumer.file-annotator \
             --agent annotator --state blocked --message review pending: myrepo\n",
            "only the blocked report may run; no release on the error path"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn status_call_failures_never_stop_the_review() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("annot-pane-status-err-{}", std::process::id()));
        let (script, log) = fake_herdr_appending(&dir, 1);
        std::env::set_var("HERDR_BIN_PATH", &script);
        std::env::set_var("HERDR_PANE_ID", "w1:p1");
        let result = ui_under_blocked_status("myrepo", || {
            mark_ui_ran(&log);
            Ok(7)
        });
        std::env::remove_var("HERDR_PANE_ID");
        std::env::remove_var("HERDR_BIN_PATH");

        assert_eq!(result.unwrap(), 7, "failing status calls must not fail the review");
        let argv = std::fs::read_to_string(&log).unwrap();
        assert!(argv.contains("ui\n"), "the UI must still run: {argv}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
