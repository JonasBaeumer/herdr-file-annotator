//! Reviewer-customizable keybindings.
//!
//! Every letter/character key in the review pane is an [`Action`] with a
//! default binding; the `[keys]` table in `config.toml` overrides them by
//! action name (see `docs/configuration.md`). Bindings are single printable
//! characters (case means shift: `"G"` is shift+g) or `ctrl+<letter>`.
//!
//! Deliberately NOT remappable, so every muscle-memory escape hatch keeps
//! working regardless of config: `ctrl+c` (cancel, everywhere), `esc`,
//! `enter`, `tab`, the arrow keys, and `pgup`/`pgdn` (fixed aliases of the
//! movement/pan actions), plus everything typed while a comment or summary
//! box is open.
//!
//! Actions carry a context — the file navigator, the diff pane, or both
//! (global verdict/layout keys and shared movement). Two actions may share
//! a key only if their contexts never overlap, so `open` (files) and, say,
//! a remapped `comment` (diff) can both use `o`, but nothing can shadow a
//! global key.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Which pane a key press is interpreted in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Context {
    Files,
    Diff,
}

const FILES: u8 = 1;
const DIFF: u8 = 2;
const ALL: u8 = FILES | DIFF;

impl Context {
    fn bit(self) -> u8 {
        match self {
            Context::Files => FILES,
            Context::Diff => DIFF,
        }
    }
}

/// Everything a remappable key can do. The handlers in `ui.rs` match on
/// this instead of on raw key codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Approve,
    RequestChanges,
    Cancel,
    Help,
    ToggleFiles,
    Zoom,
    FilesNarrower,
    FilesWider,
    Down,
    Up,
    Top,
    Bottom,
    Open,
    HalfPageDown,
    HalfPageUp,
    NextHunk,
    PrevHunk,
    PanLeft,
    PanRight,
    PanReset,
    Wrap,
    ToFiles,
    Select,
    Comment,
    DeleteAnnotation,
    ToggleView,
    Fold,
    UnfoldAll,
}

/// The single source of truth: action, its `[keys]` name, default binding,
/// and context mask. Order is stable so collision errors are deterministic.
const ACTIONS: &[(Action, &str, Binding, u8)] = &[
    (Action::Approve, "approve", Binding::ch('a'), ALL),
    (Action::RequestChanges, "request_changes", Binding::ch('r'), ALL),
    (Action::Cancel, "cancel", Binding::ch('q'), ALL),
    (Action::Help, "help", Binding::ch('?'), ALL),
    (Action::ToggleFiles, "toggle_files", Binding::ch('b'), ALL),
    (Action::Zoom, "zoom", Binding::ch('z'), ALL),
    (Action::FilesNarrower, "files_narrower", Binding::ch('['), ALL),
    (Action::FilesWider, "files_wider", Binding::ch(']'), ALL),
    (Action::Down, "down", Binding::ch('j'), ALL),
    (Action::Up, "up", Binding::ch('k'), ALL),
    (Action::Top, "top", Binding::ch('g'), ALL),
    (Action::Bottom, "bottom", Binding::ch('G'), ALL),
    (Action::Open, "open", Binding::ch('l'), FILES),
    (Action::HalfPageDown, "half_page_down", Binding::ch('d'), DIFF),
    (Action::HalfPageUp, "half_page_up", Binding::ch('u'), DIFF),
    (Action::NextHunk, "next_hunk", Binding::ch('n'), DIFF),
    (Action::PrevHunk, "prev_hunk", Binding::ch('p'), DIFF),
    (Action::PanLeft, "pan_left", Binding::ch('H'), DIFF),
    (Action::PanRight, "pan_right", Binding::ch('L'), DIFF),
    (Action::PanReset, "pan_reset", Binding::ch('0'), DIFF),
    (Action::Wrap, "wrap", Binding::ch('w'), DIFF),
    (Action::ToFiles, "to_files", Binding::ch('h'), DIFF),
    (Action::Select, "select", Binding::ch('v'), DIFF),
    (Action::Comment, "comment", Binding::ch('c'), DIFF),
    (Action::DeleteAnnotation, "delete_annotation", Binding::ch('x'), DIFF),
    (Action::ToggleView, "toggle_view", Binding::ch('t'), DIFF),
    (Action::Fold, "fold", Binding::ch('f'), DIFF),
    (Action::UnfoldAll, "unfold_all", Binding::ch('F'), DIFF),
];

