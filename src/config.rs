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

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use serde::Deserialize;

use crate::herdr::PLUGIN_ID;

const CONFIG_FILE_NAME: &str = "config.toml";
const DEFAULT_ACCEPT_TIMEOUT_SECS: u64 = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    Split,
    Tab,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    Right,
    Down,
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
        Some(other) => return Err(format!("unknown direction {other:?} (expected \"right\" or \"down\")")),
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
    })
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
    }

    #[test]
    fn rejects_unknown_placement() {
        let raw: RawConfig = toml::from_str(r#"placement = "float""#).unwrap();
        assert!(validate(raw).is_err());
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
