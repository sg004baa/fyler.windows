//! エンジン非依存のキー表記、操作、バインディング解決。

use std::fmt;

use crate::editor::{Key, KeyInput, Modifiers};

/// キーシーケンス(1個以上のキーストローク)。エンジン非依存。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeySequence(pub Vec<KeyInput>);

/// Helpモーダルへ表示する、解決済みkeymapの1操作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpEntry {
    pub command: String,
    pub description: String,
}

/// ユーザーがキーに割り当てられる操作(エンジン非依存の語彙)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorAction {
    /// ディレクトリの開閉、またはファイルを開く。
    Activate,
    /// 親ディレクトリへ移動する。
    NavigateParent,
    /// 選択したディレクトリ内へ移動する。
    NavigateInto,
    /// 隠しファイルの表示を切り替える。
    ToggleHidden,
    /// 対象の折りたたみを閉じる。
    FoldClose,
    /// 対象の折りたたみを開く。
    FoldOpen,
    /// 対象の折りたたみ状態を切り替える。
    FoldToggle,
    /// 対象以下の折りたたみを再帰的に閉じる。
    FoldCloseRecursive,
    /// 対象以下の折りたたみを再帰的に開く。
    FoldOpenRecursive,
    /// すべての折りたたみを閉じる。
    FoldCloseAll,
    /// すべての折りたたみを開く。
    FoldOpenAll,
    /// ファイルpickerを開く。
    FilePicker,
    /// 選択したパスをコピーする。
    YankPath,
    /// アプリを選んで対象を開く。
    OpenWith,
    /// 選択項目を別paneへ移動する。
    TransferMove,
    /// 選択項目を別paneへコピーする。
    TransferCopy,
    /// 左ナビゲーションドックへfocusを移す、またはエディタへ戻す。
    ToggleDockFocus,
    /// ヘルプを表示する。
    Help,
    /// paneを上下に分割する。
    PaneSplitHorizontal,
    /// paneを左右に分割する。
    PaneSplitVertical,
    /// 左のpaneへfocusを移す。
    PaneFocusLeft,
    /// 下のpaneへfocusを移す。
    PaneFocusDown,
    /// 上のpaneへfocusを移す。
    PaneFocusUp,
    /// 右のpaneへfocusを移す。
    PaneFocusRight,
    /// 次のpaneへfocusを移す。
    PaneFocusNext,
    /// 前のpaneへfocusを移す。
    PaneFocusPrevious,
    /// 現在のpaneを閉じる。
    PaneClose,
    /// paneのnavigation historyを1つ戻る。
    HistoryBack,
    /// paneのnavigation historyを1つ進む。
    HistoryForward,
    /// 現在のrootを実FSから明示的に再同期する。
    Refresh,
}

impl EditorAction {
    /// config.tomlのsnake_case名を操作へ変換する。
    pub fn from_config_name(name: &str) -> Option<Self> {
        Some(match name {
            "activate" => Self::Activate,
            "navigate_parent" => Self::NavigateParent,
            "navigate_into" => Self::NavigateInto,
            "toggle_hidden" => Self::ToggleHidden,
            "fold_close" => Self::FoldClose,
            "fold_open" => Self::FoldOpen,
            "fold_toggle" => Self::FoldToggle,
            "fold_close_recursive" => Self::FoldCloseRecursive,
            "fold_open_recursive" => Self::FoldOpenRecursive,
            "fold_close_all" => Self::FoldCloseAll,
            "fold_open_all" => Self::FoldOpenAll,
            "file_picker" => Self::FilePicker,
            "yank_path" => Self::YankPath,
            "open_with" => Self::OpenWith,
            "transfer_move" => Self::TransferMove,
            "transfer_copy" => Self::TransferCopy,
            "toggle_dock_focus" => Self::ToggleDockFocus,
            "help" => Self::Help,
            "pane_split_horizontal" => Self::PaneSplitHorizontal,
            "pane_split_vertical" => Self::PaneSplitVertical,
            "pane_focus_left" => Self::PaneFocusLeft,
            "pane_focus_down" => Self::PaneFocusDown,
            "pane_focus_up" => Self::PaneFocusUp,
            "pane_focus_right" => Self::PaneFocusRight,
            "pane_focus_next" => Self::PaneFocusNext,
            "pane_focus_previous" => Self::PaneFocusPrevious,
            "pane_close" => Self::PaneClose,
            "history_back" => Self::HistoryBack,
            "history_forward" => Self::HistoryForward,
            "refresh" => Self::Refresh,
            _ => return None,
        })
    }

