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

/// バイト数を人間可読形式("2.0 KB" / "1.5 MB")にする。
pub fn human_readable_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= MIB {
        format!("{:.1} MB", bytes / MIB)
    } else {
        format!("{:.1} KB", bytes / KIB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_readable_size_uses_kibibyte_and_mebibyte_boundaries() {
        assert_eq!(human_readable_size(0), "0.0 KB");
        assert_eq!(human_readable_size(1024), "1.0 KB");
        assert_eq!(human_readable_size(1024 * 1024 - 1), "1024.0 KB");
        assert_eq!(human_readable_size(1024 * 1024), "1.0 MB");
        assert_eq!(human_readable_size(1536 * 1024), "1.5 MB");
    }
}
