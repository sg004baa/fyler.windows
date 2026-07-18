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
use fyler_core::keymap;
use fyler_core::options::{SortKey, SortOrder, StatusItem, TerminalKind};
use fyler_gui::confirm::ConfirmDetail;

const CONFIG_FILE: &str = "config.toml";
const RECENT_FILE: &str = "recent.toml";
const MAX_RECENT_ROOTS: usize = 10;

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
    /// 正常終了した前回セッションのpane配置と表示状態を復元するか。
    pub restore_session: bool,
    /// 外部terminal emulatorの起動方法。
    pub terminal: TerminalKind,
    /// 匿名フィードバック送信endpoint。空文字列は明示的無効化を表す。
    pub feedback_url: Option<String>,
    /// 確認ダイアログの操作一覧詳細度。
    pub confirm_detail: ConfirmDetail,
    /// 日本語fallbackフォントとして読み込むファイルの絶対パス。
    pub font: Option<PathBuf>,
    /// 名前と絶対パスのブックマーク。設定ファイルでの定義順を保持する。
    pub bookmarks: Vec<(String, PathBuf)>,
    /// 解決済みkeymapバインディング(デフォルト+ユーザー上書き適用済み)。
    pub bindings: Vec<keymap::KeyBinding>,
    /// ステータスライン左クラスタの表示項目。
    pub statusline_left: Vec<StatusItem>,
    /// ステータスライン右クラスタの表示項目。
    pub statusline_right: Vec<StatusItem>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            show_hidden: false,
            sort: SortOrder::DirsFirst,
            sort_key: SortKey::Name,
            sort_reverse: false,
            restore_session: true,
            terminal: TerminalKind::Auto,
            feedback_url: None,
            confirm_detail: ConfirmDetail::Full,
            font: None,
            bookmarks: Vec::new(),
            bindings: keymap::default_bindings(keymap::default_leader()),
            statusline_left: fyler_core::options::default_statusline_left(),
            statusline_right: fyler_core::options::default_statusline_right(),
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
            warnings.push(format!(
                "Failed to locate configuration directory: {error:#}"
            ));
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
            warnings.push(format!("Failed to read {}: {error}", path.display()));
            return (Config::default(), warnings);
        }
    };
    let table = match source.parse::<toml::Table>() {
        Ok(table) => table,
        Err(error) => {
            warnings.push(format!("Invalid TOML in {}: {error}", path.display()));
            return (Config::default(), warnings);
        }
    };

    let mut config = Config::default();
    let default_leader = keymap::default_leader();
    let leader = match table.get("leader") {
        Some(value) => match value.as_str() {
            Some(value) => match keymap::parse_leader(value) {
                Ok(leader) => leader,
                Err(error) => {
                    warnings.push(format!("Invalid leader; using Space: {error}"));
                    default_leader
                }
            },
            None => {
                warnings.push("leader must be a string; using Space".to_owned());
                default_leader
            }
        },
        None => default_leader,
    };
    config.bindings = keymap::default_bindings(leader);
    if let Some(value) = table.get("show_hidden") {
        match value.as_bool() {
            Some(show_hidden) => config.show_hidden = show_hidden,
            None => warnings.push("show_hidden must be true or false".to_owned()),
        }
    }
    if let Some(value) = table.get("sort") {
        match value.as_str() {
            Some("dirs_first") => config.sort = SortOrder::DirsFirst,
            Some("mixed") => config.sort = SortOrder::Mixed,
            _ => warnings.push("sort must be \"dirs_first\" or \"mixed\"".to_owned()),
        }
    }
    if let Some(value) = table.get("sort_key") {
        match value.as_str() {
            Some("name") => config.sort_key = SortKey::Name,
            Some("date") => config.sort_key = SortKey::Date,
            Some("size") => config.sort_key = SortKey::Size,
            Some("ext") => config.sort_key = SortKey::Extension,
            _ => warnings
                .push("sort_key must be \"name\", \"date\", \"size\", or \"ext\"".to_owned()),
        }
    }
    if let Some(value) = table.get("sort_reverse") {
        match value.as_bool() {
            Some(reverse) => config.sort_reverse = reverse,
            None => warnings.push("sort_reverse must be true or false".to_owned()),
        }
    }
    if let Some(value) = table.get("restore_session") {
        match value.as_bool() {
            Some(restore_session) => config.restore_session = restore_session,
            None => warnings.push("restore_session must be true or false".to_owned()),
        }
    }
    if let Some(value) = table.get("terminal") {
        match value.as_str() {
            Some("auto") => config.terminal = TerminalKind::Auto,
            Some("windows_terminal") => config.terminal = TerminalKind::WindowsTerminal,
            Some("powershell") => config.terminal = TerminalKind::PowerShell,
            Some("cmd") => config.terminal = TerminalKind::Cmd,
            _ => warnings.push(
                "terminal must be \"auto\", \"windows_terminal\", \"powershell\", or \"cmd\""
                    .to_owned(),
            ),
        }
    }
    if let Some(value) = table.get("feedback_url") {
        match value.as_str() {
            Some(url) => config.feedback_url = Some(url.to_owned()),
            None => warnings.push("feedback_url must be a string".to_owned()),
        }
    }
    if let Some(value) = table.get("statusline") {
        match value.as_table() {
            Some(statusline) => {
                for key in statusline.keys() {
                    if key != "left" && key != "right" {
                        warnings.push(format!("Ignoring unknown statusline key: {key}"));
                    }
                }
                if let Some(items) = parse_statusline_items(statusline.get("left"), &mut warnings) {
                    config.statusline_left = items;
                }
                if let Some(items) = parse_statusline_items(statusline.get("right"), &mut warnings)
                {
                    config.statusline_right = items;
                }
            }
            None => warnings.push("statusline must be a table".to_owned()),
        }
    }
    if let Some(value) = table.get("confirm_detail") {
        match value.as_str() {
            Some("full") => config.confirm_detail = ConfirmDetail::Full,
            Some("summary") => config.confirm_detail = ConfirmDetail::Summary,
            _ => warnings.push("confirm_detail must be \"full\" or \"summary\"".to_owned()),
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
                        "Ignoring font because it is not an absolute path: {}",
                        path.display()
                    ));
                }
            }
            None => warnings.push("font must be a string".to_owned()),
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
                        warnings.push(format!("Path for bookmark {name} must be a string"));
                        continue;
                    };
                    let path = PathBuf::from(path);
                    if !path.is_absolute() {
                        warnings.push(format!(
                            "Ignoring bookmark {name} because it is not an absolute path: {}",
                            path.display()
                        ));
                        continue;
                    }
                    config.bookmarks.push((name.clone(), path));
                }
            }
            None => warnings.push("bookmarks must be a table".to_owned()),
        }
    }
    if let Some(value) = table.get("keymap") {
        match value.as_table() {
            Some(keymap_table) => {
                let mut entries = Vec::new();
                for section in keymap_table.keys() {
                    if section != "normal" {
                        warnings.push(format!("Ignoring unsupported keymap section: {section}"));
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
                                        "keymap.normal entry {sequence:?} must be a string"
                                    )),
                                }
                            }
                        }
                        None => warnings.push("keymap.normal must be a table".to_owned()),
                    }
                }
                let (bindings, keymap_warnings) = keymap::resolve_bindings(leader, &entries);
                config.bindings = bindings;
                warnings.extend(
                    keymap_warnings
                        .into_iter()
                        .map(|warning| format!("keymap: {warning}")),
                );
            }
            None => warnings.push("keymap must be a table".to_owned()),
        }
    }

    for key in table.keys() {
        if !matches!(
            key.as_str(),
            "show_hidden"
                | "sort"
                | "sort_key"
                | "sort_reverse"
                | "restore_session"
                | "terminal"
                | "feedback_url"
                | "confirm_detail"
                | "font"
                | "bookmarks"
                | "leader"
                | "keymap"
                | "statusline"
        ) {
            warnings.push(format!("Ignoring unknown configuration key: {key}"));
        }
    }

    (config, warnings)
}