    /// config.tomlで使うsnake_case名を返す。
    pub fn config_name(&self) -> &'static str {
        match self {
            Self::Activate => "activate",
            Self::NavigateParent => "navigate_parent",
            Self::NavigateInto => "navigate_into",
            Self::ToggleHidden => "toggle_hidden",
            Self::FoldClose => "fold_close",
            Self::FoldOpen => "fold_open",
            Self::FoldToggle => "fold_toggle",
            Self::FoldCloseRecursive => "fold_close_recursive",
            Self::FoldOpenRecursive => "fold_open_recursive",
            Self::FoldCloseAll => "fold_close_all",
            Self::FoldOpenAll => "fold_open_all",
            Self::FilePicker => "file_picker",
            Self::YankPath => "yank_path",
            Self::OpenWith => "open_with",
            Self::TransferMove => "transfer_move",
            Self::TransferCopy => "transfer_copy",
            Self::ToggleDockFocus => "toggle_dock_focus",
            Self::Help => "help",
            Self::PaneSplitHorizontal => "pane_split_horizontal",
            Self::PaneSplitVertical => "pane_split_vertical",
            Self::PaneFocusLeft => "pane_focus_left",
            Self::PaneFocusDown => "pane_focus_down",
            Self::PaneFocusUp => "pane_focus_up",
            Self::PaneFocusRight => "pane_focus_right",
            Self::PaneFocusNext => "pane_focus_next",
            Self::PaneFocusPrevious => "pane_focus_previous",
            Self::PaneClose => "pane_close",
            Self::HistoryBack => "history_back",
            Self::HistoryForward => "history_forward",
            Self::Refresh => "refresh",
        }
    }

    /// ヘルプ表示向けの日本語説明を返す。
    pub fn description(&self) -> &'static str {
        match self {
            Self::Activate => "Toggle directory / Open file",
            Self::NavigateParent => "Go to parent directory",
            Self::NavigateInto => "Enter directory",
            Self::ToggleHidden => "Toggle hidden files",
            Self::FoldClose => "Collapse directory",
            Self::FoldOpen => "Expand directory",
            Self::FoldToggle => "Toggle directory fold",
            Self::FoldCloseRecursive => "Collapse recursively",
            Self::FoldOpenRecursive => "Expand recursively",
            Self::FoldCloseAll => "Collapse all",
            Self::FoldOpenAll => "Expand all",
            Self::FilePicker => "Find file",
            Self::YankPath => "Copy path",
            Self::OpenWith => "Open with application",
            Self::TransferMove => "Move to another pane",
            Self::TransferCopy => "Copy to another pane",
            Self::ToggleDockFocus => "Focus navigation dock / Return to editor",
            Self::Help => "Show help",
            Self::PaneSplitHorizontal => "Split pane horizontally",
            Self::PaneSplitVertical => "Split pane vertically",
            Self::PaneFocusLeft => "Focus left pane",
            Self::PaneFocusDown => "Focus pane below",
            Self::PaneFocusUp => "Focus pane above",
            Self::PaneFocusRight => "Focus right pane",
            Self::PaneFocusNext => "Focus next pane",
            Self::PaneFocusPrevious => "Focus previous pane",
            Self::PaneClose => "Close pane",
            Self::HistoryBack => "Go back in navigation history",
            Self::HistoryForward => "Go forward in navigation history",
            Self::Refresh => "Reload the current root from disk",
        }
    }
}

