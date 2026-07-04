//! バッファ文字列に含めないツリーアイコン装飾。

use std::path::Path;

use fyler_core::grammar;

pub const DIRECTORY: &str = "D";
pub const FILE: &str = "F";
pub const RUST: &str = "R";
pub const MARKDOWN: &str = "M";
pub const TEXT: &str = "T";
pub const CONFIG: &str = "C";

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
}
