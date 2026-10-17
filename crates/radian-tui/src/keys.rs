//! The rebindable keymap (config key `tui.keybindings`).
//!
//! Actions are the names in the README's key table; their built-in keys are exactly today's
//! behavior, so an empty `tui.keybindings` changes nothing (the existing pty tests are the
//! proof). A listed action's keys **replace** its defaults. An unknown action or an
//! unparsable key string is skipped with a warning; one key bound to two actions is a startup
//! error (rather than guessing a priority).
//!
//! The approval and command-confirmation prompts (`y`/`a`/`n`/`Esc`) are deliberately **not**
//! in this map: they cannot be rebound, because a misconfiguration there could approve a tool
//! call the user meant to deny.

use std::fmt;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// One semantic action the UI can perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Send,
    Newline,
    CursorLeft,
    CursorRight,
    WordLeft,
    WordRight,
    LineStart,
    LineEnd,
    HistoryPrev,
    HistoryNext,
    /// Move the completion menu's highlight down. It only does anything
    /// while a completion menu is up; otherwise the key is inert.
    MenuNext,
    DeleteBefore,
    DeleteUnder,
    DeleteWord,
    DeleteToLineStart,
    DeleteToLineEnd,
    ScrollPageUp,
    ScrollPageDown,
    ScrollLineUp,
    ScrollLineDown,
    ScrollTop,
    ScrollBottom,
    Escape,
    Cancel,
    /// Fold or unfold the startup information. Default `Ctrl-P`.
    ToggleStartup,
    /// Complete the `@` file reference at the cursor (Phase 4).
    Complete,
    /// Open the in-transcript search box (Phase 5); when the box is already open, the earlier
    /// match — that is the historical `Ctrl-F` behavior.
    Find,
    /// The earlier match (the find box is open).
    FindPrev,
    /// The later match (the find box is open).
    FindNext,
    /// Close the find box (the find box is open).
    FindClose,
    /// Collapse or expand the reasoning blocks (ADR 0028 R3, pi's Ctrl-T).
    ToggleReasoning,
    /// Collapse or expand the tool card bodies (ADR 0028 R4, pi's Ctrl-O).
    ToggleToolOutput,
    /// The question form (ADR 0024 Q7): the next question, or the overview from the last one.
    FormNext,
    /// The previous question.
    FormPrev,
    /// Move the option cursor up.
    FormUp,
    /// Move the option cursor down.
    FormDown,
    /// Select the highlighted option (toggle it for a `multi` question).
    FormSelect,
    /// On the last question: the overview. On the overview: submit.
    FormSubmit,
    /// Decline the whole request (asked once: the second press declines).
    FormDecline,
}

/// The interaction layer a binding lives in. The find box is a layer over the transcript: while
/// it is open its keys are looked up **first**, so `enter` can be the earlier match there and
/// the send key in the transcript without either being a conflict. A binding only ever
/// conflicts with another binding in its own layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Context {
    /// The message box and the transcript.
    Main,
    /// The in-transcript search box.
    Find,
    /// The structured-question form (ADR 0024 Q7). Its keys are rebindable; the approval and
    /// confirmation keys are not.
    Form,
}

