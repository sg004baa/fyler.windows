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
    /// 選択項目をclipboardへコピーする(実FS非変更)。
    ClipboardCopy,
    /// 選択項目をclipboardへ切り取る(実FS非変更。移動はpaste側が行う)。
    ClipboardCut,
    /// clipboardの内容を現在paneのカーソル位置へ貼り付ける。
    ClipboardPaste,
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
    /// カーソル行のディレクトリのサイズを背景スレッドで再帰計算する。
    DirSize,
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
            "clipboard_copy" => Self::ClipboardCopy,
            "clipboard_cut" => Self::ClipboardCut,
            "clipboard_paste" => Self::ClipboardPaste,
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
            "dir_size" => Self::DirSize,
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
            Self::ClipboardCopy => "clipboard_copy",
            Self::ClipboardCut => "clipboard_cut",
            Self::ClipboardPaste => "clipboard_paste",
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
            Self::DirSize => "dir_size",
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
            Self::ClipboardCopy => "Copy to clipboard",
            Self::ClipboardCut => "Cut to clipboard",
            Self::ClipboardPaste => "Paste from clipboard",
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
            Self::DirSize => "Compute directory size",
        }
    }
}

/// 解決済みバインディング(シーケンス → 割り当て先)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyBinding {
    /// 起動する、展開済みのキーシーケンス。
    pub sequence: KeySequence,
    /// シーケンスへ割り当てる先(操作、または別のキーシーケンス)。
    pub target: BindingTarget,
}

/// キーシーケンスの割り当て先。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingTarget {
    /// エンジン非依存の操作を起動する。
    Action(EditorAction),
    /// 別のキーシーケンスをそのまま送出する(remapなし。任意キーへのバインド用)。
    Keys(KeySequence),
}

/// key表記を解釈できない理由。設定警告へそのまま埋め込める文を返す。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeymapError {
    #[error("Key sequence is empty")]
    Empty,
    #[error("Raw whitespace is not allowed; use <Space> instead")]
    RawWhitespace,
    #[error("Unclosed key notation: <{0}")]
    UnclosedBracket(String),
    #[error("Unknown modifier: {0}")]
    UnknownModifier(String),
    #[error("Unknown key name: {0}")]
    UnknownKey(String),
    #[error(
        "Shift cannot be used with a printable character; write the uppercase character directly: {0}"
    )]
    ShiftCharacter(String),
    #[error("leader is not configured")]
    LeaderUnset,
    #[error("leader cannot have modifiers")]
    ModifiedLeader,
    #[error("leader must be a single key")]
    LeaderSequence,
    #[error("leader cannot reference itself")]
    RecursiveLeader,
}

/// vim風表記(`gd`, `<leader>f`, `<C-r>`)のキーシーケンスを解釈する。
///
/// エンジン非依存の自前実装(nvim-rs等のnvim固有語彙には一切触れない。絶対ルール2)。
///
/// - 連結した文字はそれぞれ1ストロークになる(`gd` = `g`, `d`)。生の空白は
///   [`KeymapError::RawWhitespace`](`<Space>` を使う)。
/// - `<...>` は特殊キー・修飾つきキーを表す。`<` そのものを打つ場合は `<lt>` と書く。
pub fn parse_key_sequence(
    input: &str,
    leader: Option<KeyInput>,
) -> Result<KeySequence, KeymapError> {
    if input.is_empty() {
        return Err(KeymapError::Empty);
    }
    let mut strokes = Vec::new();
    let mut chars = input.chars();
    while let Some(character) = chars.next() {
        if character == '<' {
            let mut content = String::new();
            let mut closed = false;
            for next in chars.by_ref() {
                if next == '>' {
                    closed = true;
                    break;
                }
                content.push(next);
            }
            if !closed {
                return Err(KeymapError::UnclosedBracket(content));
            }
            strokes.push(parse_bracket(&content, leader)?);
        } else if character.is_whitespace() {
            return Err(KeymapError::RawWhitespace);
        } else {
            strokes.push(KeyInput {
                key: Key::Char(character),
                mods: Modifiers::default(),
            });
        }
    }
    if strokes.is_empty() {
        return Err(KeymapError::Empty);
    }
    Ok(KeySequence(strokes))
}