fn context_mask(action: Action) -> u8 {
    ACTIONS.iter().find(|(a, ..)| *a == action).map(|(_, _, _, m)| *m).unwrap_or(0)
}

/// A concrete key a binding resolves to: a printable character, optionally
/// with ctrl. Named keys (enter, esc, arrows, …) are never `Binding`s —
/// they are reserved, which is what keeps the fixed escape hatches fixed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Binding {
    ch: char,
    ctrl: bool,
}

impl Binding {
    const fn ch(ch: char) -> Self {
        Binding { ch, ctrl: false }
    }

    /// The binding a key event corresponds to, if any. Ctrl combinations
    /// normalize to lowercase (terminals report ctrl+T as ctrl+'t');
    /// plain characters keep their case, so 'G' and 'g' stay distinct.
    fn from_event(key: &KeyEvent) -> Option<Binding> {
        let KeyCode::Char(ch) = key.code else { return None };
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            Some(Binding { ch: ch.to_ascii_lowercase(), ctrl: true })
        } else {
            Some(Binding { ch, ctrl: false })
        }
    }

    fn parse(spec: &str) -> Result<Binding, String> {
        let spec = spec.trim();
        if let Some(rest) = spec.strip_prefix("ctrl+") {
            let mut chars = rest.chars();
            match (chars.next(), chars.next()) {
                (Some(ch), None) if !ch.is_whitespace() => {
                    let ch = ch.to_ascii_lowercase();
                    // Ctrl with punctuation or digits reaches the terminal as
                    // an unrelated control byte (ctrl+[ is esc, ctrl+? is DEL,
                    // ctrl+@ is NUL, ctrl+\ ctrl+] ctrl+^ ctrl+_ are FS..US),
                    // so such a binding could never match a key event.
                    if !ch.is_ascii_lowercase() {
                        return Err(format!(
                            "\"ctrl+{ch}\": ctrl bindings must use a letter"
                        ));
                    }
                    if ch == 'c' {
                        return Err("\"ctrl+c\" is reserved: it always cancels".to_string());
                    }
                    // Two letters alias reserved named keys the same way:
                    // terminals send ctrl+i as tab (0x09) and ctrl+m as
                    // enter (0x0d), never as a ctrl+char event.
                    if let Some(alias) = match ch {
                        'i' => Some("tab"),
                        'm' => Some("enter"),
                        _ => None,
                    } {
                        return Err(format!(
                            "\"ctrl+{ch}\" is reserved: terminals send it as {alias}"
                        ));
                    }
                    Ok(Binding { ch, ctrl: true })
                }
                _ => Err(format!("\"{spec}\": expected ctrl+ and exactly one character")),
            }
        } else {
            let mut chars = spec.chars();
            match (chars.next(), chars.next()) {
                (Some(ch), None) if !ch.is_whitespace() => Ok(Binding { ch, ctrl: false }),
                _ => Err(format!(
                    "\"{spec}\": expected a single character or ctrl+<char> \
                     (enter/esc/tab/arrows are reserved)"
                )),
            }
        }
    }

    fn label(&self) -> String {
        if self.ctrl {
            format!("ctrl+{}", self.ch)
        } else {
            self.ch.to_string()
        }
    }
}