impl Action {
    /// The action's config/README name.
    pub fn name(self) -> &'static str {
        match self {
            Action::Send => "send",
            Action::Newline => "newline",
            Action::CursorLeft => "cursor_left",
            Action::CursorRight => "cursor_right",
            Action::WordLeft => "word_left",
            Action::WordRight => "word_right",
            Action::LineStart => "line_start",
            Action::LineEnd => "line_end",
            Action::HistoryPrev => "history_prev",
            Action::HistoryNext => "history_next",
            Action::MenuNext => "menu_next",
            Action::DeleteBefore => "delete_before",
            Action::DeleteUnder => "delete_under",
            Action::DeleteWord => "delete_word",
            Action::DeleteToLineStart => "delete_to_line_start",
            Action::DeleteToLineEnd => "delete_to_line_end",
            Action::ScrollPageUp => "scroll_page_up",
            Action::ScrollPageDown => "scroll_page_down",
            Action::ScrollLineUp => "scroll_line_up",
            Action::ScrollLineDown => "scroll_line_down",
            Action::ScrollTop => "scroll_top",
            Action::ScrollBottom => "scroll_bottom",
            Action::Escape => "escape",
            Action::Cancel => "cancel",
            Action::ToggleStartup => "toggle_startup",
            Action::Complete => "complete",
            Action::Find => "find",
            Action::FindPrev => "find_prev",
            Action::FindNext => "find_next",
            Action::FindClose => "find_close",
            Action::ToggleReasoning => "toggle_reasoning",
            Action::ToggleToolOutput => "toggle_tool_output",
            Action::FormNext => "form_next",
            Action::FormPrev => "form_prev",
            Action::FormUp => "form_up",
            Action::FormDown => "form_down",
            Action::FormSelect => "form_select",
            Action::FormSubmit => "form_submit",
            Action::FormDecline => "form_decline",
        }
    }

    /// The layer this action's keys live in.
    pub fn context(self) -> Context {
        match self {
            Action::FindPrev | Action::FindNext | Action::FindClose => Context::Find,
            Action::FormNext
            | Action::FormPrev
            | Action::FormUp
            | Action::FormDown
            | Action::FormSelect
            | Action::FormSubmit
            | Action::FormDecline => Context::Form,
            _ => Context::Main,
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        DEFAULT_BINDINGS
            .iter()
            .find(|(candidate, _)| *candidate == name)
            .map(|(_, action)| *action)
    }
}

/// The built-in bindings: exactly today's key behavior, action by action.
const DEFAULT_BINDINGS: &[(&str, Action)] = &[
    ("send", Action::Send),
    ("newline", Action::Newline),
    ("cursor_left", Action::CursorLeft),
    ("cursor_right", Action::CursorRight),
    ("word_left", Action::WordLeft),
    ("word_right", Action::WordRight),
    ("line_start", Action::LineStart),
    ("line_end", Action::LineEnd),
    ("history_prev", Action::HistoryPrev),
    ("history_next", Action::HistoryNext),
    ("menu_next", Action::MenuNext),
    ("delete_before", Action::DeleteBefore),
    ("delete_under", Action::DeleteUnder),
    ("delete_word", Action::DeleteWord),
    ("delete_to_line_start", Action::DeleteToLineStart),
    ("delete_to_line_end", Action::DeleteToLineEnd),
    ("scroll_page_up", Action::ScrollPageUp),
    ("scroll_page_down", Action::ScrollPageDown),
    ("scroll_line_up", Action::ScrollLineUp),
    ("scroll_line_down", Action::ScrollLineDown),
    ("scroll_top", Action::ScrollTop),
    ("scroll_bottom", Action::ScrollBottom),
    ("escape", Action::Escape),
    ("cancel", Action::Cancel),
    ("toggle_startup", Action::ToggleStartup),
    ("complete", Action::Complete),
    ("find", Action::Find),
    ("find_prev", Action::FindPrev),
    ("find_next", Action::FindNext),
    ("find_close", Action::FindClose),
    ("form_next", Action::FormNext),
    ("form_prev", Action::FormPrev),
    ("form_up", Action::FormUp),
    ("form_down", Action::FormDown),
    ("form_select", Action::FormSelect),
    ("form_submit", Action::FormSubmit),
    ("form_decline", Action::FormDecline),
    ("toggle_reasoning", Action::ToggleReasoning),
    ("toggle_tool_output", Action::ToggleToolOutput),
];

