//! Windowsのファイル名規則の正典。**実装済み。再実装禁止。**
//!
//! DESIGN.md「validateで弾くもの」に対応する。validate層(fyler-pipeline)は
//! 必ずこのモジュールを使うこと。
//!
//! 注意: case-onlyリネームの衝突判定はここではなくfsops層の責務
//! (対象ディレクトリの実際のcase sensitivity `FILE_CASE_SENSITIVE_DIR` に従うため)。

/// Windowsでファイル名に使えない予約文字。
///
/// `/` はバッファ文法上ディレクトリサフィックス・IDプレフィックスにも使われるため、
/// parse後の「名前」に含まれていれば常に不正。
pub const RESERVED_CHARS: [char; 9] = ['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Windowsの予約デバイス名(拡張子を付けても不正。例: `CON.txt` も不可)。
pub const RESERVED_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", //
    "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", //
    "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// 名前に含まれる最初の不正文字を返す(予約文字または制御文字)。
pub fn find_reserved_char(name: &str) -> Option<char> {
    name.chars()
        .find(|c| RESERVED_CHARS.contains(c) || (*c as u32) < 0x20)
}

/// Windowsの予約名かどうか(大文字小文字を無視、拡張子付きも予約扱い)。
pub fn is_reserved_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    RESERVED_NAMES.iter().any(|r| stem.eq_ignore_ascii_case(r))
}

/// 名前の末尾が不正(スペースまたはピリオド)かどうか。
pub fn has_invalid_trailing(name: &str) -> bool {
    name.ends_with(' ') || name.ends_with('.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_chars() {
        assert_eq!(find_reserved_char("a<b.txt"), Some('<'));
        assert_eq!(find_reserved_char("a\tb"), Some('\t')); // 制御文字も不正
        assert_eq!(find_reserved_char("普通の名前.txt"), None);
    }

    #[test]
    fn reserved_names() {
        assert!(is_reserved_name("CON"));
        assert!(is_reserved_name("con"));
        assert!(is_reserved_name("Con.txt")); // 拡張子付きも予約
        assert!(is_reserved_name("lpt9.log"));
        assert!(!is_reserved_name("CONSOLE"));
        assert!(!is_reserved_name("COM10"));
        assert!(!is_reserved_name(""));
    }

    #[test]
    fn invalid_trailing() {
        assert!(has_invalid_trailing("name "));
        assert!(has_invalid_trailing("name."));
        assert!(!has_invalid_trailing("name.txt"));
    }
}
