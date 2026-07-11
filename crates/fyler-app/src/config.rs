//! fyler自身の設定・状態ファイル。
//!
//! - `config.toml`: ユーザー所有。読み取り専用(fylerは書かない)
//! - `recent.toml`: fylerが書く唯一の永続ファイル(最近使ったルート)
//! - 置き場所: Windows `%APPDATA%\fyler\`、それ以外
//!   `$XDG_CONFIG_HOME/fyler/` または `~/.config/fyler/`
//! - 環境変数`FYLER_CONFIG_DIR`があれば最優先する(テスト用)

use std::collections::HashSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use fyler_core::editor::{Key, KeyInput, Modifiers};
use fyler_core::keymap;
use fyler_core::options::{SortKey, SortOrder, TerminalKind};
use fyler_gui::confirm::{ConfirmDetail, IconStyle};

const CONFIG_FILE: &str = "config.toml";
const RECENT_FILE: &str = "recent.toml";
const MAX_RECENT_ROOTS: usize = 10;
const DEFAULT_FONT_Y_OFFSET_FACTOR: f32 = 0.12;

/// ユーザー設定。無指定または不正な項目には安全な既定値を使う。
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// 起動時から隠しファイルを表示するか。
    pub show_hidden: bool,
    /// ツリーのソート順。
    pub sort: SortOrder,
    /// ツリーのソートキー。
    pub sort_key: SortKey,
    /// ソートキー部分を降順にするか。
    pub sort_reverse: bool,
    /// 外部terminal emulatorの起動方法。
    pub terminal: TerminalKind,
    /// 匿名フィードバック送信endpoint。空文字列は明示的無効化を表す。
    pub feedback_url: Option<String>,
    /// 確認ダイアログの操作一覧詳細度。
    pub confirm_detail: ConfirmDetail,
    /// 日本語fallbackフォントとして読み込むファイルの絶対パス。
    pub font: Option<PathBuf>,
    /// CJKフォントの上寄りを補正する、フォントサイズ比の下方向オフセット。
    ///
    /// CJKフォントはascent metricsが既定フォントと異なり上寄りに描画されるため、
    /// フォントサイズ比で下方向へずらす。`0`で無効。
    pub font_y_offset_factor: f32,
    /// ツリーへ描画するファイルアイコンのスタイル。
    pub icons: IconStyle,
    /// 名前と絶対パスのブックマーク。設定ファイルでの定義順を保持する。
    pub bookmarks: Vec<(String, PathBuf)>,
    /// 解決済みkeymapバインディング(デフォルト+ユーザー上書き適用済み)。
    pub bindings: Vec<keymap::KeyBinding>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            show_hidden: false,
            sort: SortOrder::DirsFirst,
            sort_key: SortKey::Name,
            sort_reverse: false,
            terminal: TerminalKind::Auto,
            feedback_url: None,
            confirm_detail: ConfirmDetail::Full,
            font: None,
            font_y_offset_factor: DEFAULT_FONT_Y_OFFSET_FACTOR,
            icons: IconStyle::Ascii,
            bookmarks: Vec::new(),
            bindings: keymap::default_bindings(),
        }
    }
}