/// The default key strings per action.
const DEFAULT_KEYS: &[(&str, &[&str])] = &[
    ("send", &["enter"]),
    ("newline", &["ctrl-j", "alt-enter", "shift-enter"]),
    ("cursor_left", &["left"]),
    ("cursor_right", &["right"]),
    ("word_left", &["ctrl-left", "alt-left"]),
    ("word_right", &["ctrl-right", "alt-right"]),
    ("line_start", &["home", "ctrl-a"]),
    ("line_end", &["end", "ctrl-e"]),
    ("history_prev", &["up"]),
    ("history_next", &["down"]),
    // Ctrl-P is `toggle_startup`, so the menu's down key is Ctrl-N.
    ("menu_next", &["ctrl-n"]),
    ("delete_before", &["backspace"]),
    ("delete_under", &["delete"]),
    ("delete_word", &["ctrl-w"]),
    ("delete_to_line_start", &["ctrl-u"]),
    ("delete_to_line_end", &["ctrl-k"]),
    ("scroll_page_up", &["pageup"]),
    ("scroll_page_down", &["pagedown"]),
    ("scroll_line_up", &["shift-up"]),
    ("scroll_line_down", &["shift-down"]),
    ("scroll_top", &["ctrl-home"]),
    ("scroll_bottom", &["ctrl-end"]),
    ("escape", &["esc"]),
    ("cancel", &["ctrl-c"]),
    ("toggle_startup", &["ctrl-p"]),
    ("complete", &["tab"]),
    ("find", &["ctrl-f"]),
    ("toggle_reasoning", &["ctrl-t"]),
    ("toggle_tool_output", &["ctrl-o"]),
];

/// The find box's default keys. These are looked up before the transcript layer while the box
/// is open. `ctrl-f` is both "open the box" (transcript) and "earlier match" (box), exactly as
/// the hardcoded behavior was; `enter` is the earlier match here and the send key in the
/// transcript.
/// The question form's default keys (ADR 0024 Q7). Unlike the approval's, these are in the
/// keymap and can be rebound.
const FORM_DEFAULT_KEYS: &[(&str, &[&str])] = &[
    ("form_next", &["tab"]),
    ("form_prev", &["backtab", "shift-tab"]),
    ("form_up", &["up"]),
    ("form_down", &["down"]),
    ("form_select", &["space"]),
    ("form_submit", &["enter"]),
    ("form_decline", &["esc"]),
];

const FIND_DEFAULT_KEYS: &[(&str, &[&str])] = &[
    ("find_prev", &["enter", "ctrl-f"]),
    ("find_next", &["shift-enter", "ctrl-g"]),
    ("find_close", &["esc"]),
];

/// One key and its modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyPress {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl KeyPress {
    /// Parses `[ctrl-][alt-][shift-]key`, e.g. `enter`, `ctrl-j`, `alt-enter`, `shift-up`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut modifiers = KeyModifiers::NONE;
        let mut rest = text;
        while let Some((head, tail)) = rest.split_once('-') {
            match head.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => modifiers |= KeyModifiers::CONTROL,
                "alt" | "meta" => modifiers |= KeyModifiers::ALT,
                "shift" => modifiers |= KeyModifiers::SHIFT,
                _ => break,
            }
            rest = tail;
        }
        Ok(KeyPress {
            code: parse_code(rest)?,
            modifiers,
        })
    }

    /// Whether this binding is a bare printable character — the case that swallows a character
    /// the composer would otherwise type. A modifier (Ctrl/Alt/Shift) or a non-character key
    /// (arrows, tab, enter, …) is not: those are not characters the input box would have typed.
    fn is_bare_printable_char(&self) -> bool {
        self.modifiers.is_empty() && matches!(self.code, KeyCode::Char(c) if !c.is_control())
    }

    /// Whether a real key event is this binding. A character compares case-insensitively, and
    /// an uppercase character (which already encodes Shift) does not require the Shift flag.
    pub fn matches(&self, key: &KeyEvent) -> bool {
        if !codes_match(&self.code, &key.code) {
            return false;
        }
        let mut modifiers =
            key.modifiers & (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT);
        if matches!(key.code, KeyCode::Char(c) if c.is_uppercase()) {
            modifiers &= !KeyModifiers::SHIFT;
        }
        self.modifiers == modifiers
    }
}

