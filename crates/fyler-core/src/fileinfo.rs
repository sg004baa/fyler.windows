//! baselineエントリの表示用メタデータ。

/// baselineエントリの表示用メタデータ。fsopsが実FSから収集し、GUIがモードラインに描く。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileInfo {
    /// バイトサイズ。ディレクトリはNone。
    pub size: Option<u64>,
    /// ローカル時刻で整形済みの更新日時 "YYYY-MM-DD HH:MM"。取得失敗はNone。
    pub modified: Option<String>,
    /// クラウドプレースホルダ(OneDrive等)。trueなら内容がローカルにない。
    pub is_placeholder: bool,
}

/// スキャン時に列挙メタデータから収集する生の表示用メタデータ。
///
/// [`FileInfo`] との違いは、更新日時を未整形の [`std::time::SystemTime`] で
/// 保持すること。整形は表示対象に限定して行う(全エントリ分の文字列生成を
/// スキャン時に発生させない)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryMeta {
    /// バイトサイズ。ディレクトリはNone。
    pub size: Option<u64>,
    /// 更新日時。取得できないファイルシステムではNone。
    pub modified: Option<std::time::SystemTime>,
    /// クラウドプレースホルダ(OneDrive等)。trueなら内容がローカルにない。
    pub is_placeholder: bool,
}

/// バイト数を人間可読形式(`"512 B"` / `"2.0 KB"` / `"1.5 MB"` / `"2.3 GB"`)にする。
/// 全クレート共通の正典フォーマッタ(行表示・modeline・backup警告が共有する)。
/// 1024バイト未満は整数バイト表記、それ以上は1024進で小数点1桁に整形する。
pub fn human_readable_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let bytes = bytes as f64;
    if bytes >= GIB {
        format!("{:.1} GB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{:.1} MB", bytes / MIB)
    } else {
        format!("{:.1} KB", bytes / KIB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_readable_size_uses_byte_kibibyte_mebibyte_and_gibibyte_boundaries() {
        assert_eq!(human_readable_size(0), "0 B");
        assert_eq!(human_readable_size(512), "512 B");
        assert_eq!(human_readable_size(1023), "1023 B");
        assert_eq!(human_readable_size(1024), "1.0 KB");
        assert_eq!(human_readable_size(1024 * 1024 - 1), "1024.0 KB");
        assert_eq!(human_readable_size(1024 * 1024), "1.0 MB");
        assert_eq!(human_readable_size(1536 * 1024), "1.5 MB");
        assert_eq!(human_readable_size(1024 * 1024 * 1024 - 1), "1024.0 MB");
        assert_eq!(human_readable_size(1024 * 1024 * 1024), "1.0 GB");
        assert_eq!(human_readable_size(1024 * 1024 * 1024 * 3 / 2), "1.5 GB");
    }
}