/// The active action↔key mapping: defaults plus whatever the `[keys]`
/// config table overrode. Collision-free by construction — `with_overrides`
/// rejects any table where two actions with overlapping contexts share a
/// key, and the config loader falls back to defaults on rejection.
#[derive(Debug, Clone)]
pub struct Keymap {
    files: HashMap<Binding, Action>,
    diff: HashMap<Binding, Action>,
    bindings: HashMap<Action, Binding>,
}

impl Default for Keymap {
    fn default() -> Self {
        Keymap::with_overrides(&HashMap::new()).expect("default bindings are collision-free")
    }
}

impl Keymap {
    pub fn with_overrides(overrides: &HashMap<String, String>) -> Result<Keymap, String> {
        let mut bindings: HashMap<Action, Binding> =
            ACTIONS.iter().map(|&(action, _, default, _)| (action, default)).collect();
        for (name, spec) in overrides {
            let Some(&(action, ..)) = ACTIONS.iter().find(|(_, n, ..)| n == name) else {
                let known: Vec<&str> = ACTIONS.iter().map(|(_, n, ..)| *n).collect();
                return Err(format!("unknown action \"{name}\" (known: {})", known.join(", ")));
            };
            bindings.insert(action, Binding::parse(spec).map_err(|e| format!("{name} = {e}"))?);
        }

        let mut files = HashMap::new();
        let mut diff = HashMap::new();
        for &(action, name, _, mask) in ACTIONS {
            let binding = bindings[&action];
            for (bit, map) in [(FILES, &mut files), (DIFF, &mut diff)] {
                if mask & bit == 0 {
                    continue;
                }
                if let Some(&taken) = map.get(&binding) {
                    let taken_name =
                        ACTIONS.iter().find(|(a, ..)| *a == taken).map(|(_, n, ..)| *n).unwrap();
                    return Err(format!(
                        "\"{}\" is bound to both {taken_name} and {name} in the same view",
                        binding.label()
                    ));
                }
                map.insert(binding, action);
            }
        }
        Ok(Keymap { files, diff, bindings })
    }

    /// The action a key press means in `ctx`, if any. Fixed hardware
    /// aliases (arrows, pgup/pgdn) resolve first and cannot be remapped;
    /// enter/tab/esc are NOT resolved here — their meaning is modal and
    /// stays hardcoded at the call sites.
    pub fn lookup(&self, key: &KeyEvent, ctx: Context) -> Option<Action> {
        let fixed = match (ctx, key.code) {
            (_, KeyCode::Down) => Some(Action::Down),
            (_, KeyCode::Up) => Some(Action::Up),
            (Context::Diff, KeyCode::Right) => Some(Action::PanRight),
            (Context::Diff, KeyCode::Left) => Some(Action::PanLeft),
            (Context::Diff, KeyCode::PageDown) => Some(Action::HalfPageDown),
            (Context::Diff, KeyCode::PageUp) => Some(Action::HalfPageUp),
            _ => None,
        };
        if fixed.is_some() {
            return fixed;
        }
        let binding = Binding::from_event(key)?;
        match ctx {
            Context::Files => self.files.get(&binding),
            Context::Diff => self.diff.get(&binding),
        }
        .copied()
    }

