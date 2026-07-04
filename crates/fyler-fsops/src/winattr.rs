//! Win32のファイル属性取得を共有する内部ヘルパー。

#[cfg(windows)]
use std::path::Path;

/// `GetFileAttributesW`でパスの属性を取得する。
///
/// 呼び出し側は属性ビットの意味だけを判断し、Win32 APIの呼び出し手順と
/// エラー変換を重複実装しない。
#[cfg(windows)]
pub(crate) fn get(path: &Path) -> anyhow::Result<u32> {
    use std::os::windows::ffi::OsStrExt;

    use anyhow::bail;
    use windows::Win32::Foundation::GetLastError;
    use windows::Win32::Storage::FileSystem::GetFileAttributesW;
    use windows::core::PCWSTR;

    let fs_path = crate::long_path::to_fs(path);
    let wide_path: Vec<u16> = fs_path
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

    Ok(attributes)
}
