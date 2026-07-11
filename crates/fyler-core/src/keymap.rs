//! エンジン非依存のキー表記、操作、バインディング解決。

use std::fmt;

use crate::editor::{Key, KeyInput, Modifiers};

/// キーシーケンス(1個以上のキーストローク)。エンジン非依存。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeySequence(pub Vec<KeyInput>);

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
        }
    }

    /// ヘルプ表示向けの日本語説明を返す。
    pub fn description(&self) -> &'static str {
        match self {
            Self::Activate => "ディレクトリ開閉 / ファイルを開く",
            Self::NavigateParent => "親ディレクトリへ移動",
            Self::NavigateInto => "ディレクトリ内へ移動",
            Self::ToggleHidden => "隠しファイル表示を切り替え",
            Self::FoldClose => "ディレクトリを折りたたむ",
            Self::FoldOpen => "ディレクトリを展開",
            Self::FoldToggle => "折りたたみ状態を切り替え",
            Self::FoldCloseRecursive => "配下を再帰的に折りたたむ",
            Self::FoldOpenRecursive => "配下を再帰的に展開",
            Self::FoldCloseAll => "すべて折りたたむ",
            Self::FoldOpenAll => "すべて展開",
            Self::FilePicker => "ファイルを検索",
            Self::YankPath => "パスをコピー",
            Self::OpenWith => "アプリを選んで開く",
            Self::TransferMove => "別ペインへ移動",
            Self::TransferCopy => "別ペインへコピー",
            Self::Help => "ヘルプを表示",
            Self::PaneSplitHorizontal => "ペインを上下分割",
            Self::PaneSplitVertical => "ペインを左右分割",
            Self::PaneFocusLeft => "左ペインへ移動",
            Self::PaneFocusDown => "下ペインへ移動",
            Self::PaneFocusUp => "上ペインへ移動",
            Self::PaneFocusRight => "右ペインへ移動",
            Self::PaneFocusNext => "次のペインへ移動",
            Self::PaneFocusPrevious => "前のペインへ移動",
            Self::PaneClose => "ペインを閉じる",
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
    #[error("キーシーケンスが空です")]
    Empty,
    #[error("未知の修飾キーです: {0}")]
    UnknownModifier(String),
    #[error("未知のキー名です: {0}")]
    UnknownKey(String),
    #[error("印字可能文字にShiftは指定できません。大文字を直接記述してください: {0}")]
    ShiftCharacter(String),
    #[error("Leaderが設定されていません")]
    LeaderUnset,
    #[error("Leaderに修飾キーは指定できません")]
    ModifiedLeader,
    #[error("leaderは単一キーで指定してください")]
    LeaderSequence,
    #[error("leader自身に修飾キーは指定できません")]
    LeaderModified,
    #[error("leaderにLeaderは指定できません")]
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

/// 現行の組み込みキー操作と順序が一致する既定バインディングを返す。
pub fn default_bindings() -> Vec<KeyBinding> {
    let entries = [
        ("Enter", EditorAction::Activate),
        ("^", EditorAction::NavigateParent),
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
    ];
    entries
        .into_iter()
        .map(|(sequence, action)| KeyBinding {
            sequence: parse_key_sequence(sequence, None).expect("組み込みkeymapは妥当"),
            action,
        })
        .collect()
}

/// 既定値へユーザー指定を順に適用し、不正な項目だけを警告して無視する。
pub fn resolve_bindings(
    leader: Option<KeyInput>,
    user_entries: &[(String, String)],
) -> (Vec<KeyBinding>, Vec<String>) {
    let mut bindings = default_bindings();
    let mut warnings = Vec::new();
    let mut seen = Vec::<KeySequence>::new();
    for (source, action_name) in user_entries {
        let sequence = match parse_key_sequence(source, leader) {
            Ok(sequence) => sequence,
            Err(error) => {
                warnings.push(format!("キー{source:?}を無視します: {error}"));
                continue;
            }
        };
        if seen.contains(&sequence) {
            warnings.push(format!(
                "キーシーケンス{sequence}が重複しています。後の指定を使います"
            ));
        }
        seen.push(sequence.clone());
        if action_name.eq_ignore_ascii_case("none") {
            let before = bindings.len();
            bindings.retain(|binding| binding.sequence != sequence);
            if bindings.len() == before {
                warnings.push(format!("未割り当てのシーケンス{sequence}は解除できません"));
            }
            continue;
        }
        let Some(action) = EditorAction::from_config_name(action_name) else {
            warnings.push(format!("未知のaction名を無視します: {action_name}"));
            continue;
        };
        if is_ctrl_w_only(&sequence) {
            warnings.push("単独のCtrl+Wにはバインドできません".to_owned());
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
                "Ctrl+Wシーケンス{sequence}は既存シーケンスとプレフィックス衝突するため無視します"
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
    fn empty_resolution_preserves_defaults_exactly() {
        assert_eq!(
            resolve_bindings(Some(space()), &[]),
            (default_bindings(), Vec::new())
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
        let (bindings, warnings) = resolve_bindings(Some(space()), &entries);
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
                .any(|warning| warning.contains("未知のaction"))
        );
        assert!(warnings.iter().any(|warning| warning.contains("重複")));
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("単独のCtrl+W"))
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("プレフィックス衝突"))
        );
    }

    #[test]
    fn action_config_names_round_trip() {
        for binding in default_bindings() {
            let action = binding.action;
            assert_eq!(
                EditorAction::from_config_name(action.config_name()),
                Some(action)
            );
            assert!(!action.description().is_empty());
        }
    }
}