impl fmt::Display for KeyPress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            f.write_str("ctrl-")?;
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            f.write_str("alt-")?;
        }
        if self.modifiers.contains(KeyModifiers::SHIFT) {
            f.write_str("shift-")?;
        }
        f.write_str(&code_name(&self.code))
    }
}

/// The config spelling of a key code (the inverse of [`parse_code`]).
fn code_name(code: &KeyCode) -> String {
    match code {
        KeyCode::Char(' ') => "space".to_owned(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "enter".to_owned(),
        KeyCode::Esc => "esc".to_owned(),
        KeyCode::Tab => "tab".to_owned(),
        KeyCode::BackTab => "backtab".to_owned(),
        KeyCode::Backspace => "backspace".to_owned(),
        KeyCode::Delete => "delete".to_owned(),
        KeyCode::Insert => "insert".to_owned(),
        KeyCode::Up => "up".to_owned(),
        KeyCode::Down => "down".to_owned(),
        KeyCode::Left => "left".to_owned(),
        KeyCode::Right => "right".to_owned(),
        KeyCode::Home => "home".to_owned(),
        KeyCode::End => "end".to_owned(),
        KeyCode::PageUp => "pageup".to_owned(),
        KeyCode::PageDown => "pagedown".to_owned(),
        other => format!("{other:?}"),
    }
}

fn parse_code(name: &str) -> Result<KeyCode, String> {
    let lower = name.to_ascii_lowercase();
    let code = match lower.as_str() {
        "enter" | "return" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "backspace" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "page-up" | "pgup" => KeyCode::PageUp,
        "pagedown" | "page-down" | "pgdn" => KeyCode::PageDown,
        "space" => KeyCode::Char(' '),
        _ => {
            let mut chars = name.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => KeyCode::Char(c.to_ascii_lowercase()),
                _ => return Err(format!("unknown key {name:?}")),
            }
        }
    };
    Ok(code)
}

fn codes_match(left: &KeyCode, right: &KeyCode) -> bool {
    match (left, right) {
        (KeyCode::Char(a), KeyCode::Char(b)) => a.eq_ignore_ascii_case(b),
        _ => left == right,
    }
}

fn lookup(bindings: &[(KeyPress, Action)], key: &KeyEvent) -> Option<Action> {
    bindings
        .iter()
        .find(|(press, _)| press.matches(key))
        .map(|(_, action)| *action)
}

/// One key bound to two actions in the same layer.
fn check_layer(bindings: &[(KeyPress, Action)]) -> Result<(), KeymapError> {
    for (index, (press, action)) in bindings.iter().enumerate() {
        for (other_press, other) in &bindings[index + 1..] {
            if press == other_press && action != other {
                return Err(KeymapError::Conflict {
                    key: press.to_string(),
                    first: action.name().to_owned(),
                    second: other.name().to_owned(),
                });
            }
        }
    }
    Ok(())
}

/// Why a keymap was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeymapError {
    /// One key is bound to two actions; the user must choose.
    Conflict {
        key: String,
        first: String,
        second: String,
    },
}

impl fmt::Display for KeymapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeymapError::Conflict { key, first, second } => write!(
                f,
                "tui.keybindings: `{key}` is bound to both `{first}` and `{second}`; pick one"
            ),
        }
    }
}

impl std::error::Error for KeymapError {}

/// The action for each key.
#[derive(Debug, Clone)]
pub struct Keymap {
    bindings: Vec<(KeyPress, Action)>,
    /// The find box's layer, looked up before `bindings` while the box is open.
    find_bindings: Vec<(KeyPress, Action)>,
    /// The question form's layer (ADR 0024 Q7), looked up while a question modal is on top.
    form_bindings: Vec<(KeyPress, Action)>,
    /// Unknown actions and unparsable keys, for the startup line.
    pub warnings: Vec<String>,
}

