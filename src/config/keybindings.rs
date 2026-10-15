//! Bounded keyboard settings shared by the input editor and app shortcuts.

use std::collections::{BTreeMap, HashSet};
use std::sync::OnceLock;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::{Deserialize, Deserializer};

pub type KeyOverrides = BTreeMap<String, Vec<String>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KeyAction {
    LineStart,
    LineEnd,
    Left,
    Right,
    Up,
    Down,
    WordLeft,
    WordRight,
    DeleteBackward,
    DeleteForward,
    DeleteWordBackward,
    DeleteWordForward,
    KillBefore,
    KillAfter,
    Yank,
    YankPrevious,
    Newline,
    Submit,
    Complete,
    Undo,
    Interrupt,
    Eof,
    CycleMode,
    CycleReasoning,
    PageUp,
    PageDown,
    TranscriptTop,
    TranscriptBottom,
    OpenEditor,
    Lazygit,
    CopySelection,
    PasteClipboard,
    ToggleVim,
    ShowKeys,
}

#[derive(Clone, Copy)]
struct ActionSpec {
    action: KeyAction,
    name: &'static str,
    defaults: &'static [&'static str],
    editor: bool,
    windows_only_default: bool,
}

macro_rules! editor {
    ($action:ident, $name:literal, $($key:literal),* $(,)?) => {
        ActionSpec { action: KeyAction::$action, name: $name, defaults: &[$($key),*], editor: true, windows_only_default: false }
    };
}
macro_rules! app {
    ($action:ident, $name:literal, $($key:literal),* $(,)?) => {
        ActionSpec { action: KeyAction::$action, name: $name, defaults: &[$($key),*], editor: false, windows_only_default: false }
    };
}

const SPECS: &[ActionSpec] = &[
    editor!(LineStart, "line_start", "ctrl+a", "home"),
    editor!(LineEnd, "line_end", "ctrl+e", "end"),
    editor!(Left, "left", "ctrl+b", "left"),
    editor!(Right, "right", "ctrl+f", "right"),
    editor!(Up, "up", "ctrl+p", "up"),
    editor!(Down, "down", "ctrl+n", "down"),
    editor!(WordLeft, "word_left", "ctrl+left", "alt+b", "alt+left"),
    editor!(WordRight, "word_right", "ctrl+right", "alt+f", "alt+right"),
    editor!(DeleteBackward, "delete_backward", "backspace", "ctrl+h"),
    editor!(DeleteForward, "delete_forward", "delete", "ctrl+d"),
    editor!(
        DeleteWordBackward,
        "delete_word_backward",
        "ctrl+w",
        "alt+backspace"
    ),
    editor!(DeleteWordForward, "delete_word_forward", "alt+d"),
    editor!(KillBefore, "kill_before", "ctrl+u"),
    editor!(KillAfter, "kill_after", "ctrl+k"),
    editor!(Yank, "yank", "ctrl+y"),
    editor!(YankPrevious, "yank_previous", "alt+y"),
    editor!(Newline, "newline", "shift+enter", "alt+enter"),
    editor!(Submit, "submit", "enter"),
    editor!(Complete, "complete", "tab"),
    editor!(Undo, "undo",),
    app!(Interrupt, "interrupt", "ctrl+c"),
    app!(Eof, "eof", "ctrl+d"),
    app!(CycleMode, "cycle_mode", "shift+tab", "backtab"),
    app!(CycleReasoning, "cycle_reasoning", "ctrl+r"),
    app!(PageUp, "page_up", "pageup"),
    app!(PageDown, "page_down", "pagedown"),
    app!(TranscriptTop, "transcript_top", "ctrl+home"),
    app!(TranscriptBottom, "transcript_bottom", "ctrl+end"),
    app!(OpenEditor, "open_editor", "ctrl+g"),
    app!(Lazygit, "lazygit", "ctrl+o", "ctrl+shift+o"),
    ActionSpec {
        action: KeyAction::CopySelection,
        name: "copy_selection",
        defaults: &["ctrl+shift+c"],
        editor: false,
        windows_only_default: true,
    },
    ActionSpec {
        action: KeyAction::PasteClipboard,
        name: "paste_clipboard",
        defaults: &["ctrl+v"],
        editor: false,
        windows_only_default: true,
    },
    app!(ToggleVim, "toggle_vim",),
    app!(ShowKeys, "show_keys",),
];

