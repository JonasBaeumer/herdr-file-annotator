//! Plugin config file (`config.toml`) resolution, parsing, and validation.
//!
//! The config directory is resolved via `herdr plugin config-dir <plugin-id>`
//! (binary from HERDR_BIN_PATH, falling back to `herdr` on PATH), which prints
//! the directory path as plain text (verified against herdr 0.8.0 — no JSON).
//! If that command is unavailable or fails, we fall back to computing the same
//! path herdr itself uses: `$XDG_CONFIG_HOME` (or `~/.config`) +
//! `/herdr/plugins/config/<plugin-id>`.
//!
//! Missing file or missing keys fall back to defaults (current behavior).
//! Malformed TOML or invalid values print ONE warning line to stderr and fall
//! back to full defaults — a bad config file must never crash the server.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use serde::Deserialize;

use crate::herdr::PLUGIN_ID;
use crate::keymap::Keymap;

const CONFIG_FILE_NAME: &str = "config.toml";
const DEFAULT_ACCEPT_TIMEOUT_SECS: u64 = 20;

/// Every tool name the MCP server knows, and the single source of truth for
/// what `enabled_tools` may name — `config.rs` and `mcp.rs` both read it.
pub const ALL_TOOL_NAMES: &[&str] = &[
    "review_changes",
    "show_changes",
    "goto",
    "focus",
    "collect_review",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    Split,
    Tab,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    Right,
    Down,
    /// Right or down, chosen from the agent pane's shape each time a review opens.
    Auto,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub placement: Placement,
    pub direction: SplitDirection,
    pub focus: bool,
    pub accept_timeout: Duration,
    pub review_timeout: Option<Duration>,
    /// When a non-blocking review's verdict lands with no `collect_review`
    /// call waiting on it, type a short prompt into the agent's pane so the
    /// agent picks the feedback up on its own.
    pub notify_on_verdict: bool,
    /// Start the pane with long lines wrapped to the pane width instead of
    /// clipped-and-pannable. Either way `w` toggles it live per review.
    pub wrap_lines: bool,
    /// The pane's keybindings: defaults plus the `[keys]` table's
    /// overrides. Resolved here so an invalid table follows the same
    /// warn-and-use-defaults path as every other config problem.
    pub keymap: Keymap,
    /// The MCP tools the server exposes. `None` (the default) means every
    /// tool; `Some(list)` exposes only the listed tools — `tools/list` hides
    /// the rest and a call to a hidden tool is rejected. An explicit empty
    /// list exposes no tools. Names are validated against `ALL_TOOL_NAMES`:
    /// unknown or duplicate entries reject the whole config, same
    /// all-or-nothing fallback as a bad `[keys]` table. One dependency is
    /// enforced: `show_changes` requires `collect_review`, because only
    /// `collect_review` clears the open-review slot — without it the first
    /// guided review would wedge the server for good.
    pub enabled_tools: Option<Vec<String>>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            placement: Placement::Split,
            direction: SplitDirection::Right,
            focus: true,
            accept_timeout: Duration::from_secs(DEFAULT_ACCEPT_TIMEOUT_SECS),
            review_timeout: None,
            notify_on_verdict: true,
            wrap_lines: false,
            keymap: Keymap::default(),
            enabled_tools: None,
        }
    }
}

/// Intermediate all-`Option` shape mirroring the on-disk TOML schema, so a
/// partially-specified file (or an absent one) leaves the rest at defaults.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    placement: Option<String>,
    direction: Option<String>,
    focus: Option<bool>,
    accept_timeout_secs: Option<u64>,
    review_timeout_secs: Option<u64>,
    notify_on_verdict: Option<bool>,
    wrap_lines: Option<bool>,
    keys: Option<HashMap<String, String>>,
    enabled_tools: Option<Vec<String>>,
}

/// Load the plugin config, falling back to defaults on any problem. Never
/// returns an `Err` and never panics — a bad or missing config must not stop
/// the MCP server from starting.
pub fn load() -> Config {
    let dir = config_dir();
    let path = dir.join(CONFIG_FILE_NAME);

    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Config::default(),
        Err(err) => {
            eprintln!(
                "herdr-annotator: warning: could not read config file {} ({err}); using defaults",
                path.display()
            );
            return Config::default();
        }
    };

    let raw: RawConfig = match toml::from_str(&contents) {
        Ok(raw) => raw,
        Err(err) => {
            eprintln!(
                "herdr-annotator: warning: malformed config file {} ({err}); using defaults",
                path.display()
            );
            return Config::default();
        }
    };

    match validate(raw) {
        Ok(config) => config,
        Err(reason) => {
            eprintln!(
                "herdr-annotator: warning: invalid config in {} ({reason}); using defaults",
                path.display()
            );
            Config::default()
        }
    }
}

