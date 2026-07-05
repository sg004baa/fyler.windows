//! バッファ文字列に含めないツリーアイコン装飾。

use std::path::Path;

use fyler_core::grammar;

use crate::confirm::IconStyle;

pub const DIRECTORY: &str = "D";
pub const FILE: &str = "F";
pub const RUST: &str = "R";
pub const MARKDOWN: &str = "M";
pub const TEXT: &str = "T";
pub const CONFIG: &str = "C";

const NERD_DIRECTORY: &str = "\u{f4d3}";
const NERD_FILE: &str = "\u{f4a5}";
const NERD_RUST: &str = "\u{e7a8}";
const NERD_MARKDOWN: &str = "\u{e73e}";
const NERD_TEXT: &str = "\u{f15c}";
const NERD_CONFIG: &str = "\u{e615}";

/// conceal済みの表示名に対応するASCIIアイコンを返す。
///
/// ASCIIだけを使い、eguiの既定フォントでも欠けないようにする。
pub fn for_display_name(display_name: &str) -> &'static str {
    let (name, is_dir) = grammar::split_dir_suffix(display_name);
    if is_dir {
        return DIRECTORY;
    }

    match Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
    {
        Some(extension) if extension.eq_ignore_ascii_case("rs") => RUST,
        Some(extension) if extension.eq_ignore_ascii_case("md") => MARKDOWN,
        Some(extension) if extension.eq_ignore_ascii_case("txt") => TEXT,
        Some(extension) if extension.eq_ignore_ascii_case("toml") => CONFIG,
        _ => FILE,
    }
}

/// conceal済みの表示名と指定スタイルに対応するアイコンを返す。
///
/// Nerd Font非対応フォントではNerdアイコンがtofuになる。既定は
/// [`IconStyle::Ascii`]で、ユーザーが`config.toml`で明示的に有効化する。
pub fn for_display_name_styled(display_name: &str, style: IconStyle) -> &'static str {
    if style == IconStyle::Ascii {
        return for_display_name(display_name);
    }

    let (name, is_dir) = grammar::split_dir_suffix(display_name);
    if is_dir {
        return NERD_DIRECTORY;
    }

    match Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
    {
        Some(extension) if extension.eq_ignore_ascii_case("rs") => NERD_RUST,
        Some(extension) if extension.eq_ignore_ascii_case("md") => NERD_MARKDOWN,
        Some(extension) if extension.eq_ignore_ascii_case("txt") => NERD_TEXT,
        Some(extension) if extension.eq_ignore_ascii_case("toml") => NERD_CONFIG,
        _ => NERD_FILE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_directory_icon_from_grammar_suffix() {
        assert_eq!(for_display_name("  src/"), DIRECTORY);
    }

    #[test]
    fn selects_icons_for_supported_extensions_case_insensitively() {
        assert_eq!(for_display_name("main.rs"), RUST);
        assert_eq!(for_display_name("README.MD"), MARKDOWN);
        assert_eq!(for_display_name("notes.txt"), TEXT);
        assert_eq!(for_display_name("Cargo.toml"), CONFIG);
    }

    #[test]
    fn selects_general_file_icon_for_other_files() {
        assert_eq!(for_display_name("archive.zip"), FILE);
        assert_eq!(for_display_name("LICENSE"), FILE);
    }

    #[test]
    fn styled_ascii_icons_match_existing_mapping() {
        for name in [
            "src/",
            "main.rs",
            "README.md",
            "notes.txt",
            "Cargo.toml",
            "archive.zip",
        ] {
            assert_eq!(
                for_display_name_styled(name, IconStyle::Ascii),
                for_display_name(name)
            );
        }
    }

    #[test]
    fn styled_nerd_icons_cover_directories_and_supported_extensions() {
        assert_eq!(
            for_display_name_styled("src/", IconStyle::Nerd),
            NERD_DIRECTORY
        );
        assert_eq!(
            for_display_name_styled("main.rs", IconStyle::Nerd),
            NERD_RUST
        );
        assert_eq!(
            for_display_name_styled("README.MD", IconStyle::Nerd),
            NERD_MARKDOWN
        );
        assert_eq!(
            for_display_name_styled("notes.txt", IconStyle::Nerd),
            NERD_TEXT
        );
        assert_eq!(
            for_display_name_styled("Cargo.toml", IconStyle::Nerd),
            NERD_CONFIG
        );
        assert_eq!(
            for_display_name_styled("archive.zip", IconStyle::Nerd),
            NERD_FILE
        );
    }
}