/// `config.toml`を読み込む。
///
/// ファイルがなければ警告なしで既定値を返す。構文エラー、型不一致、未知キー、
/// 相対パスのフォント・ブックマークは起動を止めず、該当項目を無視して警告を返す。
pub fn load() -> (Config, Vec<String>) {
    let mut warnings = Vec::new();
    let directory = match config_dir() {
        Ok(directory) => directory,
        Err(error) => {
            warnings.push(format!("設定ディレクトリを特定できません: {error:#}"));
            return (Config::default(), warnings);
        }
    };
    let path = directory.join(CONFIG_FILE);
    let source = match fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (Config::default(), warnings);
        }
        Err(error) => {
            warnings.push(format!("{}を読み込めません: {error}", path.display()));
            return (Config::default(), warnings);
        }
    };
    let table = match source.parse::<toml::Table>() {
        Ok(table) => table,
        Err(error) => {
            warnings.push(format!("{}のTOMLが壊れています: {error}", path.display()));
            return (Config::default(), warnings);
        }
    };

    let mut config = Config::default();
    let default_leader = KeyInput {
        key: Key::Char(' '),
        mods: Modifiers::default(),
    };
    let leader = match table.get("leader") {
        Some(value) => match value.as_str() {
            Some(value) => match keymap::parse_leader(value) {
                Ok(leader) => leader,
                Err(error) => {
                    warnings.push(format!("leaderの指定が不正なためSpaceを使います: {error}"));
                    default_leader
                }
            },
            None => {
                warnings.push("leaderは文字列で指定してください。Spaceを使います".to_owned());
                default_leader
            }
        },
        None => default_leader,
    };
    if let Some(value) = table.get("show_hidden") {
        match value.as_bool() {
            Some(show_hidden) => config.show_hidden = show_hidden,
            None => warnings.push("show_hiddenはtrueまたはfalseで指定してください".to_owned()),
        }
    }
    if let Some(value) = table.get("sort") {
        match value.as_str() {
            Some("dirs_first") => config.sort = SortOrder::DirsFirst,
            Some("mixed") => config.sort = SortOrder::Mixed,
            _ => warnings.push("sortは\"dirs_first\"または\"mixed\"で指定してください".to_owned()),
        }
    }
    if let Some(value) = table.get("sort_key") {
        match value.as_str() {
            Some("name") => config.sort_key = SortKey::Name,
            Some("date") => config.sort_key = SortKey::Date,
            Some("size") => config.sort_key = SortKey::Size,
            Some("ext") => config.sort_key = SortKey::Extension,
            _ => warnings.push(
                "sort_keyは\"name\"、\"date\"、\"size\"、\"ext\"のいずれかで指定してください"
                    .to_owned(),
            ),
        }
    }
    if let Some(value) = table.get("sort_reverse") {
        match value.as_bool() {
            Some(reverse) => config.sort_reverse = reverse,
            None => warnings.push("sort_reverseはtrueまたはfalseで指定してください".to_owned()),
        }
    }
    if let Some(value) = table.get("terminal") {
        match value.as_str() {
            Some("auto") => config.terminal = TerminalKind::Auto,
            Some("windows_terminal") => config.terminal = TerminalKind::WindowsTerminal,
            Some("powershell") => config.terminal = TerminalKind::PowerShell,
            Some("cmd") => config.terminal = TerminalKind::Cmd,
            _ => warnings.push(
                "terminalは\"auto\"、\"windows_terminal\"、\"powershell\"、\"cmd\"のいずれかで指定してください"
                    .to_owned(),
            ),
        }
    }
    if let Some(value) = table.get("feedback_url") {
        match value.as_str() {
            Some(url) => config.feedback_url = Some(url.to_owned()),
            None => warnings.push("feedback_urlは文字列で指定してください".to_owned()),
        }
    }
    if let Some(value) = table.get("confirm_detail") {
        match value.as_str() {
            Some("full") => config.confirm_detail = ConfirmDetail::Full,
            Some("summary") => config.confirm_detail = ConfirmDetail::Summary,
            _ => warnings
                .push("confirm_detailは\"full\"または\"summary\"で指定してください".to_owned()),
        }
    }
    if let Some(value) = table.get("font") {
        match value.as_str() {
            Some(path) => {
                let path = PathBuf::from(path);
                if path.is_absolute() {
                    config.font = Some(path);
                } else {
                    warnings.push(format!(
                        "fontは絶対パスではないため無視します: {}",
                        path.display()
                    ));
                }
            }
            None => warnings.push("fontは文字列で指定してください".to_owned()),
        }
    }
    if let Some(value) = table.get("font_y_offset_factor") {
        match numeric_f32(value) {
            Some(factor) => config.font_y_offset_factor = factor,
            None => warnings.push("font_y_offset_factorは数値で指定してください".to_owned()),
        }
    }
    if let Some(value) = table.get("icons") {
        match value.as_str() {
            Some("ascii") => config.icons = IconStyle::Ascii,
            Some("nerd") => config.icons = IconStyle::Nerd,
            _ => warnings.push("iconsは\"ascii\"または\"nerd\"で指定してください".to_owned()),
        }
    }
    if let Some(value) = table.get("bookmarks") {
        match value.as_table() {
            Some(bookmarks) => {
                let order = bookmark_definition_order(&source);
                let mut visited = HashSet::new();
                for name in order.iter().chain(bookmarks.keys()) {
                    if !visited.insert(name.clone()) {
                        continue;
                    }
                    let Some(value) = bookmarks.get(name) else {
                        continue;
                    };
                    let Some(path) = value.as_str() else {
                        warnings.push(format!(
                            "ブックマーク{name}のパスは文字列で指定してください"
                        ));
                        continue;
                    };
                    let path = PathBuf::from(path);
                    if !path.is_absolute() {
                        warnings.push(format!(
                            "ブックマーク{name}は絶対パスではないため無視します: {}",
                            path.display()
                        ));
                        continue;
                    }
                    config.bookmarks.push((name.clone(), path));
                }
            }
            None => warnings.push("bookmarksはテーブルで指定してください".to_owned()),
        }
    }
    if let Some(value) = table.get("keymap") {
        match value.as_table() {
            Some(keymap_table) => {
                let mut entries = Vec::new();
                for section in keymap_table.keys() {
                    if section != "normal" {
                        warnings.push(format!("未対応のkeymapセクションを無視します: {section}"));
                    }
                }
                if let Some(normal) = keymap_table.get("normal") {
                    match normal.as_table() {
                        Some(normal) => {
                            for (sequence, value) in normal {
                                match value.as_str() {
                                    Some(action) => {
                                        entries.push((sequence.clone(), action.to_owned()))
                                    }
                                    None => warnings.push(format!(
                                        "keymap.normalの{sequence:?}は文字列で指定してください"
                                    )),
                                }
                            }
                        }
                        None => {
                            warnings.push("keymap.normalはテーブルで指定してください".to_owned())
                        }
                    }
                }
                let (bindings, keymap_warnings) = keymap::resolve_bindings(Some(leader), &entries);
                config.bindings = bindings;
                warnings.extend(
                    keymap_warnings
                        .into_iter()
                        .map(|warning| format!("keymap: {warning}")),
                );
            }
            None => warnings.push("keymapはテーブルで指定してください".to_owned()),
        }
    }

    for key in table.keys() {
        if !matches!(
            key.as_str(),
            "show_hidden"
                | "sort"
                | "sort_key"
                | "sort_reverse"
                | "terminal"
                | "feedback_url"
                | "confirm_detail"
                | "font"
                | "font_y_offset_factor"
                | "icons"
                | "bookmarks"
                | "leader"
                | "keymap"
        ) {
            warnings.push(format!("未知の設定キーを無視します: {key}"));
        }
    }

    (config, warnings)
}