/// 解決済みバインディング(シーケンス → 操作)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyBinding {
    /// 操作を起動する、展開済みのキーシーケンス。
    pub sequence: KeySequence,
    /// シーケンスへ割り当てる操作。
    pub action: EditorAction,
}

/// key表記を解釈できない理由。設定警告へそのまま埋め込める日本語を返す。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeymapError {
    #[error("Key sequence is empty")]
    Empty,
    #[error("Unknown modifier: {0}")]
    UnknownModifier(String),
    #[error("Unknown key name: {0}")]
    UnknownKey(String),
    #[error(
        "Shift cannot be used with a printable character; write the uppercase character directly: {0}"
    )]
    ShiftCharacter(String),
    #[error("Leader is not configured")]
    LeaderUnset,
    #[error("Leader cannot have modifiers")]
    ModifiedLeader,
    #[error("leader must be a single key")]
    LeaderSequence,
    #[error("leader cannot have modifiers")]
    LeaderModified,
    #[error("leader cannot be Leader")]
    RecursiveLeader,
}

/// エンジン非依存表記のキーシーケンスを解釈する。
pub fn parse_key_sequence(
    input: &str,
    leader: Option<KeyInput>,
) -> Result<KeySequence, KeymapError> {
    let tokens = input.split_whitespace().collect::<Vec<_>>();
    if tokens.is_empty() {
        return Err(KeymapError::Empty);
    }
    tokens
        .into_iter()
        .map(|token| parse_stroke(token, leader))
        .collect::<Result<Vec<_>, _>>()
        .map(KeySequence)
}

/// leader指定を単一・無修飾キーとして解釈する。
pub fn parse_leader(input: &str) -> Result<KeyInput, KeymapError> {
    let tokens = input.split_whitespace().collect::<Vec<_>>();
    if tokens.len() != 1 {
        return Err(KeymapError::LeaderSequence);
    }
    if tokens[0].eq_ignore_ascii_case("leader") {
        return Err(KeymapError::RecursiveLeader);
    }
    let stroke = parse_stroke(tokens[0], None)?;
    if stroke.mods != Modifiers::default() {
        return Err(KeymapError::LeaderModified);
    }
    Ok(stroke)
}

fn parse_stroke(token: &str, leader: Option<KeyInput>) -> Result<KeyInput, KeymapError> {
    let parts = token.split('+').collect::<Vec<_>>();
    if parts.iter().any(|part| part.is_empty()) {
        return Err(KeymapError::UnknownKey(token.to_owned()));
    }
    let (key_name, modifier_names) = parts.split_last().expect("split always has one item");
    let mut mods = Modifiers::default();
    for modifier in modifier_names {
        if modifier.eq_ignore_ascii_case("ctrl") {
            mods.ctrl = true;
        } else if modifier.eq_ignore_ascii_case("alt") {
            mods.alt = true;
        } else if modifier.eq_ignore_ascii_case("shift") {
            mods.shift = true;
        } else {
            return Err(KeymapError::UnknownModifier((*modifier).to_owned()));
        }
    }
    if key_name.eq_ignore_ascii_case("leader") {
        if mods != Modifiers::default() {
            return Err(KeymapError::ModifiedLeader);
        }
        return leader.ok_or(KeymapError::LeaderUnset);
    }

    let mut named = true;
    let mut key = match key_name.to_ascii_lowercase().as_str() {
        "enter" => Key::Enter,
        "esc" => Key::Esc,
        "backspace" => Key::Backspace,
        "tab" => Key::Tab,
        "delete" => Key::Delete,
        "space" => Key::Char(' '),
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        lower if lower.len() >= 2 && lower.starts_with('f') => lower[1..]
            .parse::<u8>()
            .ok()
            .filter(|number| (1..=12).contains(number))
            .map(Key::F)
            .ok_or_else(|| KeymapError::UnknownKey((*key_name).to_owned()))?,
        _ => {
            let mut chars = key_name.chars();
            let character = chars
                .next()
                .filter(|character| !character.is_control() && chars.next().is_none())
                .ok_or_else(|| KeymapError::UnknownKey((*key_name).to_owned()))?;
            named = false;
            Key::Char(character)
        }
    };
    if !named && mods.shift {
        return Err(KeymapError::ShiftCharacter((*key_name).to_owned()));
    }
    if (mods.ctrl || mods.alt)
        && let Key::Char(character) = &mut key
        && character.is_ascii_alphabetic()
    {
        *character = character.to_ascii_lowercase();
    }
    Ok(KeyInput { key, mods })
}