/// leader指定を単一・無修飾ストロークとして解釈する。
///
/// vimrcの`let mapleader=" "`慣習に合わせ、raw1文字スペースに限り`<Space>`として
/// 受理する(生の空白は通常エラーだが、leader定義だけの特例)。
pub fn parse_leader(input: &str) -> Result<KeyInput, KeymapError> {
    if input == " " {
        return Ok(KeyInput {
            key: Key::Char(' '),
            mods: Modifiers::default(),
        });
    }
    let sequence = parse_key_sequence(input, None).map_err(|error| match error {
        KeymapError::LeaderUnset => KeymapError::RecursiveLeader,
        other => other,
    })?;
    match sequence.0.as_slice() {
        [stroke] if stroke.mods == Modifiers::default() => Ok(*stroke),
        [_stroke] => Err(KeymapError::ModifiedLeader),
        _ => Err(KeymapError::LeaderSequence),
    }
}

/// `<...>` の中身(前後の`<`/`>`を除いた文字列)を1ストロークへ解釈する。
fn parse_bracket(content: &str, leader: Option<KeyInput>) -> Result<KeyInput, KeymapError> {
    if content.is_empty() {
        return Err(KeymapError::UnknownKey(String::new()));
    }
    let (modifier_names, key_name) = split_modifiers(content);
    let mut mods = Modifiers::default();
    for modifier in modifier_names {
        if modifier.eq_ignore_ascii_case("c") {
            mods.ctrl = true;
        } else if modifier.eq_ignore_ascii_case("a") || modifier.eq_ignore_ascii_case("m") {
            mods.alt = true;
        } else if modifier.eq_ignore_ascii_case("s") {
            mods.shift = true;
        } else {
            return Err(KeymapError::UnknownModifier(modifier.to_owned()));
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
        "cr" | "enter" => Key::Enter,
        "esc" => Key::Esc,
        "bs" => Key::Backspace,
        "tab" => Key::Tab,
        "del" => Key::Delete,
        "space" => Key::Char(' '),
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "lt" => Key::Char('<'),
        lower if lower.len() >= 2 && lower.starts_with('f') => lower[1..]
            .parse::<u8>()
            .ok()
            .filter(|number| (1..=12).contains(number))
            .map(Key::F)
            .ok_or_else(|| KeymapError::UnknownKey(key_name.to_owned()))?,
        _ => {
            let mut chars = key_name.chars();
            let character = chars
                .next()
                .filter(|character| !character.is_control() && chars.next().is_none())
                .ok_or_else(|| KeymapError::UnknownKey(key_name.to_owned()))?;
            named = false;
            Key::Char(character)
        }
    };
    if !named && mods.shift {
        return Err(KeymapError::ShiftCharacter(key_name.to_owned()));
    }
    if (mods.ctrl || mods.alt)
        && let Key::Char(character) = &mut key
        && character.is_ascii_alphabetic()
    {
        *character = character.to_ascii_lowercase();
    }
    Ok(KeyInput { key, mods })
}

/// `<...>` の中身をmodifierトークン列とキー名へ分割する。
///
/// 末尾が単独の`-`ならキー名は`-`自体を指す(例: `<C-->` = Ctrl+ハイフン)。
fn split_modifiers(content: &str) -> (Vec<&str>, &str) {
    if content == "-" {
        return (Vec::new(), "-");
    }
    if let Some(head) = content.strip_suffix('-') {
        let modifiers = head.split('-').filter(|part| !part.is_empty()).collect();
        return (modifiers, "-");
    }
    match content.rfind('-') {
        Some(index) => {
            let modifiers = content[..index]
                .split('-')
                .filter(|part| !part.is_empty())
                .collect();
            (modifiers, &content[index + 1..])
        }
        None => (Vec::new(), content),
    }
}

impl fmt::Display for KeySequence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for stroke in &self.0 {
            write_stroke(formatter, stroke)?;
        }
        Ok(())
    }
}