/// The parsed default chords of each entry of [`SPECS`] (same order), parsed
/// once per process instead of on every key lookup.
fn default_chords() -> &'static [Vec<KeyChord>] {
    static DEFAULTS: OnceLock<Vec<Vec<KeyChord>>> = OnceLock::new();
    DEFAULTS.get_or_init(|| {
        SPECS
            .iter()
            .map(|spec| {
                spec.defaults
                    .iter()
                    .map(|text| KeyChord::parse(text).expect("static key chord"))
                    .collect()
            })
            .collect()
    })
}

/// Every spec paired with its parsed default chords.
fn specs_with_defaults() -> impl Iterator<Item = (&'static ActionSpec, &'static [KeyChord])> {
    SPECS.iter().zip(default_chords().iter().map(Vec::as_slice))
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct KeyChord {
    code: KeyCode,
    modifiers: KeyModifiers,
}

impl KeyChord {
    pub(crate) fn parse(text: &str) -> Result<Self, &'static str> {
        if text.is_empty() || text.len() > 40 || text.chars().any(char::is_control) {
            return Err("key chord is empty, too long, or contains control characters");
        }
        let mut modifiers = KeyModifiers::NONE;
        let mut parts = text.split('+').peekable();
        while let Some(part) = parts.next() {
            if parts.peek().is_some() {
                let modifier = match part.to_ascii_lowercase().as_str() {
                    "ctrl" | "control" => KeyModifiers::CONTROL,
                    "alt" | "meta" => KeyModifiers::ALT,
                    "shift" => KeyModifiers::SHIFT,
                    "super" => KeyModifiers::SUPER,
                    _ => return Err("unknown key modifier"),
                };
                if modifiers.contains(modifier) {
                    return Err("duplicate key modifier");
                }
                modifiers.insert(modifier);
                continue;
            }
            let lower = part.to_ascii_lowercase();
            let code = match lower.as_str() {
                "enter" | "return" => KeyCode::Enter,
                "esc" | "escape" => KeyCode::Esc,
                "backspace" => KeyCode::Backspace,
                "delete" | "del" => KeyCode::Delete,
                "tab" if modifiers.contains(KeyModifiers::SHIFT) => KeyCode::BackTab,
                "tab" => KeyCode::Tab,
                "backtab" => KeyCode::BackTab,
                "left" => KeyCode::Left,
                "right" => KeyCode::Right,
                "up" => KeyCode::Up,
                "down" => KeyCode::Down,
                "home" => KeyCode::Home,
                "end" => KeyCode::End,
                "pageup" => KeyCode::PageUp,
                "pagedown" => KeyCode::PageDown,
                "space" => KeyCode::Char(' '),
                "plus" => KeyCode::Char('+'),
                _ if lower.starts_with('f') && lower.len() > 1 => {
                    let number = lower[1..].parse::<u8>().map_err(|_| "unknown key name")?;
                    if !(1..=24).contains(&number) {
                        return Err("function key is outside F1..F24");
                    }
                    KeyCode::F(number)
                }
                _ => {
                    let mut chars = part.chars();
                    let c = chars.next().ok_or("unknown key name")?;
                    if chars.next().is_some() {
                        return Err("unknown key name");
                    }
                    KeyCode::Char(c.to_ascii_lowercase())
                }
            };
            return Ok(Self { code, modifiers });
        }
        Err("key chord has no key")
    }

    fn matches(self, key: KeyEvent) -> bool {
        let code = match key.code {
            KeyCode::Char(c) => KeyCode::Char(c.to_ascii_lowercase()),
            other => other,
        };
        self.code == code && self.modifiers == key.modifiers
    }

    fn event(self) -> KeyEvent {
        KeyEvent::new(self.code, self.modifiers)
    }
}