fn numeric_f32(value: &toml::Value) -> Option<f32> {
    let value = match value {
        toml::Value::Float(value) => *value as f32,
        toml::Value::Integer(value) => *value as f32,
        _ => return None,
    };
    value.is_finite().then_some(value)
}

/// `recent.toml`から最近使ったルートを新しい順で読む。
///
/// ファイルがない場合や内容を解釈できない場合は空を返す。
pub fn load_recent_roots() -> Vec<PathBuf> {
    let Ok(directory) = config_dir() else {
        return Vec::new();
    };
    let Ok(source) = fs::read_to_string(directory.join(RECENT_FILE)) else {
        return Vec::new();
    };
    let Ok(table) = source.parse::<toml::Table>() else {
        return Vec::new();
    };
    let Some(roots) = table.get("roots").and_then(toml::Value::as_array) else {
        return Vec::new();
    };

    let mut seen = HashSet::new();
    roots
        .iter()
        .filter_map(toml::Value::as_str)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .filter(|path| seen.insert(path.clone()))
        .take(MAX_RECENT_ROOTS)
        .collect()
}

/// ルートを最近使った一覧の先頭へ追加し、`recent.toml`へ保存する。
///
/// 既存の重複は取り除いて先頭へ移動し、最大10件に切り詰める。保存は同じ
/// ディレクトリの一時ファイルへ完全なTOMLを書いてからrenameする。
pub fn record_recent_root(root: &Path) -> anyhow::Result<()> {
    if !root.is_absolute() {
        anyhow::bail!("最近使ったルートには絶対パスが必要です: {}", root.display());
    }

    let directory = config_dir()?;
    fs::create_dir_all(&directory)
        .with_context(|| format!("設定ディレクトリを作成できません: {}", directory.display()))?;

    let mut roots = load_recent_roots();
    roots.retain(|recent| recent != root);
    roots.insert(0, root.to_path_buf());
    roots.truncate(MAX_RECENT_ROOTS);

    let mut table = toml::Table::new();
    table.insert(
        "roots".to_owned(),
        toml::Value::Array(
            roots
                .into_iter()
                .map(|path| toml::Value::String(path.to_string_lossy().into_owned()))
                .collect(),
        ),
    );
    let contents = table.to_string();
    let target = directory.join(RECENT_FILE);
    let temporary = directory.join(format!(".{RECENT_FILE}.{}.tmp", std::process::id()));
    fs::write(&temporary, contents)
        .with_context(|| format!("一時設定ファイルを書き込めません: {}", temporary.display()))?;
    if let Err(error) = fs::rename(&temporary, &target) {
        let _ = fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("{}を置き換えできません", target.display()));
    }
    Ok(())
}