fn validate(raw: RawConfig) -> Result<Config, String> {
    let default = Config::default();

    let placement = match raw.placement.as_deref() {
        None => default.placement,
        Some("split") => Placement::Split,
        Some("tab") => Placement::Tab,
        Some(other) => return Err(format!("unknown placement {other:?} (expected \"split\" or \"tab\")")),
    };

    let direction = match raw.direction.as_deref() {
        None => default.direction,
        Some("right") => SplitDirection::Right,
        Some("down") => SplitDirection::Down,
        Some("auto") => SplitDirection::Auto,
        Some(other) => return Err(format!("unknown direction {other:?} (expected \"right\", \"down\" or \"auto\")")),
    };

    let focus = raw.focus.unwrap_or(default.focus);

    let accept_timeout = match raw.accept_timeout_secs {
        None => default.accept_timeout,
        Some(0) => return Err("accept_timeout_secs must be greater than 0".to_string()),
        Some(secs) => Duration::from_secs(secs),
    };

    let review_timeout = match raw.review_timeout_secs {
        None => default.review_timeout,
        Some(0) => return Err("review_timeout_secs must be greater than 0".to_string()),
        Some(secs) => Some(Duration::from_secs(secs)),
    };

    Ok(Config {
        placement,
        direction,
        focus,
        accept_timeout,
        review_timeout,
        notify_on_verdict: raw.notify_on_verdict.unwrap_or(default.notify_on_verdict),
        wrap_lines: raw.wrap_lines.unwrap_or(default.wrap_lines),
        keymap: match &raw.keys {
            None => default.keymap,
            Some(pairs) => Keymap::with_overrides(pairs).map_err(|e| format!("[keys] {e}"))?,
        },
        enabled_tools: match raw.enabled_tools {
            None => default.enabled_tools,
            Some(names) => {
                let mut seen = std::collections::HashSet::new();
                for name in &names {
                    if !ALL_TOOL_NAMES.contains(&name.as_str()) {
                        return Err(format!(
                            "[enabled_tools] unknown tool {name:?} (known tools: {ALL_TOOL_NAMES:?})"
                        ));
                    }
                    if !seen.insert(name.clone()) {
                        return Err(format!("[enabled_tools] duplicate entry {name:?}"));
                    }
                }
                // Only collect_review clears the open-review slot, so a list
                // that lets the agent open a guided review but never collect
                // it would wedge the server after one show_changes call.
                if names.iter().any(|n| n == "show_changes")
                    && !names.iter().any(|n| n == "collect_review")
                {
                    return Err("[enabled_tools] show_changes requires collect_review \
                                (a guided review's verdict is only retrievable through \
                                collect_review)"
                        .to_string());
                }
                Some(names)
            }
        },
    })
}

impl Config {
    /// Is the named tool exposed by this config? `None` means every tool.
    pub fn tool_enabled(&self, name: &str) -> bool {
        match &self.enabled_tools {
            None => true,
            Some(list) => list.iter().any(|n| n == name),
        }
    }
}

/// Resolve the plugin's config directory: prefer asking herdr itself (so we
/// stay correct if herdr ever changes its layout), falling back to computing
/// the same path herdr 0.8.0 uses if the CLI call is unavailable or fails.
fn config_dir() -> PathBuf {
    let bin = std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".to_string());
    let output = Command::new(&bin)
        .args(["plugin", "config-dir", PLUGIN_ID])
        .output();

    if let Ok(output) = output {
        if output.status.success() {
            let printed = String::from_utf8_lossy(&output.stdout);
            let trimmed = printed.trim();
            if !trimmed.is_empty() {
                return PathBuf::from(trimmed);
            }
        }
    }

    fallback_config_dir()
}