#[derive(Clone, Debug)]
#[cfg_attr(not(windows), derive(Default))]
pub(crate) struct KeyBindings {
    overrides: Vec<(KeyAction, Vec<KeyChord>)>,
    native_clipboard: bool,
}

#[cfg(windows)]
impl Default for KeyBindings {
    fn default() -> Self {
        Self {
            overrides: Vec::new(),
            native_clipboard: true,
        }
    }
}

impl KeyBindings {
    fn default_available(&self, spec: &ActionSpec) -> bool {
        !spec.windows_only_default
            || self.native_clipboard
            || (cfg!(feature = "multimodal") && spec.action == KeyAction::PasteClipboard)
    }
    pub(crate) fn from_overrides(settings: Option<&KeyOverrides>) -> Result<Self, &'static str> {
        let mut result = Self::default();
        let Some(settings) = settings else {
            return Ok(result);
        };
        if settings.len() > SPECS.len() {
            return Err("too many key actions");
        }
        let mut assigned = HashSet::new();
        for (name, keys) in settings {
            let spec = SPECS
                .iter()
                .find(|s| s.name == name)
                .ok_or("unknown key action")?;
            if keys.len() > 8 {
                return Err("too many chords for one key action");
            }
            let mut parsed = Vec::with_capacity(keys.len());
            for text in keys {
                let chord = KeyChord::parse(text)?;
                // Editor/global overlap exists in defaults (Ctrl+D); explicit
                // overrides must be unambiguous instead of silently losing.
                if !assigned.insert(chord) {
                    return Err("key chord is assigned more than once");
                }
                parsed.push(chord);
            }
            result.overrides.push((spec.action, parsed));
        }
        Ok(result)
    }

    pub(crate) fn action(&self, key: KeyEvent, editor: bool) -> Option<KeyAction> {
        for (action, chords) in &self.overrides {
            if chords.iter().any(|chord| chord.matches(key)) {
                return SPECS
                    .iter()
                    .any(|s| s.action == *action && s.editor == editor)
                    .then_some(*action);
            }
        }
        let key = legacy_default_event(key, editor, self.native_clipboard)?;
        specs_with_defaults()
            .filter(|(spec, _)| spec.editor == editor)
            .filter(|(spec, _)| self.default_available(spec))
            .filter(|(spec, _)| {
                !self
                    .overrides
                    .iter()
                    .any(|(action, _)| *action == spec.action)
            })
            .find(|(_, chords)| chords.iter().any(|chord| chord.matches(key)))
            .map(|(spec, _)| spec.action)
    }

    pub(crate) fn removed_default(&self, key: KeyEvent, editor: bool) -> bool {
        if self.overrides.is_empty() {
            return false;
        }
        let Some(key) = legacy_default_event(key, editor, self.native_clipboard) else {
            return false;
        };
        specs_with_defaults()
            .filter(|(spec, _)| spec.editor == editor)
            .filter(|(spec, _)| self.default_available(spec))
            .any(|(spec, chords)| {
                self.overrides
                    .iter()
                    .any(|(action, _)| *action == spec.action)
                    && chords.iter().any(|chord| chord.matches(key))
            })
    }

    /// Whether configuration changed what `key` does: it is an explicit
    /// binding, or a default (global or editor) chord whose action was
    /// rebound. Such keys must not fall back to their raw meaning.
    pub(crate) fn is_reconfigured(&self, key: KeyEvent) -> bool {
        !self.overrides.is_empty()
            && (self.is_explicit_binding(key)
                || self.removed_default(key, false)
                || self.removed_default(key, true))
    }

    pub(crate) fn effective_editor_event(&self, key: KeyEvent) -> Option<KeyEvent> {
        for (action, chords) in &self.overrides {
            if chords.iter().any(|chord| chord.matches(key)) {
                return SPECS
                    .iter()
                    .any(|spec| spec.action == *action && spec.editor)
                    .then(|| Self::canonical(*action))
                    .flatten();
            }
        }
        (!self.removed_default(key, true)).then_some(key)
    }

    pub(crate) fn has_override(&self, action: KeyAction) -> bool {
        self.overrides
            .iter()
            .any(|(candidate, chords)| *candidate == action && !chords.is_empty())
    }

    pub(crate) fn is_explicit_binding(&self, key: KeyEvent) -> bool {
        self.overrides
            .iter()
            .any(|(_, chords)| chords.iter().any(|chord| chord.matches(key)))
    }

    pub(crate) fn canonical(action: KeyAction) -> Option<KeyEvent> {
        specs_with_defaults()
            .find(|(spec, _)| spec.action == action)?
            .1
            .first()
            .map(|chord| chord.event())
    }

    pub(crate) fn describe(&self) -> Vec<String> {
        SPECS
            .iter()
            .map(|spec| {
                let keys = if let Some((_, chords)) = self
                    .overrides
                    .iter()
                    .find(|(action, _)| *action == spec.action)
                {
                    chords
                        .iter()
                        .map(|chord| chord.describe())
                        .collect::<Vec<_>>()
                } else if !self.default_available(spec) {
                    Vec::new()
                } else {
                    let index = SPECS
                        .iter()
                        .position(|candidate| candidate.action == spec.action)
                        .expect("spec from SPECS");
                    spec.defaults
                        .iter()
                        .zip(&default_chords()[index])
                        .filter(|(_, chord)| {
                            !self
                                .overrides
                                .iter()
                                .any(|(_, chords)| chords.contains(chord))
                        })
                        .map(|(text, _)| (*text).to_owned())
                        .collect()
                };
                format!(
                    "{}: {}",
                    spec.name,
                    if keys.is_empty() {
                        "unbound".into()
                    } else {
                        keys.join(", ")
                    }
                )
            })
            .collect()
    }
}

