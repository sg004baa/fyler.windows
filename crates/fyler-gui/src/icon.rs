//! バッファ文字列に含めないツリーアイコン装飾。
//!
//! アイコンは常に組み込みNerd Fontグリフを使う。`config.font`の指定に
//! 左右されないよう、専用フォントファミリ([`font_family`])で描画する。

use std::path::Path;

use eframe::egui;
use fyler_core::grammar;

const DIRECTORY: &str = " ";
const DIRECTORY_OPEN: &str = " ";
const FILE: &str = " ";
const RUST: &str = " ";
const MARKDOWN: &str = " ";
const TEXT: &str = " ";
const TOML: &str = " ";

/// アイコン描画専用のフォントファミリ名。組み込みフォントだけを含み、
/// `config.font`に影響されずアイコンが同一に描画されることを保証する。
pub const FONT_FAMILY_NAME: &str = "fyler-icons";

/// アイコン描画専用のフォントファミリを返す。
pub fn font_family() -> egui::FontFamily {
    egui::FontFamily::Name(FONT_FAMILY_NAME.into())
}

/// 左ドック(ブックマーク・ドライブ)用のディレクトリアイコン。
pub fn directory() -> &'static str {
    DIRECTORY
}

/// conceal済みの表示名と展開状態に対応するNerd Fontアイコンを返す。
///
/// ディレクトリは展開状態でグリフを切り替える。拡張子判定は大文字小文字を
/// 区別しない。
pub fn for_display_name(display_name: &str, expanded: bool) -> &'static str {
    let (name, is_dir) = grammar::split_dir_suffix(display_name);
    if is_dir {
        return if expanded { DIRECTORY_OPEN } else { DIRECTORY };
    }

    match Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
    {
        Some(extension) if extension.eq_ignore_ascii_case("rs") => RUST,
        Some(extension) if extension.eq_ignore_ascii_case("md") => MARKDOWN,
        Some(extension) if extension.eq_ignore_ascii_case("txt") => TEXT,
        Some(extension) if extension.eq_ignore_ascii_case("toml") => TOML,
        _ => FILE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_directory_icon_from_grammar_suffix() {
        assert_eq!(for_display_name("  src/", false), DIRECTORY);
    }

    #[test]
    fn directory_icon_changes_by_expanded_state() {
        assert_eq!(for_display_name("src/", false), DIRECTORY);
        assert_eq!(for_display_name("src/", true), DIRECTORY_OPEN);
        // ファイルは展開状態に影響されない。
        assert_eq!(for_display_name("main.rs", true), RUST);
        assert_eq!(for_display_name("main.rs", false), RUST);
    }

    #[test]
    fn selects_icons_for_supported_extensions_case_insensitively() {
        assert_eq!(for_display_name("main.rs", false), RUST);
        assert_eq!(for_display_name("README.MD", false), MARKDOWN);
        assert_eq!(for_display_name("notes.TXT", false), TEXT);
        assert_eq!(for_display_name("Cargo.TOML", false), TOML);
    }

    #[test]
    fn selects_general_file_icon_for_other_files() {
        assert_eq!(for_display_name("archive.zip", false), FILE);
        assert_eq!(for_display_name("LICENSE", false), FILE);
    }
}