impl Default for Keymap {
    fn default() -> Self {
        Self::builtin()
    }
}

impl Keymap {
    /// The built-in map (today's behavior).
    pub fn builtin() -> Self {
        let built = |table: &[(&str, &[&str])]| {
            let mut bindings = Vec::new();
            for (name, keys) in table {
                let action = Action::from_name(name).expect("a built-in action name");
                for key in *keys {
                    bindings.push((KeyPress::parse(key).expect("a built-in key"), action));
                }
            }
            bindings
        };
        Self {
            bindings: built(DEFAULT_KEYS),
            find_bindings: built(FIND_DEFAULT_KEYS),
            form_bindings: built(FORM_DEFAULT_KEYS),
            warnings: Vec::new(),
        }
    }

    /// Builds the map from `tui.keybindings`: each listed action's keys replace its defaults.
    /// An unknown action or an unparsable key is skipped with a warning; a key bound to two
    /// actions is a startup error.
    pub fn from_config(bindings: &[(String, Vec<String>)]) -> Result<Self, KeymapError> {
        let mut map = Self::builtin();
        for (name, keys) in bindings {
            let Some(action) = Action::from_name(name) else {
                map.warnings
                    .push(format!("tui.keybindings: unknown action {name:?} ignored"));
                continue;
            };
            let mut parsed = Vec::new();
            for key in keys {
                match KeyPress::parse(key) {
                    Ok(press) => {
                        // A bare printable character bound in the main view swallows that
                        // character for the composer, so the user must be told (owner
                        // 2026-10-17). The binding is kept — README allows it — it is only
                        // warned about. Never for the find/form layers, whose keys do not
                        // reach the composer, and never for a Ctrl/Alt/Shift binding, which
                        // is not a character the input box would have typed.
                        if action.context() == Context::Main && press.is_bare_printable_char() {
                            map.warnings.push(format!(
                                "tui.keybindings: '{press}' is bound to {} in the main view, so \
                                 it cannot be typed in the input box",
                                action.name()
                            ));
                        }
                        parsed.push(press);
                    }
                    Err(error) => map
                        .warnings
                        .push(format!("tui.keybindings.{name}: {error} ignored")),
                }
            }
            let layer = match action.context() {
                Context::Main => &mut map.bindings,
                Context::Find => &mut map.find_bindings,
                Context::Form => &mut map.form_bindings,
            };
            layer.retain(|(_, bound)| *bound != action);
            for press in parsed {
                layer.push((press, action));
            }
        }
        map.check_conflicts()?;
        Ok(map)
    }

    /// The action a key event performs in the transcript, if any.
    pub fn action_for(&self, key: &KeyEvent) -> Option<Action> {
        lookup(&self.bindings, key)
    }

    /// The action a key event performs while the find box is open, if any. Looked up before
    /// [`Self::action_for`], so the box's layer wins while it is up.
    pub fn find_action_for(&self, key: &KeyEvent) -> Option<Action> {
        lookup(&self.find_bindings, key)
    }

    /// The action a key event performs while a question form is on top (ADR 0024 Q7). Looked
    /// up before [`Self::action_for`], so the form's layer wins while it is up.
    pub fn form_action_for(&self, key: &KeyEvent) -> Option<Action> {
        lookup(&self.form_bindings, key)
    }