impl KeyChord {
    fn describe(self) -> String {
        let mut parts = Vec::new();
        for (modifier, name) in [
            (KeyModifiers::CONTROL, "ctrl"),
            (KeyModifiers::ALT, "alt"),
            (KeyModifiers::SHIFT, "shift"),
            (KeyModifiers::SUPER, "super"),
        ] {
            if self.modifiers.contains(modifier) {
                parts.push(name.to_owned());
            }
        }
        parts.push(match self.code {
            KeyCode::Char(' ') => "space".into(),
            KeyCode::Char('+') => "plus".into(),
            KeyCode::Char(c) => c.to_string(),
            KeyCode::F(n) => format!("f{n}"),
            KeyCode::Enter => "enter".into(),
            KeyCode::Esc => "esc".into(),
            KeyCode::Backspace => "backspace".into(),
            KeyCode::Delete => "delete".into(),
            KeyCode::Tab => "tab".into(),
            KeyCode::BackTab => "backtab".into(),
            KeyCode::Left => "left".into(),
            KeyCode::Right => "right".into(),
            KeyCode::Up => "up".into(),
            KeyCode::Down => "down".into(),
            KeyCode::Home => "home".into(),
            KeyCode::End => "end".into(),
            KeyCode::PageUp => "pageup".into(),
            KeyCode::PageDown => "pagedown".into(),
            _ => unreachable!("validated chord code"),
        });
        parts.join("+")
    }
}