fn fallback_config_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").unwrap_or_else(|| "/".into());
            PathBuf::from(home).join(".config")
        });
    base.join("herdr").join("plugins").join("config").join(PLUGIN_ID)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_when_no_keys_present() {
        let raw: RawConfig = toml::from_str("").unwrap();
        let config = validate(raw).unwrap();
        assert_eq!(config.placement, Placement::Split);
        assert_eq!(config.direction, SplitDirection::Right);
        assert!(config.focus);
        assert_eq!(config.accept_timeout, Duration::from_secs(20));
        assert_eq!(config.review_timeout, None);
        assert!(config.notify_on_verdict, "the nudge is on unless the config turns it off");
        assert!(!config.wrap_lines, "clip-and-pan stays the default; wrap is opt-in");
        assert!(
            config.enabled_tools.is_none(),
            "enabled_tools defaults to every tool"
        );
    }

    #[test]
    fn parses_all_keys() {
        let raw: RawConfig = toml::from_str(
            r#"
            placement = "tab"
            direction = "down"
            focus = false
            accept_timeout_secs = 5
            review_timeout_secs = 600
            notify_on_verdict = false
            wrap_lines = true
            "#,
        )
        .unwrap();
        let config = validate(raw).unwrap();
        assert_eq!(config.placement, Placement::Tab);
        assert_eq!(config.direction, SplitDirection::Down);
        assert!(!config.focus);
        assert_eq!(config.accept_timeout, Duration::from_secs(5));
        assert_eq!(config.review_timeout, Some(Duration::from_secs(600)));
        assert!(!config.notify_on_verdict);
        assert!(config.wrap_lines);
    }

    #[test]
    fn enabled_tools_defaults_to_every_tool_and_a_list_restricts_it() {
        // Absent means all tools.
        let all: RawConfig = toml::from_str("").unwrap();
        let config = validate(all).unwrap();
        assert!(config.enabled_tools.is_none());
        for name in ALL_TOOL_NAMES {
            assert!(config.tool_enabled(name));
        }

        // A non-empty list exposes only the chosen tools.
        let raw: RawConfig =
            toml::from_str(r#"enabled_tools = ["show_changes", "goto", "collect_review"]"#)
                .unwrap();
        let config = validate(raw).unwrap();
        assert!(config.tool_enabled("show_changes"));
        assert!(config.tool_enabled("goto"));
        assert!(config.tool_enabled("collect_review"));
        assert!(!config.tool_enabled("review_changes"));
        assert!(!config.tool_enabled("focus"));

        // An explicit empty list exposes no tools — a deliberate shutdown,
        // distinct from the key being absent (every tool).
        let none: RawConfig = toml::from_str("enabled_tools = []").unwrap();
        let config = validate(none).unwrap();
        assert_eq!(config.enabled_tools, Some(Vec::new()));
        assert!(!config.tool_enabled("review_changes"));
    }

    #[test]
    fn enabled_tools_unknown_or_duplicate_names_reject_the_config() {
        for table in [
            r#"enabled_tools = ["review_changes", "nope"]"#,
            r#"enabled_tools = ["show_changes", "show_changes", "collect_review"]"#,
            r#"enabled_tools = [""]"#,
            // show_changes without collect_review: the verdict of a guided
            // review would be unretrievable and the open-review slot could
            // never be cleared — the server would wedge after one call.
            r#"enabled_tools = ["show_changes"]"#,
            r#"enabled_tools = ["show_changes", "goto", "focus"]"#,
        ] {
            let raw: RawConfig = toml::from_str(table).unwrap();
            let err = validate(raw);
            assert!(err.is_err(), "{table:?} must be rejected, got {err:?}");
        }
    }

    #[test]
    fn keys_table_overrides_bindings_and_bad_tables_reject_the_config() {
        let raw: RawConfig = toml::from_str(
            r#"
            [keys]
            wrap = "W"
            approve = "ctrl+a"
            "#,
        )
        .unwrap();
        let config = validate(raw).unwrap();
        assert_eq!(config.keymap.label(crate::keymap::Action::Wrap), "W");
        assert_eq!(config.keymap.label(crate::keymap::Action::Approve), "ctrl+a");

        // Any bad entry rejects the whole config, which `load` then turns
        // into the standard warn-and-use-defaults fallback.
        for table in ["[keys]\nfly = \"f\"", "[keys]\napprove = \"enter\"", "[keys]\ndown = \"a\""]
        {
            let raw: RawConfig = toml::from_str(table).unwrap();
            let err = validate(raw);
            assert!(err.is_err(), "{table:?} must be rejected, got {err:?}");
        }
    }

    #[test]
    fn rejects_unknown_placement() {
        let raw: RawConfig = toml::from_str(r#"placement = "float""#).unwrap();
        assert!(validate(raw).is_err());
    }

    #[test]
    fn parses_auto_direction() {
        let raw: RawConfig = toml::from_str(r#"direction = "auto""#).unwrap();
        assert_eq!(validate(raw).unwrap().direction, SplitDirection::Auto);
    }

    #[test]
    fn rejects_unknown_direction() {
        let raw: RawConfig = toml::from_str(r#"direction = "up""#).unwrap();
        assert!(validate(raw).is_err());
    }

    #[test]
    fn rejects_zero_accept_timeout() {
        let raw: RawConfig = toml::from_str("accept_timeout_secs = 0").unwrap();
        assert!(validate(raw).is_err());
    }

    #[test]
    fn malformed_toml_falls_back_to_defaults() {
        // load() itself is exercised indirectly via validate()/RawConfig above;
        // this covers the toml::from_str error path directly.
        let err = toml::from_str::<RawConfig>("placement = [1, 2]").unwrap_err();
        assert!(!err.to_string().is_empty());
    }
}
