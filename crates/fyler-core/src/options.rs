//! 表示・走査に共通するエンジン非依存の設定型。

/// ツリーエントリのソート順。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortOrder {
    /// ディレクトリを先にまとめ、それぞれを自然順で並べる。
    #[default]
    DirsFirst,
    /// 種別を分けず、ディレクトリとファイルを自然順で混在させる。
    Mixed,
}

/// ツリーエントリのソートキー。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortKey {
    /// 名前の自然順(既定)。
    #[default]
    Name,
    /// 更新日時順。
    Date,
    /// バイトサイズ順。
    Size,
    /// 拡張子順。
    Extension,
}

/// `:terminal` で起動する外部terminal emulatorの種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TerminalKind {
    /// Windows Terminal → PowerShell → cmd の順で利用可能なものを起動する。
    #[default]
    Auto,
    /// Windows Terminal (`wt.exe`)。
    WindowsTerminal,
    /// PowerShell (`powershell.exe`)。
    PowerShell,
    /// cmd (`cmd.exe`)。
    Cmd,
}

/// ステータスライン(モードライン)へ並べる表示項目。
///
/// GUIはこの順序で左右のクラスタを描く。`config.toml`の`[statusline]`で
/// ユーザーがカスタムでき、既定は左=mode/branch/path、右=line/column/percent。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusItem {
    /// 現在のエディタモード(NORMAL/INSERT ...)。
    Mode,
    /// Git管理下ならブランチ名。管理外では何も描かない。
    Branch,
    /// 現在の表示ルートの絶対パス。
    Path,
    /// カーソル行番号(1始まり)。
    Line,
    /// カーソル桁番号(1始まり)。
    Column,
    /// ファイル内のカーソル位置の百分率。
    Percent,
    /// カーソル行のエントリのバイトサイズ。
    Size,
    /// カーソル行のエントリの更新日時。
    Modified,
}

impl StatusItem {
    /// `config.toml`で使う項目名から対応する項目を返す。
    pub fn from_config_name(name: &str) -> Option<Self> {
        Some(match name {
            "mode" => Self::Mode,
            "branch" => Self::Branch,
            "path" => Self::Path,
            "line" => Self::Line,
            "column" => Self::Column,
            "percent" => Self::Percent,
            "size" => Self::Size,
            "modified" => Self::Modified,
            _ => return None,
        })
    }

    /// `config.toml`で使う項目名。
    pub fn config_name(self) -> &'static str {
        match self {
            Self::Mode => "mode",
            Self::Branch => "branch",
            Self::Path => "path",
            Self::Line => "line",
            Self::Column => "column",
            Self::Percent => "percent",
            Self::Size => "size",
            Self::Modified => "modified",
        }
    }
}

/// 既定のステータスライン左クラスタ(mode, branch, path)。
pub fn default_statusline_left() -> Vec<StatusItem> {
    vec![StatusItem::Mode, StatusItem::Branch, StatusItem::Path]
}

/// 既定のステータスライン右クラスタ(line, column, percent)。
pub fn default_statusline_right() -> Vec<StatusItem> {
    vec![StatusItem::Line, StatusItem::Column, StatusItem::Percent]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_item_config_names_round_trip() {
        for item in [
            StatusItem::Mode,
            StatusItem::Branch,
            StatusItem::Path,
            StatusItem::Line,
            StatusItem::Column,
            StatusItem::Percent,
            StatusItem::Size,
            StatusItem::Modified,
        ] {
            assert_eq!(StatusItem::from_config_name(item.config_name()), Some(item));
        }
        assert_eq!(StatusItem::from_config_name("nope"), None);
    }

    #[test]
    fn statusline_defaults_match_requested_layout() {
        assert_eq!(
            default_statusline_left(),
            [StatusItem::Mode, StatusItem::Branch, StatusItem::Path]
        );
        assert_eq!(
            default_statusline_right(),
            [StatusItem::Line, StatusItem::Column, StatusItem::Percent]
        );
    }
}