/// Preserve the modifiers accepted by the existing handlers for default keys.
/// Explicit configured chords use exact modifiers instead.
fn legacy_default_event(key: KeyEvent, editor: bool, windows: bool) -> Option<KeyEvent> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let event = |code, modifiers| Some(KeyEvent::new(code, modifiers));
    if !editor {
        if windows
            && matches!(key.code, KeyCode::Char('c'))
            && key.modifiers == (KeyModifiers::CONTROL | KeyModifiers::SHIFT)
        {
            return Some(key);
        }
        if (windows || cfg!(feature = "multimodal"))
            && key.code == KeyCode::Char('v')
            && key.modifiers == KeyModifiers::CONTROL
        {
            return Some(key);
        }
        return match key.code {
            KeyCode::Char(c @ ('c' | 'd' | 'r' | 'g')) if ctrl => {
                event(KeyCode::Char(c), KeyModifiers::CONTROL)
            }
            KeyCode::Char('o' | 'O') if ctrl && !alt => {
                event(KeyCode::Char('o'), KeyModifiers::CONTROL)
            }
            code @ (KeyCode::PageUp | KeyCode::PageDown) => event(code, KeyModifiers::NONE),
            code @ (KeyCode::Home | KeyCode::End) if ctrl => event(code, KeyModifiers::CONTROL),
            KeyCode::BackTab
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                Some(key)
            }
            _ => None,
        };
    }
    match key.code {
        KeyCode::Char(
            c @ ('a' | 'e' | 'b' | 'f' | 'p' | 'n' | 'w' | 'u' | 'k' | 'd' | 'y' | 'h'),
        ) if ctrl => return event(KeyCode::Char(c), KeyModifiers::CONTROL),
        code @ (KeyCode::Left | KeyCode::Right) if ctrl && !alt => {
            return event(code, KeyModifiers::CONTROL);
        }
        _ => {}
    }
    if alt
        && let code @ (KeyCode::Char('b' | 'f' | 'd' | 'y')
        | KeyCode::Left
        | KeyCode::Right
        | KeyCode::Backspace) = key.code
    {
        return event(code, KeyModifiers::ALT);
    }
    if matches!(key.code, KeyCode::Char(c) if ctrl != alt && !(ctrl && matches!(c, 'h'|'H'))) {
        return None;
    }
    match key.code {
        KeyCode::Enter
            if key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
        {
            event(KeyCode::Enter, KeyModifiers::SHIFT)
        }
        KeyCode::Char('\u{8}') => event(KeyCode::Backspace, KeyModifiers::NONE),
        code @ (KeyCode::Enter
        | KeyCode::Backspace
        | KeyCode::Delete
        | KeyCode::Left
        | KeyCode::Right
        | KeyCode::Up
        | KeyCode::Down
        | KeyCode::Home
        | KeyCode::End
        | KeyCode::Tab) => event(code, KeyModifiers::NONE),
        _ => None,
    }
}