    fn check_conflicts(&self) -> Result<(), KeymapError> {
        check_layer(&self.bindings)?;
        check_layer(&self.find_bindings)?;
        check_layer(&self.form_bindings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn the_builtin_map_is_todays_behavior() {
        let map = Keymap::builtin();
        assert_eq!(
            map.action_for(&event(KeyCode::Enter, KeyModifiers::NONE)),
            Some(Action::Send)
        );
        assert_eq!(
            map.action_for(&event(KeyCode::Char('j'), KeyModifiers::CONTROL)),
            Some(Action::Newline)
        );
        assert_eq!(
            map.action_for(&event(KeyCode::Enter, KeyModifiers::SHIFT)),
            Some(Action::Newline)
        );
        assert_eq!(
            map.action_for(&event(KeyCode::Up, KeyModifiers::NONE)),
            Some(Action::HistoryPrev)
        );
        assert_eq!(
            map.action_for(&event(KeyCode::Up, KeyModifiers::SHIFT)),
            Some(Action::ScrollLineUp)
        );
        assert_eq!(
            map.action_for(&event(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            Some(Action::LineStart)
        );
        assert_eq!(
            map.action_for(&event(KeyCode::Char('z'), KeyModifiers::NONE)),
            None
        );
        assert!(map.warnings.is_empty());
    }

    #[test]
    fn key_strings_round_trip() {
        for text in [
            "enter",
            "ctrl-j",
            "alt-enter",
            "shift-up",
            "ctrl-home",
            "pageup",
            "space",
        ] {
            let press = KeyPress::parse(text).unwrap();
            assert_eq!(press.to_string(), text, "{text}");
        }
        assert!(KeyPress::parse("ctrl-unknown").is_err());
        assert!(KeyPress::parse("").is_err());
    }

    #[test]
    fn a_configured_action_replaces_its_default_keys() {
        let map = Keymap::from_config(&[("send".into(), vec!["ctrl-s".into()])]).unwrap();
        assert_eq!(
            map.action_for(&event(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            Some(Action::Send)
        );
        assert_eq!(
            map.action_for(&event(KeyCode::Enter, KeyModifiers::NONE)),
            None,
            "Enter no longer sends"
        );
    }

    #[test]
    fn a_second_action_on_the_same_key_is_a_startup_error() {
        let error = Keymap::from_config(&[("send".into(), vec!["ctrl-j".into()])]).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("ctrl-j"), "{text}");
        assert!(text.contains("send") && text.contains("newline"), "{text}");
    }

    #[test]
    fn unknown_actions_and_keys_are_warnings_not_errors() {
        let map = Keymap::from_config(&[
            ("future".into(), vec!["ctrl-x".into()]),
            ("send".into(), vec!["ctrl-s".into(), "not-a-key".into()]),
        ])
        .unwrap();
        assert_eq!(map.warnings.len(), 2, "{:?}", map.warnings);
        assert!(map.warnings.iter().any(|w| w.contains("future")));
        assert!(map.warnings.iter().any(|w| w.contains("not-a-key")));
        // The valid key in the same list still took effect.
        assert_eq!(
            map.action_for(&event(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            Some(Action::Send)
        );
    }

    #[test]
    fn the_find_box_is_a_second_layer() {
        let map = Keymap::builtin();
        // The transcript: `enter` sends, `ctrl-f` opens the box.
        assert_eq!(
            map.action_for(&event(KeyCode::Enter, KeyModifiers::NONE)),
            Some(Action::Send)
        );
        assert_eq!(
            map.action_for(&event(KeyCode::Char('f'), KeyModifiers::CONTROL)),
            Some(Action::Find)
        );
        // The box: `enter` and `ctrl-f` are the earlier match, `shift-enter`/`ctrl-g` the later
        // one, `esc` closes it.
        for key in [
            event(KeyCode::Enter, KeyModifiers::NONE),
            event(KeyCode::Char('f'), KeyModifiers::CONTROL),
        ] {
            assert_eq!(map.find_action_for(&key), Some(Action::FindPrev), "{key:?}");
        }
        for key in [
            event(KeyCode::Enter, KeyModifiers::SHIFT),
            event(KeyCode::Char('g'), KeyModifiers::CONTROL),
        ] {
            assert_eq!(map.find_action_for(&key), Some(Action::FindNext), "{key:?}");
        }
        assert_eq!(
            map.find_action_for(&event(KeyCode::Esc, KeyModifiers::NONE)),
            Some(Action::FindClose)
        );
        assert_eq!(
            map.find_action_for(&event(KeyCode::Char('z'), KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn a_binding_only_conflicts_with_its_own_layer() {
        // `enter` is `send` in the transcript and `find_prev` in the box: not a conflict.
        let map = Keymap::from_config(&[("find_prev".into(), vec!["enter".into()])]).unwrap();
        assert_eq!(
            map.find_action_for(&event(KeyCode::Enter, KeyModifiers::NONE)),
            Some(Action::FindPrev)
        );
        assert_eq!(
            map.action_for(&event(KeyCode::Enter, KeyModifiers::NONE)),
            Some(Action::Send)
        );
        // Two actions of the same layer still conflict.
        let error = Keymap::from_config(&[("find_next".into(), vec!["enter".into()])]).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("enter"), "{text}");
        assert!(
            text.contains("find_prev") && text.contains("find_next"),
            "{text}"
        );
        // Rebinding a box action replaces its defaults in the box layer only.
        let map = Keymap::from_config(&[("find_close".into(), vec!["ctrl-x".into()])]).unwrap();
        assert_eq!(
            map.find_action_for(&event(KeyCode::Char('x'), KeyModifiers::CONTROL)),
            Some(Action::FindClose)
        );
        assert_eq!(
            map.find_action_for(&event(KeyCode::Esc, KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            map.action_for(&event(KeyCode::Esc, KeyModifiers::NONE)),
            Some(Action::Escape),
            "the transcript's `escape` is untouched"
        );
    }

    #[test]
    fn uppercase_characters_match_without_the_shift_flag() {
        let press = KeyPress::parse("a").unwrap();
        assert!(press.matches(&event(KeyCode::Char('a'), KeyModifiers::NONE)));
        assert!(press.matches(&event(KeyCode::Char('A'), KeyModifiers::SHIFT)));
        assert!(!press.matches(&event(KeyCode::Char('a'), KeyModifiers::CONTROL)));
    }

    /// a printable single character bound in the **main** view swallows that
    /// character for the composer, so startup warns about it. The binding is kept (README allows
    /// it); a Ctrl/Alt/Shift binding, a non-character key, and a character in another layer do
    /// not warn.
    #[test]
    fn a_bare_printable_character_in_the_main_view_warns() {
        let map = Keymap::from_config(&[("toggle_reasoning".into(), vec!["p".into()])]).unwrap();
        assert_eq!(map.warnings.len(), 1, "{:?}", map.warnings);
        let warning = &map.warnings[0];
        assert!(warning.contains("'p'"), "{warning}");
        assert!(warning.contains("toggle_reasoning"), "{warning}");
        assert!(
            warning.contains("cannot be typed in the input box"),
            "{warning}"
        );
        // Kept, not dropped: the warning never changes the binding.
        assert_eq!(
            map.action_for(&event(KeyCode::Char('p'), KeyModifiers::NONE)),
            Some(Action::ToggleReasoning)
        );

        // A modified binding is not a character the input box would have typed.
        for key in ["ctrl-y", "alt-y", "shift-p"] {
            let map =
                Keymap::from_config(&[("toggle_reasoning".into(), vec![key.into()])]).unwrap();
            assert!(map.warnings.is_empty(), "{key}: {:?}", map.warnings);
        }
        // A non-character key does not swallow text either.
        let map =
            Keymap::from_config(&[("toggle_reasoning".into(), vec!["insert".into()])]).unwrap();
        assert!(map.warnings.is_empty(), "{:?}", map.warnings);
        // Only the main view feeds the composer: the find and form layers may take characters.
        for (name, key) in [("find_close", "x"), ("form_select", "x")] {
            let map = Keymap::from_config(&[(name.into(), vec![key.into()])]).unwrap();
            assert!(map.warnings.is_empty(), "{name}: {:?}", map.warnings);
        }
    }
}