/// `[statusline]`の`left`/`right`配列を項目列へ変換する。値が無ければ`None`。
///
/// 配列でない場合と不正な項目名は警告して該当項目を無視する。
fn parse_statusline_items(
    value: Option<&toml::Value>,
    warnings: &mut Vec<String>,
) -> Option<Vec<StatusItem>> {
    let value = value?;
    let Some(array) = value.as_array() else {
        warnings.push("statusline entries must be an array of item names".to_owned());
        return None;
    };
    let mut items = Vec::with_capacity(array.len());
    for entry in array {
        match entry.as_str().and_then(StatusItem::from_config_name) {
            Some(item) => items.push(item),
            None => warnings.push(format!("Ignoring unknown statusline item: {entry}")),
        }
    }
    Some(items)
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
        anyhow::bail!("Recent root must be an absolute path: {}", root.display());
    }

    let directory = config_dir()?;
    fs::create_dir_all(&directory).with_context(|| {
        format!(
            "Failed to create configuration directory: {}",
            directory.display()
        )
    })?;

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
    fs::write(&temporary, contents).with_context(|| {
        format!(
            "Failed to write temporary configuration file: {}",
            temporary.display()
        )
    })?;
    if let Err(error) = fs::rename(&temporary, &target) {
        let _ = fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("Failed to replace {}", target.display()));
    }
    Ok(())
}