pub(crate) fn deserialize_keys<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<KeyOverrides>, D::Error> {
    let settings = Option::<KeyOverrides>::deserialize(deserializer)?;
    KeyBindings::from_overrides(settings.as_ref()).map_err(serde::de::Error::custom)?;
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preparsed_default_chords_match_the_spec_table() {
        let parsed = default_chords();
        assert_eq!(parsed.len(), SPECS.len());
        for (spec, chords) in SPECS.iter().zip(parsed) {
            let expected: Vec<KeyChord> = spec
                .defaults
                .iter()
                .map(|text| KeyChord::parse(text).unwrap())
                .collect();
            assert_eq!(chords, &expected, "{}", spec.name);
        }
        assert!(std::ptr::eq(default_chords(), parsed), "parsed once");
    }

    #[test]
    fn reconfigured_keys_cover_explicit_and_removed_defaults_only() {
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        let defaults = KeyBindings::default();
        assert!(!defaults.is_reconfigured(ctrl('a')));
        assert!(!defaults.is_reconfigured(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)));
        let mut settings = KeyOverrides::new();
        // Editor action moved: its old default is removed, the new chord is
        // explicit, and unrelated keys keep their raw meaning.
        settings.insert("line_start".into(), vec!["ctrl+t".into()]);
        // Global action moved.
        settings.insert("page_up".into(), vec!["ctrl+shift+u".into()]);
        let bindings = KeyBindings::from_overrides(Some(&settings)).unwrap();
        for key in [
            ctrl('a'),
            ctrl('t'),
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
            KeyEvent::new(
                KeyCode::Char('u'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
        ] {
            assert_eq!(
                bindings.is_reconfigured(key),
                bindings.is_explicit_binding(key)
                    || bindings.removed_default(key, false)
                    || bindings.removed_default(key, true),
                "{key:?}"
            );
            assert!(bindings.is_reconfigured(key), "{key:?}");
        }
        assert!(!bindings.is_reconfigured(ctrl('e')));
        assert_eq!(bindings.action(ctrl('e'), true), Some(KeyAction::LineEnd));
        assert_eq!(
            KeyBindings::canonical(KeyAction::LineStart),
            Some(ctrl('a'))
        );
    }

    #[test]
    fn keys73_literal_chords_are_bounded_and_normalized() {
        assert!(
            KeyChord::parse("CTRL+Shift+雪")
                .unwrap()
                .matches(KeyEvent::new(
                    KeyCode::Char('雪'),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT
                ))
        );
        assert!(
            KeyChord::parse("ctrl+f12")
                .unwrap()
                .matches(KeyEvent::new(KeyCode::F(12), KeyModifiers::CONTROL))
        );
        for bad in [
            "",
            "ctrl+",
            "ctrl+ctrl+a",
            "unknown+a",
            "f25",
            "\x1b",
            "ctrl+not-a-key",
        ] {
            assert!(KeyChord::parse(bad).is_err());
        }
        assert!(KeyChord::parse(&"x".repeat(41)).is_err());
    }

    #[test]
    fn keys73_rebinding_replaces_old_action_and_steals_default_alias() {
        let settings = BTreeMap::from([
            ("line_start".into(), vec!["ctrl+e".into()]),
            ("submit".into(), vec![]),
        ]);
        let bindings = KeyBindings::from_overrides(Some(&settings)).unwrap();
        assert_eq!(
            bindings.action(
                KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
                true
            ),
            Some(KeyAction::LineStart)
        );
        assert!(bindings.removed_default(
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
            true
        ));
        assert_eq!(
            bindings.action(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), true),
            None
        );
        assert!(bindings.removed_default(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), true));
        assert!(
            KeyBindings::from_overrides(Some(&BTreeMap::from([
                ("submit".into(), vec!["ctrl+a".into()]),
                ("interrupt".into(), vec!["ctrl+a".into()])
            ])))
            .is_err()
        );
    }

    #[test]
    fn clipboard74_default_and_override_keep_exact_paste_modifiers_and_feature_boundary() {
        let defaults = KeyBindings::default();
        let paste = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL);
        assert_eq!(
            defaults.action(paste, false),
            (cfg!(windows) || cfg!(feature = "multimodal")).then_some(KeyAction::PasteClipboard)
        );
        assert_eq!(
            defaults.action(
                KeyEvent::new(
                    KeyCode::Char('v'),
                    KeyModifiers::CONTROL | KeyModifiers::ALT
                ),
                false
            ),
            None
        );
        let configured = KeyBindings::from_overrides(Some(&BTreeMap::from([(
            "paste_clipboard".into(),
            vec!["f6".into()],
        )])))
        .unwrap();
        assert_eq!(
            configured.action(KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE), false),
            Some(KeyAction::PasteClipboard)
        );
        assert_eq!(configured.action(paste, false), None);
    }

    #[test]
    fn keys73_settings_reject_unknown_actions_without_echoing_values() {
        assert_eq!(
            KeyBindings::from_overrides(Some(&BTreeMap::from([(
                "PRIVATE_KEY_NAME".into(),
                vec!["PRIVATE_VALUE".into()]
            )])))
            .unwrap_err(),
            "unknown key action"
        );
        let defaults = KeyBindings::default();
        assert_eq!(
            defaults.action(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT), false),
            Some(KeyAction::CycleMode)
        );
        assert_eq!(
            defaults.action(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                false
            ),
            Some(KeyAction::Interrupt)
        );
        assert!(
            defaults
                .describe()
                .iter()
                .any(|line| line == "submit: enter")
        );
    }
}