fn write_stroke(formatter: &mut fmt::Formatter<'_>, stroke: &KeyInput) -> fmt::Result {
    let bare = stroke.mods == Modifiers::default()
        && matches!(stroke.key, Key::Char(character) if character != '<' && character != ' ');
    if let (true, Key::Char(character)) = (bare, stroke.key) {
        return write!(formatter, "{character}");
    }

    formatter.write_str("<")?;
    if stroke.mods.ctrl {
        formatter.write_str("C-")?;
    }
    if stroke.mods.alt {
        formatter.write_str("A-")?;
    }
    if stroke.mods.shift {
        formatter.write_str("S-")?;
    }
    match stroke.key {
        Key::Char(' ') => formatter.write_str("Space")?,
        Key::Char('<') => formatter.write_str("lt")?,
        Key::Char(character) => write!(formatter, "{character}")?,
        Key::Enter => formatter.write_str("CR")?,
        Key::Esc => formatter.write_str("Esc")?,
        Key::Backspace => formatter.write_str("BS")?,
        Key::Tab => formatter.write_str("Tab")?,
        Key::Delete => formatter.write_str("Del")?,
        Key::Up => formatter.write_str("Up")?,
        Key::Down => formatter.write_str("Down")?,
        Key::Left => formatter.write_str("Left")?,
        Key::Right => formatter.write_str("Right")?,
        Key::Home => formatter.write_str("Home")?,
        Key::End => formatter.write_str("End")?,
        Key::PageUp => formatter.write_str("PageUp")?,
        Key::PageDown => formatter.write_str("PageDown")?,
        Key::F(number) => write!(formatter, "F{number}")?,
    }
    formatter.write_str(">")
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
        ("<CR>", EditorAction::Activate),
        ("<BS>", EditorAction::NavigateParent),
        ("gd", EditorAction::NavigateInto),
        ("g.", EditorAction::ToggleHidden),
        ("zc", EditorAction::FoldClose),
        ("zo", EditorAction::FoldOpen),
        ("za", EditorAction::FoldToggle),
        ("zC", EditorAction::FoldCloseRecursive),
        ("zO", EditorAction::FoldOpenRecursive),
        ("zM", EditorAction::FoldCloseAll),
        ("zR", EditorAction::FoldOpenAll),
        ("g/", EditorAction::FilePicker),
        ("gy", EditorAction::YankPath),
        ("go", EditorAction::OpenWith),
        ("gm", EditorAction::TransferMove),
        ("gc", EditorAction::TransferCopy),
        ("gs", EditorAction::DirSize),
        ("<C-c>", EditorAction::ClipboardCopy),
        ("<C-x>", EditorAction::ClipboardCut),
        ("<C-v>", EditorAction::ClipboardPaste),
        ("<leader>e", EditorAction::ToggleDockFocus),
        ("?", EditorAction::Help),
        ("<C-w>s", EditorAction::PaneSplitHorizontal),
        ("<C-w>S", EditorAction::PaneSplitHorizontal),
        ("<C-w>v", EditorAction::PaneSplitVertical),
        ("<C-w>h", EditorAction::PaneFocusLeft),
        ("<C-w>j", EditorAction::PaneFocusDown),
        ("<C-w>k", EditorAction::PaneFocusUp),
        ("<C-w>l", EditorAction::PaneFocusRight),
        ("<C-w>w", EditorAction::PaneFocusNext),
        ("<C-w><C-w>", EditorAction::PaneFocusNext),
        ("<C-w>p", EditorAction::PaneFocusPrevious),
        ("<C-w>q", EditorAction::PaneClose),
        ("<C-w>c", EditorAction::PaneClose),
        ("<C-p>", EditorAction::HistoryBack),
        ("<C-n>", EditorAction::HistoryForward),
        ("<C-r>", EditorAction::Refresh),
    ];
    entries
        .into_iter()
        .map(|(sequence, action)| KeyBinding {
            sequence: parse_key_sequence(sequence, Some(leader))
                .expect("built-in keymap must be valid"),
            target: BindingTarget::Action(action),
        })
        .collect()
}