pub(crate) fn config_dir() -> anyhow::Result<PathBuf> {
    if let Some(path) = nonempty_env("FYLER_CONFIG_DIR") {
        return Ok(PathBuf::from(path));
    }

    #[cfg(windows)]
    {
        nonempty_env("APPDATA")
            .map(PathBuf::from)
            .map(|path| path.join("fyler"))
            .context("APPDATA is not set")
    }

    #[cfg(not(windows))]
    {
        if let Some(path) = nonempty_env("XDG_CONFIG_HOME") {
            return Ok(PathBuf::from(path).join("fyler"));
        }
        nonempty_env("HOME")
            .map(PathBuf::from)
            .map(|path| path.join(".config").join("fyler"))
            .context("Neither XDG_CONFIG_HOME nor HOME is set")
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
        fs::write(&path, "restore_session = false\n").unwrap();
        let (config, warnings) = load();
        assert!(!config.restore_session);
        assert!(warnings.is_empty(), "{warnings:?}");

        fs::write(&path, "restore_session = 'no'\n").unwrap();
        let (config, warnings) = load();
        assert!(config.restore_session);
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("restore_session"))
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

        fs::write(&path, "leader = 'x'\n").unwrap();
        let (config, warnings) = load();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(config.bindings.iter().any(|binding| {
            binding.sequence.to_string() == "xe"
                && binding.target
                    == keymap::BindingTarget::Action(keymap::EditorAction::ToggleDockFocus)
        }));

        fs::write(
            &path,
            "leader = '<Space>'\n[keymap.normal]\n'<leader>f' = 'file_picker'\n'gd' = 'none'\n",
        )
        .unwrap();
        let (config, warnings) = load();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(config.bindings.iter().any(|binding| {
            binding.sequence.to_string() == "<Space>f"
                && binding.target == keymap::BindingTarget::Action(keymap::EditorAction::FilePicker)
        }));
        assert!(
            !config
                .bindings
                .iter()
                .any(|binding| binding.sequence.to_string() == "gd")
        );

        fs::write(
            &path,
            "leader = 123\n[keymap.normal]\nx = 1\ny = 'not a real action'\n[keymap.visual]\nz = 'help'\n",
        )
        .unwrap();
        let (config, warnings) = load();
        assert_eq!(
            config.bindings,
            keymap::default_bindings(keymap::default_leader())
        );
        assert!(warnings.iter().any(|warning| warning.contains("leader")));
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("must be a string"))
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("not an action name"))
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("unsupported"))
        );

        fs::write(&path, "[bookmarks]\nrelative = 'child/path'\n").unwrap();
        let (config, warnings) = load();
        assert!(config.bookmarks.is_empty());
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("not an absolute path"))
        );

        fs::write(
            &path,
            "font = true\nicons = 1\nfont_y_offset_factor = 'low'\n",
        )
        .unwrap();
        let (config, warnings) = load();
        assert_eq!(config.font, None);
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("font must be a string"))
        );
        // 廃止済みキーは未知キー警告になる。
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("unknown configuration key: icons"))
        );
        assert!(warnings.iter().any(|warning| {
            warning.contains("unknown configuration key: font_y_offset_factor")
        }));

        fs::write(&path, "font = 'relative/font.ttf'\n").unwrap();
        let (config, warnings) = load();
        assert_eq!(config.font, None);
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("not an absolute path"))
        );

        fs::write(
            &path,
            "[statusline]\nleft = ['mode', 'path']\nright = ['percent']\n",
        )
        .unwrap();
        let (config, warnings) = load();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(config.statusline_left, [StatusItem::Mode, StatusItem::Path]);
        assert_eq!(config.statusline_right, [StatusItem::Percent]);

        fs::write(
            &path,
            "[statusline]\nleft = ['mode', 'bogus']\nright = 'percent'\ntop = ['mode']\n",
        )
        .unwrap();
        let (config, warnings) = load();
        assert_eq!(config.statusline_left, [StatusItem::Mode]);
        assert_eq!(
            config.statusline_right,
            fyler_core::options::default_statusline_right()
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("unknown statusline item"))
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("must be an array"))
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("unknown statusline key"))
        );

        let first = directory.path().join("z-first");
        let second = directory.path().join("a-second");
        let font = directory.path().join("JapaneseFont.ttf");
        fs::write(
            &path,
            format!(
                "show_hidden = true\nsort = \"mixed\"\nconfirm_detail = \"summary\"\n\
                 sort_key = \"date\"\nsort_reverse = true\n\
                 font = '{}'\n\
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
}