impl fmt::Display for KeySequence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, stroke) in self.0.iter().enumerate() {
            if index != 0 {
                formatter.write_str(" ")?;
            }
            write_stroke(formatter, stroke)?;
        }
        Ok(())
    }
}

fn write_stroke(formatter: &mut fmt::Formatter<'_>, stroke: &KeyInput) -> fmt::Result {
    if stroke.mods.ctrl {
        formatter.write_str("Ctrl+")?;
    }
    if stroke.mods.alt {
        formatter.write_str("Alt+")?;
    }
    if stroke.mods.shift {
        formatter.write_str("Shift+")?;
    }
    match stroke.key {
        Key::Char(' ') => formatter.write_str("Space"),
        Key::Char(character) => write!(formatter, "{character}"),
        Key::Enter => formatter.write_str("Enter"),
        Key::Esc => formatter.write_str("Esc"),
        Key::Backspace => formatter.write_str("Backspace"),
        Key::Tab => formatter.write_str("Tab"),
        Key::Delete => formatter.write_str("Delete"),
        Key::Up => formatter.write_str("Up"),
        Key::Down => formatter.write_str("Down"),
        Key::Left => formatter.write_str("Left"),
        Key::Right => formatter.write_str("Right"),
        Key::Home => formatter.write_str("Home"),
        Key::End => formatter.write_str("End"),
        Key::PageUp => formatter.write_str("PageUp"),
        Key::PageDown => formatter.write_str("PageDown"),
        Key::F(number) => write!(formatter, "F{number}"),
    }
}

/// 組み込みkeymapで使う既定leader。
pub fn default_leader() -> KeyInput {
    KeyInput {
        key: Key::Char(' '),
        mods: Modifiers::default(),
    }
}

/// 現行の組み込みキー操作と順序が一致する既定バインディングを返す。
pub fn default_bindings(leader: KeyInput) -> Vec<KeyBinding> {
    let entries = [
        ("Enter", EditorAction::Activate),
        ("Backspace", EditorAction::NavigateParent),
        ("g d", EditorAction::NavigateInto),
        ("g .", EditorAction::ToggleHidden),
        ("z c", EditorAction::FoldClose),
        ("z o", EditorAction::FoldOpen),
        ("z a", EditorAction::FoldToggle),
        ("z C", EditorAction::FoldCloseRecursive),
        ("z O", EditorAction::FoldOpenRecursive),
        ("z M", EditorAction::FoldCloseAll),
        ("z R", EditorAction::FoldOpenAll),
        ("g /", EditorAction::FilePicker),
        ("g y", EditorAction::YankPath),
        ("g o", EditorAction::OpenWith),
        ("g m", EditorAction::TransferMove),
        ("g c", EditorAction::TransferCopy),
        ("Leader e", EditorAction::ToggleDockFocus),
        ("?", EditorAction::Help),
        ("Ctrl+W s", EditorAction::PaneSplitHorizontal),
        ("Ctrl+W S", EditorAction::PaneSplitHorizontal),
        ("Ctrl+W v", EditorAction::PaneSplitVertical),
        ("Ctrl+W h", EditorAction::PaneFocusLeft),
        ("Ctrl+W j", EditorAction::PaneFocusDown),
        ("Ctrl+W k", EditorAction::PaneFocusUp),
        ("Ctrl+W l", EditorAction::PaneFocusRight),
        ("Ctrl+W w", EditorAction::PaneFocusNext),
        ("Ctrl+W Ctrl+W", EditorAction::PaneFocusNext),
        ("Ctrl+W p", EditorAction::PaneFocusPrevious),
        ("Ctrl+W q", EditorAction::PaneClose),
        ("Ctrl+W c", EditorAction::PaneClose),
        ("Ctrl+P", EditorAction::HistoryBack),
        ("Ctrl+N", EditorAction::HistoryForward),
        ("Ctrl+R", EditorAction::Refresh),
    ];
    entries
        .into_iter()
        .map(|(sequence, action)| KeyBinding {
            sequence: parse_key_sequence(sequence, Some(leader))
                .expect("built-in keymap must be valid"),
            action,
        })
        .collect()
}

