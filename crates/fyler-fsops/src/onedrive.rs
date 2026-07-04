//! OneDriveプレースホルダ対応(DESIGN.md「その他の対応事項」)。M5。

use std::path::Path;

/// クラウドプレースホルダ(中身がローカルにないファイル)を示す属性。
/// このファイルのデータを読むとhydration(リモート取得)が発生する。
pub const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;

/// ファイルを開いた時点でクラウドからの取得が必要なことを示す属性。
pub const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;

/// ファイルの内容がローカルにないことを示す属性。
pub const FILE_ATTRIBUTE_OFFLINE: u32 = 0x0000_1000;

/// パスがクラウドプレースホルダかどうか。
///
/// 実装契約:
/// - 属性取得のみで判定する(データを読まない)
/// - サイズ取得・プレビュー・ハッシュ等、**内容に触れる処理の前に必ずこれを確認**し、
///   プレースホルダに対しては不要なhydrationを発生させない
pub fn is_cloud_placeholder(path: &Path) -> anyhow::Result<bool> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        use anyhow::bail;
        use windows::Win32::Foundation::GetLastError;
        use windows::Win32::Storage::FileSystem::GetFileAttributesW;
        use windows::core::PCWSTR;

        let wide_path: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let attributes = unsafe { GetFileAttributesW(PCWSTR(wide_path.as_ptr())) };
        if attributes == u32::MAX {
            let error = unsafe { GetLastError() };
            bail!(
                "ファイル属性を取得できません: {}: GetLastError={} ({})",
                path.display(),
                error.0,
                std::io::Error::from_raw_os_error(error.0 as i32)
            );
        }

        let cloud_attributes = FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
            | FILE_ATTRIBUTE_RECALL_ON_OPEN
            | FILE_ATTRIBUTE_OFFLINE;
        Ok(attributes & cloud_attributes != 0)
    }

    #[cfg(not(windows))]
    {
        let _ = path;
        Ok(false)
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn ordinary_file_is_not_a_cloud_placeholder() {
        let root = tempdir().unwrap();
        let path = root.path().join("ordinary.txt");
        fs::write(&path, b"content").unwrap();

        assert!(!is_cloud_placeholder(&path).unwrap());
    }
}