/// 既定値へユーザー指定を順に適用し、不正な項目だけを警告して無視する。
///
/// 各エントリの値は次の優先順位で解釈する:
/// 1. `"none"`(大文字小文字非依存) — 対応するシーケンスのバインドを解除する。
/// 2. [`EditorAction::from_config_name`] に一致する名前 — その操作を割り当てる。
/// 3. それ以外 — vim風表記のキーシーケンスとして解釈し、[`BindingTarget::Keys`]として
///    割り当てる(remapなし。`vim.keymap.set`の`noremap`相当。任意キーへ任意キーを
///    バインドできる、例: `";" = ":"`)。
///
/// いずれの解釈にも失敗した項目は警告して無視する(既存contract)。
pub fn resolve_bindings(
    leader: KeyInput,
    user_entries: &[(String, String)],
) -> (Vec<KeyBinding>, Vec<String>) {
    let mut bindings = default_bindings(leader);
    let mut warnings = Vec::new();
    let mut seen = Vec::<KeySequence>::new();
    for (source, value) in user_entries {
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
        if value.eq_ignore_ascii_case("none") {
            let before = bindings.len();
            bindings.retain(|binding| binding.sequence != sequence);
            if bindings.len() == before {
                warnings.push(format!("Cannot unmap unassigned sequence {sequence}"));
            }
            continue;
        }
        let target = if let Some(action) = EditorAction::from_config_name(value) {
            BindingTarget::Action(action)
        } else {
            match parse_key_sequence(value, Some(leader)) {
                Ok(keys) => BindingTarget::Keys(keys),
                Err(error) => {
                    warnings.push(format!(
                        "Ignoring key {source:?}: {value:?} is not an action name or a valid key sequence: {error}"
                    ));
                    continue;
                }
            }
        };
        if is_ctrl_w_only(&sequence) {
            warnings.push("<C-w> cannot be bound by itself".to_owned());
            continue;
        }
        if starts_ctrl_w(&sequence) && matches!(target, BindingTarget::Keys(_)) {
            warnings.push(format!(
                "Ignoring key {sequence}: <C-w>-prefixed sequences can only be bound to actions"
            ));
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
                "Ignoring <C-w> sequence {sequence} because it has a prefix conflict with an existing sequence"
            ));
            continue;
        }
        bindings.retain(|binding| binding.sequence != sequence);
        bindings.push(KeyBinding { sequence, target });
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
            "gd",
            "<C-w>v",
            "<C-A-F5>",
            "<CR>",
            "<Space>",
            "?",
            "<Esc>",
            "<BS>",
            "<Tab>",
            "<Del>",
            "<Up>",
            "<Down>",
            "<Left>",
            "<Right>",
            "<Home>",
            "<End>",
            "<PageUp>",
            "<PageDown>",
            "<F1>",
            "<F12>",
        ] {
            assert!(parse_key_sequence(input, None).is_ok(), "{input}");
        }
        assert_eq!(
            parse_key_sequence("<c-w>", None),
            parse_key_sequence("<C-W>", None)
        );
        assert_eq!(
            parse_key_sequence("V", None).unwrap().0[0].key,
            Key::Char('V')
        );
        assert_eq!(
            parse_key_sequence("<M-x>", None),
            parse_key_sequence("<A-x>", None)
        );
    }

    #[test]
    fn rejects_invalid_sequences() {
        for input in ["", "<NoSuchKey>", "<Meta-x>", "<S-v>", "a b", "<C-w"] {
            assert!(parse_key_sequence(input, None).is_err(), "{input}");
        }
        assert_eq!(
            parse_key_sequence("<leader>", None),
            Err(KeymapError::LeaderUnset)
        );
        assert_eq!(
            parse_key_sequence("<C-leader>", Some(space())),
            Err(KeymapError::ModifiedLeader)
        );
    }

    #[test]
    fn leader_is_one_unmodified_key() {
        assert_eq!(parse_leader("<Space>"), Ok(space()));
        assert_eq!(parse_leader(" "), Ok(space()));
        assert_eq!(parse_leader("gd"), Err(KeymapError::LeaderSequence));
        assert_eq!(parse_leader("<C-x>"), Err(KeymapError::ModifiedLeader));
        assert_eq!(parse_leader("<leader>"), Err(KeymapError::RecursiveLeader));
    }

    #[test]
    fn display_round_trips() {
        for input in [
            "gd", "<C-w>v", "<C-A-F5>", "<S-CR>", "<Space>f", ";", "<lt>",
        ] {
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
            binding.sequence.to_string() == "xe"
                && binding.target == BindingTarget::Action(EditorAction::ToggleDockFocus)
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
            ("gd".into(), "help".into()),
            ("g.".into(), "none".into()),
            ("<leader>f".into(), "file_picker".into()),
            ("x".into(), "not a real action".into()),
            ("gd".into(), "activate".into()),
            ("<C-w>".into(), "help".into()),
            ("<C-w>vx".into(), "help".into()),
        ];
        let (bindings, warnings) = resolve_bindings(space(), &entries);
        assert!(bindings.iter().any(|binding| {
            binding.sequence.to_string() == "gd"
                && binding.target == BindingTarget::Action(EditorAction::Activate)
        }));
        assert!(
            !bindings
                .iter()
                .any(|binding| binding.sequence.to_string() == "g.")
        );
        assert!(bindings.iter().any(|binding| {
            binding.sequence.to_string() == "<Space>f"
                && binding.target == BindingTarget::Action(EditorAction::FilePicker)
        }));
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("not an action name"))
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
    fn resolves_vim_notation_and_arbitrary_key_targets() {
        let entries = vec![
            ("gd".into(), "navigate_into".into()),
            ("<leader>f".into(), "file_picker".into()),
            ("<C-r>".into(), "refresh".into()),
            (";".into(), ":".into()),
        ];
        let (bindings, warnings) = resolve_bindings(space(), &entries);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(bindings.iter().any(|binding| {
            binding.sequence.to_string() == "gd"
                && binding.target == BindingTarget::Action(EditorAction::NavigateInto)
        }));
        assert!(bindings.iter().any(|binding| {
            binding.sequence.to_string() == "<Space>f"
                && binding.target == BindingTarget::Action(EditorAction::FilePicker)
        }));
        assert!(bindings.iter().any(|binding| {
            binding.sequence.to_string() == "<C-r>"
                && binding.target == BindingTarget::Action(EditorAction::Refresh)
        }));
        let semicolon = bindings
            .iter()
            .find(|binding| binding.sequence.to_string() == ";")
            .expect("semicolon binding must resolve");
        assert_eq!(
            semicolon.target,
            BindingTarget::Keys(parse_key_sequence(":", None).unwrap())
        );
    }

    #[test]
    fn ctrl_w_prefixed_keys_binding_is_rejected() {
        let entries = vec![("<C-w>x".into(), ":".into())];
        let (bindings, warnings) = resolve_bindings(space(), &entries);
        assert!(
            !bindings
                .iter()
                .any(|binding| binding.sequence.to_string() == "<C-w>x")
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("<C-w>-prefixed"))
        );
    }

    #[test]
    fn action_config_names_round_trip() {
        for binding in default_bindings(space()) {
            let BindingTarget::Action(action) = binding.target else {
                panic!("default bindings must all be actions");
            };
            assert_eq!(
                EditorAction::from_config_name(action.config_name()),
                Some(action)
            );
            assert!(!action.description().is_empty());
        }
    }
}