/// 既定値へユーザー指定を順に適用し、不正な項目だけを警告して無視する。
pub fn resolve_bindings(
    leader: KeyInput,
    user_entries: &[(String, String)],
) -> (Vec<KeyBinding>, Vec<String>) {
    let mut bindings = default_bindings(leader);
    let mut warnings = Vec::new();
    let mut seen = Vec::<KeySequence>::new();
    for (source, action_name) in user_entries {
        let sequence = match parse_key_sequence(source, Some(leader)) {
            Ok(sequence) => sequence,
            Err(error) => {
                warnings.push(format!("Ignoring key {source:?}: {error}"));
                continue;
            }
        };
        if seen.contains(&sequence) {
            warnings.push(format!(
                "Duplicate key sequence {sequence}; using the later binding"
            ));
        }
        seen.push(sequence.clone());
        if action_name.eq_ignore_ascii_case("none") {
            let before = bindings.len();
            bindings.retain(|binding| binding.sequence != sequence);
            if bindings.len() == before {
                warnings.push(format!("Cannot unmap unassigned sequence {sequence}"));
            }
            continue;
        }
        let Some(action) = EditorAction::from_config_name(action_name) else {
            warnings.push(format!("Ignoring unknown action name: {action_name}"));
            continue;
        };
        if is_ctrl_w_only(&sequence) {
            warnings.push("Ctrl+W cannot be bound by itself".to_owned());
            continue;
        }
        let remaining = bindings
            .iter()
            .filter(|binding| binding.sequence != sequence);
        if starts_ctrl_w(&sequence)
            && remaining.into_iter().any(|binding| {
                starts_ctrl_w(&binding.sequence) && is_true_prefix(&sequence.0, &binding.sequence.0)
                    || starts_ctrl_w(&binding.sequence)
                        && is_true_prefix(&binding.sequence.0, &sequence.0)
            })
        {
            warnings.push(format!(
                "Ignoring Ctrl+W sequence {sequence} because it has a prefix conflict with an existing sequence"
            ));
            continue;
        }
        bindings.retain(|binding| binding.sequence != sequence);
        bindings.push(KeyBinding { sequence, action });
    }
    (bindings, warnings)
}