    /// The key label for an action's ACTIVE binding — what the `?` overlay
    /// renders, so a remapped pane documents itself truthfully.
    pub fn label(&self, action: Action) -> String {
        debug_assert!(context_mask(action) != 0, "label for an unmapped action");
        self.bindings.get(&action).map(Binding::label).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)
    }

    fn overrides(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn defaults_resolve_the_documented_keys() {
        let km = Keymap::default();
        assert_eq!(km.lookup(&event('j'), Context::Files), Some(Action::Down));
        assert_eq!(km.lookup(&event('j'), Context::Diff), Some(Action::Down));
        assert_eq!(km.lookup(&event('l'), Context::Files), Some(Action::Open));
        assert_eq!(km.lookup(&event('l'), Context::Diff), None, "open is files-only");
        assert_eq!(km.lookup(&event('G'), Context::Diff), Some(Action::Bottom));
        assert_eq!(km.lookup(&event('g'), Context::Diff), Some(Action::Top), "case matters");
        assert_eq!(km.lookup(&event('a'), Context::Diff), Some(Action::Approve));
        assert_eq!(km.label(Action::Wrap), "w");
    }

    #[test]
    fn arrow_and_page_aliases_are_fixed_and_survive_remaps() {
        let km = Keymap::with_overrides(&overrides(&[("down", "s"), ("pan_right", "e")])).unwrap();
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(km.lookup(&down, Context::Diff), Some(Action::Down));
        let right = KeyEvent::new(KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(km.lookup(&right, Context::Diff), Some(Action::PanRight));
        assert_eq!(km.lookup(&event('s'), Context::Diff), Some(Action::Down));
        assert_eq!(km.lookup(&event('j'), Context::Diff), None, "the old key is released");
    }

    #[test]
    fn ctrl_bindings_parse_and_match_normalized() {
        let km = Keymap::with_overrides(&overrides(&[("approve", "ctrl+A")])).unwrap();
        let ctrl_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(km.lookup(&ctrl_a, Context::Diff), Some(Action::Approve));
        assert_eq!(km.lookup(&event('a'), Context::Diff), None);
        assert_eq!(km.label(Action::Approve), "ctrl+a");
    }

    #[test]
    fn reserved_and_malformed_specs_are_rejected() {
        for (name, spec) in
            [("cancel", "ctrl+c"), ("approve", "enter"), ("approve", ""), ("approve", "ctrl+")]
        {
            let err = Keymap::with_overrides(&overrides(&[(name, spec)]));
            assert!(err.is_err(), "{name} = {spec:?} must be rejected");
        }
        let unknown = Keymap::with_overrides(&overrides(&[("fly", "f")]));
        assert!(unknown.unwrap_err().contains("unknown action \"fly\""));
    }

    #[test]
    fn ctrl_aliases_of_reserved_named_keys_are_rejected() {
        // Terminals encode ctrl+i as tab (0x09), ctrl+m as enter (0x0d) and
        // ctrl+[ as esc (0x1b), so crossterm reports them as the named keys —
        // never as Char events with CONTROL. Accepting them would release the
        // action's default and bind it to a key that can never arrive.
        for spec in [
            "ctrl+i", "ctrl+I", "ctrl+m", "ctrl+M", "ctrl+[", // tab / enter / esc
            "ctrl+?", "ctrl+8", // DEL: crossterm reports Backspace
            "ctrl+@", "ctrl+2", // NUL: crossterm reports ctrl+space
            "ctrl+\\", "ctrl+]", "ctrl+^", "ctrl+_", // FS/GS/RS/US: ctrl+4..7
        ] {
            let err = Keymap::with_overrides(&overrides(&[("approve", spec)]));
            assert!(err.is_err(), "approve = {spec:?} must be rejected");
        }
        // Ctrl+letter combinations stay bindable.
        for spec in ["ctrl+h", "ctrl+j", "ctrl+n"] {
            let ok = Keymap::with_overrides(&overrides(&[("approve", spec)]));
            assert!(ok.is_ok(), "approve = {spec:?} must stay bindable");
        }
    }

    #[test]
    fn collisions_are_rejected_only_when_contexts_overlap() {
        // approve is global: it overlaps diff's comment key.
        let clash = Keymap::with_overrides(&overrides(&[("approve", "c")]));
        assert!(clash.unwrap_err().contains("bound to both"));
        // open (files) and comment (diff) never see the same key press.
        let km = Keymap::with_overrides(&overrides(&[("open", "o"), ("comment", "o")])).unwrap();
        assert_eq!(km.lookup(&event('o'), Context::Files), Some(Action::Open));
        assert_eq!(km.lookup(&event('o'), Context::Diff), Some(Action::Comment));
    }
}
