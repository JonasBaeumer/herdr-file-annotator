mod diff;
mod herdr;
mod mcp;
mod pane;
mod protocol;
mod ui;

use std::process::ExitCode;

const USAGE: &str = "\
herdr-annotator — agent-summoned diff review inside herdr

Usage:
  herdr-annotator mcp     Run as an MCP stdio server (register this with your agent)
  herdr-annotator pane    Run as the herdr review pane (launched by herdr, not by hand)
";

fn main() -> ExitCode {
    let mode = std::env::args().nth(1);
    let result = match mode.as_deref() {
        Some("mcp") => mcp::run(),
        Some("pane") => pane::run(),
        Some("--version") | Some("-V") => {
            println!("herdr-annotator {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        _ => {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("herdr-annotator: {err:#}");
            ExitCode::FAILURE
        }
    }
}