fn ctrl_w() -> KeyInput {
    KeyInput {
        key: Key::Char('w'),
        mods: Modifiers {
            ctrl: true,
            ..Modifiers::default()
        },
    }
}
fn starts_ctrl_w(sequence: &KeySequence) -> bool {
    sequence.0.first() == Some(&ctrl_w())
}
fn is_ctrl_w_only(sequence: &KeySequence) -> bool {
    sequence.0.as_slice() == [ctrl_w()]
}
fn is_true_prefix(left: &[KeyInput], right: &[KeyInput]) -> bool {
    left.len() < right.len() && right.starts_with(left)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space() -> KeyInput {
        KeyInput {
            key: Key::Char(' '),
            mods: Modifiers::default(),
        }
    }

    #[test]
    fn parses_sequences_named_keys_and_normalization() {
        for input in [
            "g d",
            "Ctrl+W v",
            "Ctrl+Alt+F5",
            "Enter",
            "Space",
            "?",
            "Esc",
            "Backspace",
            "Tab",
            "Delete",
            "Up",
            "Down",
            "Left",
            "Right",
            "Home",
            "End",
            "PageUp",
            "PageDown",
            "F1",
            "F12",
        ] {
            assert!(parse_key_sequence(input, None).is_ok(), "{input}");
        }
        assert_eq!(
            parse_key_sequence("ctrl+w", None),
            parse_key_sequence("Ctrl+W", None)
        );
        assert_eq!(
            parse_key_sequence("V", None).unwrap().0[0].key,
            Key::Char('V')
        );
    }

    #[test]
    fn rejects_invalid_sequences() {
        for input in ["", "NoSuchKey", "Meta+X", "Shift+v"] {
            assert!(parse_key_sequence(input, None).is_err(), "{input}");
        }
        assert_eq!(
            parse_key_sequence("Leader", None),
            Err(KeymapError::LeaderUnset)
        );
        assert_eq!(
            parse_key_sequence("Ctrl+Leader", Some(space())),
            Err(KeymapError::ModifiedLeader)
        );
    }

    #[test]
    fn leader_is_one_unmodified_key() {
        assert_eq!(parse_leader("Space"), Ok(space()));
        assert_eq!(parse_leader("g d"), Err(KeymapError::LeaderSequence));
        assert_eq!(parse_leader("Ctrl+X"), Err(KeymapError::LeaderModified));
    }

    #[test]
    fn display_round_trips() {
        for input in ["g d", "Ctrl+W v", "Ctrl+Alt+F5", "Shift+Enter", "Space f"] {
            let sequence = parse_key_sequence(input, None).unwrap();
            assert_eq!(
                parse_key_sequence(&sequence.to_string(), None),
                Ok(sequence)
            );
        }
    }

    #[test]
    fn default_dock_focus_binding_follows_configured_leader() {
        let leader = parse_leader("x").unwrap();
        assert!(default_bindings(leader).iter().any(|binding| {
            binding.sequence.to_string() == "x e" && binding.action == EditorAction::ToggleDockFocus
        }));
    }

    #[test]
    fn empty_resolution_preserves_defaults_exactly() {
        assert_eq!(
            resolve_bindings(space(), &[]),
            (default_bindings(space()), Vec::new())
        );
    }

    #[test]
    fn resolution_supports_override_unmap_leader_and_warnings() {
        let entries = vec![
            ("g d".into(), "help".into()),
            ("g .".into(), "none".into()),
            ("Leader f".into(), "file_picker".into()),
            ("x".into(), "unknown".into()),
            ("g d".into(), "activate".into()),
            ("Ctrl+W".into(), "help".into()),
            ("Ctrl+W v x".into(), "help".into()),
        ];
        let (bindings, warnings) = resolve_bindings(space(), &entries);
        assert!(
            bindings
                .iter()
                .any(|binding| binding.sequence.to_string() == "g d"
                    && binding.action == EditorAction::Activate)
        );
        assert!(
            !bindings
                .iter()
                .any(|binding| binding.sequence.to_string() == "g .")
        );
        assert!(
            bindings
                .iter()
                .any(|binding| binding.sequence.to_string() == "Space f")
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("unknown action"))
        );
        assert!(warnings.iter().any(|warning| warning.contains("Duplicate")));
        assert!(warnings.iter().any(|warning| warning.contains("by itself")));
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("prefix conflict"))
        );
    }

    #[test]
    fn action_config_names_round_trip() {
        for binding in default_bindings(space()) {
            let action = binding.action;
            assert_eq!(
                EditorAction::from_config_name(action.config_name()),
                Some(action)
            );
            assert!(!action.description().is_empty());
        }
    }
}
