//! case-onlyリネーム(`Foo → foo`)対応(DESIGN.md「その他の対応事項」)。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, bail};

static TEMP_NAME_COUNTER: AtomicU64 = AtomicU64::new(0);

/// ディレクトリが大文字小文字を区別する設定かどうかを返す。
///
/// 実装契約(Windows): Windowsは**ディレクトリ単位**でcase-sensitiveにできる
/// (`FILE_CASE_SENSITIVE_DIR`)。名前衝突の判定は対象ディレクトリの実際の
/// 設定に合わせること(グローバルに大文字小文字無視と決めつけない)。
///
/// Windowsではディレクトリを開いて`FileCaseSensitiveInfo`を照会する。ハンドルの
/// オープンまたは照会に失敗した場合は、保守的にcase-insensitiveとして`Ok(false)`へ
/// フォールバックする。非Windowsではcase-sensitiveを返す。
///
/// validate層は純粋性を維持して保守的なcase-fold重複検出を行い、実FS設定による
/// 精緻化はapply直前のpreflightだけで行う。
pub fn dir_is_case_sensitive(dir: &Path) -> anyhow::Result<bool> {
    #[cfg(windows)]
    {
        use std::ffi::c_void;
        use std::mem::size_of;
        use std::os::windows::ffi::OsStrExt;

        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::Storage::FileSystem::{
            CreateFileW, FILE_CASE_SENSITIVE_INFO, FILE_FLAG_BACKUP_SEMANTICS,
            FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            FileCaseSensitiveInfo, GetFileInformationByHandleEx, OPEN_EXISTING,
        };
        use windows::Win32::System::SystemServices::FILE_CS_FLAG_CASE_SENSITIVE_DIR;
        use windows::core::PCWSTR;

        let fs_dir = crate::long_path::to_fs(dir);
        let wide_dir: Vec<u16> = fs_dir
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let handle = unsafe {
            CreateFileW(
                PCWSTR::from_raw(wide_dir.as_ptr()),
                FILE_READ_ATTRIBUTES.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                None,
            )
        };
        let Ok(handle) = handle else {
            return Ok(false);
        };

        let mut info = FILE_CASE_SENSITIVE_INFO::default();
        let query = unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileCaseSensitiveInfo,
                (&raw mut info).cast::<c_void>(),
                size_of::<FILE_CASE_SENSITIVE_INFO>() as u32,
            )
        };
        let _ = unsafe { CloseHandle(handle) };
        if query.is_err() {
            return Ok(false);
        }

        Ok(info.Flags & FILE_CS_FLAG_CASE_SENSITIVE_DIR != 0)
    }

    #[cfg(not(windows))]
    {
        let _ = dir;
        Ok(true)
    }
}

/// case-onlyリネームを実行する。
///
/// 実装契約:
/// - case-insensitiveなディレクトリでは `Foo → foo` が同名扱いで失敗し得るため、
///   **temp名経由の2段rename**で行う(`Foo → .fyler-tmp-XXXX → foo`)
/// - temp名は同一ディレクトリ内で衝突しない名前を生成する
/// - 1段目成功後に2段目が失敗した場合は1段目を巻き戻す(この操作内のみロールバック)
pub fn case_only_rename(from: &Path, to: &Path) -> anyhow::Result<()> {
    let parent = from
        .parent()
        .context("Source of case-only rename has no parent directory")?;
    if to.parent() != Some(parent) {
        bail!("Source and destination of case-only rename have different parent directories");
    }

    let temporary = unique_temporary_path(parent);
    fs::rename(
        crate::long_path::to_fs(from),
        crate::long_path::to_fs(&temporary),
    )
    .with_context(|| {
        format!(
            "First stage of case-only rename failed: {} → {}",
            from.display(),
            temporary.display()
        )
    })?;

    if let Err(rename_error) = fs::rename(
        crate::long_path::to_fs(&temporary),
        crate::long_path::to_fs(to),
    ) {
        return match fs::rename(
            crate::long_path::to_fs(&temporary),
            crate::long_path::to_fs(from),
        ) {
            Ok(()) => Err(anyhow::anyhow!(
                "Second stage of case-only rename failed and the original name was restored: {} → {}: {rename_error}",
                temporary.display(),
                to.display()
            )),
            Err(rollback_error) => Err(anyhow::anyhow!(
                "case-only renameの二段目と巻き戻しに失敗しました: {} → {}: \
                 {rename_error}; {} → {}: {rollback_error}",
                temporary.display(),
                to.display(),
                temporary.display(),
                from.display()
            )),
        };
    }

    Ok(())
}

fn unique_temporary_path(parent: &Path) -> PathBuf {
    loop {
        let sequence = TEMP_NAME_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".fyler-tmp-{}-{sequence}", std::process::id()));
        if !crate::long_path::to_fs(&candidate).exists() {
            return candidate;
        }
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn directories_are_case_sensitive_off_windows() {
        let root = tempdir().unwrap();
        assert!(dir_is_case_sensitive(root.path()).unwrap());
    }
}