fn config_dir() -> anyhow::Result<PathBuf> {
    if let Some(path) = nonempty_env("FYLER_CONFIG_DIR") {
        return Ok(PathBuf::from(path));
    }

    #[cfg(windows)]
    {
        nonempty_env("APPDATA")
            .map(PathBuf::from)
            .map(|path| path.join("fyler"))
            .context("APPDATAが設定されていません")
    }

    #[cfg(not(windows))]
    {
        if let Some(path) = nonempty_env("XDG_CONFIG_HOME") {
            return Ok(PathBuf::from(path).join("fyler"));
        }
        nonempty_env("HOME")
            .map(PathBuf::from)
            .map(|path| path.join(".config").join("fyler"))
            .context("XDG_CONFIG_HOMEとHOMEが設定されていません")
    }
}

fn nonempty_env(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

// toml::Tableは既定機能ではキーを辞書順に保持するため、正しくparseした値とは別に
// bookmarks節の単純な1行代入を再parseして、ユーザーの定義順だけを復元する。
fn bookmark_definition_order(source: &str) -> Vec<String> {
    let mut in_bookmarks = false;
    let mut names = Vec::new();
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_bookmarks = trimmed == "[bookmarks]";
            continue;
        }
        if !in_bookmarks || trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Ok(line_table) = trimmed.parse::<toml::Table>()
            && line_table.len() == 1
            && let Some(name) = line_table.keys().next()
        {
            names.push(name.clone());
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    struct ConfigDirEnv {
        previous: Option<OsString>,
    }

    impl ConfigDirEnv {
        fn set(path: &Path) -> Self {
            let previous = std::env::var_os("FYLER_CONFIG_DIR");
            // SAFETY: FYLER_CONFIG_DIRを変更するテストはこの1件だけで完結する。
            unsafe {
                std::env::set_var("FYLER_CONFIG_DIR", path);
            }
            Self { previous }
        }
    }

    impl Drop for ConfigDirEnv {
        fn drop(&mut self) {
            // SAFETY: FYLER_CONFIG_DIRを変更するテストはこの1件だけで完結する。
            unsafe {
                match self.previous.take() {
                    Some(previous) => std::env::set_var("FYLER_CONFIG_DIR", previous),
                    None => std::env::remove_var("FYLER_CONFIG_DIR"),
                }
            }
        }
    }

    #[test]
    fn config_and_recent_files_follow_the_loading_and_persistence_contract() {
        let directory = tempdir().unwrap();
        let _env = ConfigDirEnv::set(directory.path());
        let path = directory.path().join(CONFIG_FILE);

        assert_eq!(load(), (Config::default(), Vec::new()));

        fs::write(&path, "show_hidden = [").unwrap();
        let (config, warnings) = load();
        assert_eq!(config, Config::default());
        assert_eq!(warnings.len(), 1);

        fs::write(&path, "show_hidden = \"yes\"").unwrap();
        let (config, warnings) = load();
        assert!(!config.show_hidden);
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("show_hidden"))
        );

        fs::write(&path, "sort_key = 'mtime'\nsort_reverse = 'yes'\n").unwrap();
        let (config, warnings) = load();
        assert_eq!(config.sort_key, SortKey::Name);
        assert!(!config.sort_reverse);
        assert!(warnings.iter().any(|warning| warning.contains("sort_key")));
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("sort_reverse"))
        );

        for (value, expected) in [
            ("auto", TerminalKind::Auto),
            ("windows_terminal", TerminalKind::WindowsTerminal),
            ("powershell", TerminalKind::PowerShell),
            ("cmd", TerminalKind::Cmd),
        ] {
            fs::write(&path, format!("terminal = '{value}'\n")).unwrap();
            let (config, warnings) = load();
            assert!(warnings.is_empty(), "{warnings:?}");
            assert_eq!(config.terminal, expected);
        }

        fs::write(&path, "terminal = 'wezterm'\n").unwrap();
        let (config, warnings) = load();
        assert_eq!(config.terminal, TerminalKind::Auto);
        assert!(warnings.iter().any(|warning| warning.contains("terminal")));

        fs::write(&path, "terminal = 1\n").unwrap();
        let (config, warnings) = load();
        assert_eq!(config.terminal, TerminalKind::Auto);
        assert!(warnings.iter().any(|warning| warning.contains("terminal")));

        fs::write(&path, "show_hidden = true\n").unwrap();
        let (config, warnings) = load();
        assert_eq!(config.terminal, TerminalKind::Auto);
        assert!(warnings.is_empty(), "{warnings:?}");

        fs::write(&path, "unknown = true").unwrap();
        let (config, warnings) = load();
        assert_eq!(config, Config::default());
        assert!(warnings.iter().any(|warning| warning.contains("unknown")));

        fs::write(
            &path,
            "leader = 'Space'\n[keymap.normal]\n'Leader f' = 'file_picker'\n'g d' = 'none'\n",
        )
        .unwrap();
        let (config, warnings) = load();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(config.bindings.iter().any(|binding| {
            binding.sequence.to_string() == "Space f"
                && binding.action == keymap::EditorAction::FilePicker
        }));
        assert!(
            !config
                .bindings
                .iter()
                .any(|binding| binding.sequence.to_string() == "g d")
        );

        fs::write(
            &path,
            "leader = 123\n[keymap.normal]\nx = 1\ny = 'unknown_action'\n[keymap.visual]\nz = 'help'\n",
        )
        .unwrap();
        let (config, warnings) = load();
        assert_eq!(config.bindings, keymap::default_bindings());
        assert!(warnings.iter().any(|warning| warning.contains("leader")));
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("文字列で指定"))
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("未知のaction"))
        );
        assert!(warnings.iter().any(|warning| warning.contains("未対応")));

        fs::write(&path, "[bookmarks]\nrelative = 'child/path'\n").unwrap();
        let (config, warnings) = load();
        assert!(config.bookmarks.is_empty());
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("絶対パスではない"))
        );

        fs::write(
            &path,
            "font = true\nfont_y_offset_factor = 'low'\nicons = 1\n",
        )
        .unwrap();
        let (config, warnings) = load();
        assert_eq!(config.font, None);
        assert_eq!(config.font_y_offset_factor, DEFAULT_FONT_Y_OFFSET_FACTOR);
        assert_eq!(config.icons, IconStyle::Ascii);
        assert!(warnings.iter().any(|warning| warning.contains("font")));
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("font_y_offset_factor"))
        );
        assert!(warnings.iter().any(|warning| warning.contains("icons")));

        fs::write(&path, "font = 'relative/font.ttf'\nicons = 'ascii'\n").unwrap();
        let (config, warnings) = load();
        assert_eq!(config.font, None);
        assert_eq!(config.icons, IconStyle::Ascii);
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("絶対パスではない"))
        );

        let first = directory.path().join("z-first");
        let second = directory.path().join("a-second");
        let font = directory.path().join("JapaneseFont.ttf");
        fs::write(
            &path,
            format!(
                "show_hidden = true\nsort = \"mixed\"\nconfirm_detail = \"summary\"\n\
                 sort_key = \"date\"\nsort_reverse = true\n\
                 font = '{}'\nfont_y_offset_factor = 0.25\nicons = \"nerd\"\n\
                 [bookmarks]\nzeta = '{}'\nalpha = '{}'\n",
                font.display(),
                first.display(),
                second.display()
            ),
        )
        .unwrap();
        let (config, warnings) = load();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(config.show_hidden);
        assert_eq!(config.sort, SortOrder::Mixed);
        assert_eq!(config.sort_key, SortKey::Date);
        assert!(config.sort_reverse);
        assert_eq!(config.confirm_detail, ConfirmDetail::Summary);
        assert_eq!(config.font, Some(font));
        assert_eq!(config.font_y_offset_factor, 0.25);
        assert_eq!(config.icons, IconStyle::Nerd);
        assert_eq!(
            config.bookmarks,
            [("zeta".to_owned(), first), ("alpha".to_owned(), second),]
        );

        assert!(load_recent_roots().is_empty());
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        record_recent_root(&first).unwrap();
        record_recent_root(&second).unwrap();
        assert_eq!(load_recent_roots(), [second.clone(), first.clone()]);

        record_recent_root(&first).unwrap();
        assert_eq!(load_recent_roots(), [first, second]);

        let roots = (0..11)
            .map(|index| directory.path().join(format!("root-{index}")))
            .collect::<Vec<_>>();
        for root in &roots {
            record_recent_root(root).unwrap();
        }
        let loaded = load_recent_roots();
        assert_eq!(loaded.len(), 10);
        assert_eq!(loaded[0], roots[10]);
        assert_eq!(loaded[9], roots[1]);

        let saved = fs::read_to_string(directory.path().join(RECENT_FILE)).unwrap();
        let table = saved.parse::<toml::Table>().unwrap();
        assert_eq!(
            table
                .get("roots")
                .and_then(toml::Value::as_array)
                .unwrap()
                .len(),
            10
        );
    }

    #[test]
    fn font_y_offset_factor_parser_accepts_numbers_and_rejects_other_types() {
        assert_eq!(numeric_f32(&toml::Value::Float(0.25)), Some(0.25));
        assert_eq!(numeric_f32(&toml::Value::Integer(0)), Some(0.0));
        assert_eq!(numeric_f32(&toml::Value::String("0.25".to_owned())), None);
    }
}
