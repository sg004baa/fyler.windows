//! カーソル行dirのオンデマンド再帰サイズ計算(issue #38フォローアップ、案B)。
//!
//! ディレクトリサイズを返すWindows APIはなく、常時表示のための再帰列挙は
//! M13のlazy scanと衝突するため行わない。代わりに、明示アクションでカーソル行の
//! ディレクトリ1つだけを背景スレッドで再帰合算する([`dir_size_cancellable`])。

use std::fs::{self, ReadDir};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context;

use crate::scan::is_link_or_reparse;

/// [`dir_size_cancellable`] の合算結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirSizeOutcome {
    /// 読めたファイルサイズの合算(バイト)。
    pub total: u64,
    /// 合算対象になったファイル数。
    pub files: usize,
    /// 列挙できずスキップしたサブディレクトリ数。
    pub unreadable_dirs: usize,
}

/// `dir` 以下を再帰的に列挙し、ファイルサイズを合算する。
///
/// 実装契約:
/// - `Ok(None)` はキャンセル。ルート `dir` 自体が列挙できない場合は `Err`(fail-fast)。
/// - サブディレクトリの列挙に失敗した場合は結果の `unreadable_dirs` へ数えて
///   スキップする(silent fallbackにしない)。列挙とmetadata取得の間に消えた
///   `NotFound` レースは無視してスキップする(`scan`モジュールと同じ契約)。
/// - symlink・junction・reparse pointは辿らない。リンク自体もサイズへ加えない。
/// - OneDriveプレースホルダは`DirEntry::metadata`の`len()`だけを読み、
///   hydrate(リモート取得)を発生させない。
/// - パス変換は[`crate::long_path::to_fs`]経由のみ(このモジュールへ`\\?\`を書かない)。
/// - 再帰は明示スタック(`Vec`)で実装する(深い木でのスタックオーバーフロー回避)。
///   cancelは列挙ループ内で毎エントリ確認する。
pub fn dir_size_cancellable(
    dir: &Path,
    cancel: &AtomicBool,
) -> anyhow::Result<Option<DirSizeOutcome>> {
    if cancel.load(Ordering::Relaxed) {
        return Ok(None);
    }
    let root_metadata = fs::symlink_metadata(crate::long_path::to_fs(dir))
        .with_context(|| format!("Failed to get metadata: {}", dir.display()))?;
    if is_link_or_reparse(&root_metadata) {
        anyhow::bail!(
            "Cannot compute the size of a symlink, junction, or reparse point: {}",
            dir.display()
        );
    }
    if !root_metadata.is_dir() {
        anyhow::bail!("Not a directory: {}", dir.display());
    }

    let mut total: u64 = 0;
    let mut files: usize = 0;
    let mut unreadable_dirs: usize = 0;
    let mut stack: Vec<PathBuf> = Vec::new();

    // ルート自体の列挙失敗はサブディレクトリと違い fail-fast(unreadable_dirsに
    // 数えない)。呼び出し側は「対象が丸ごと読めない」ことをエラーとして扱う。
    let root_entries = fs::read_dir(crate::long_path::to_fs(dir))
        .with_context(|| format!("Failed to enumerate directory: {}", dir.display()))?;
    if !visit_directory(
        root_entries,
        dir,
        cancel,
        &mut total,
        &mut files,
        &mut stack,
    ) {
        return Ok(None);
    }

    while let Some(current) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        match fs::read_dir(crate::long_path::to_fs(&current)) {
            Ok(entries) => {
                if !visit_directory(
                    entries, &current, cancel, &mut total, &mut files, &mut stack,
                ) {
                    return Ok(None);
                }
            }
            Err(_) => {
                // 列挙できないサブディレクトリはスキップして兄弟の走査を続ける。
                // silent fallbackにしないため、件数を呼び出し元へ明示的に返す。
                unreadable_dirs += 1;
            }
        }
    }

    Ok(Some(DirSizeOutcome {
        total,
        files,
        unreadable_dirs,
    }))
}

/// `entries` の子を合算し、子ディレクトリを`stack`へ積む。`false`はキャンセルされたことを示す。
fn visit_directory(
    entries: ReadDir,
    parent: &Path,
    cancel: &AtomicBool,
    total: &mut u64,
    files: &mut usize,
    stack: &mut Vec<PathBuf>,
) -> bool {
    for entry in entries {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        let entry = match entry {
            Ok(entry) => entry,
            // 列挙とmetadata取得の間に消えたraceは次回計算で自然に収束する。
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => continue,
        };
        // `DirEntry::metadata`は追加のディレクトリ列挙(long_path変換)を伴わず、
        // symlinkを辿らないstat相当の呼び出しなので、OneDriveプレースホルダを
        // hydrateしない。
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => continue,
        };
        if is_link_or_reparse(&metadata) {
            continue;
        }
        if metadata.is_dir() {
            // `entry.path()`はread_dirへ渡したlong_path変換済み親パス由来になり得る
            // ため使わず、呼び出し側の論理パスから組み立てる(絶対ルール3)。
            stack.push(parent.join(entry.file_name()));
        } else {
            *total = total.saturating_add(metadata.len());
            *files += 1;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn sums_nested_directories() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("a.txt"), b"hello").unwrap(); // 5 bytes
        fs::create_dir(root.path().join("sub")).unwrap();
        fs::write(root.path().join("sub").join("b.txt"), b"world!").unwrap(); // 6 bytes
        fs::create_dir(root.path().join("sub").join("deeper")).unwrap();
        fs::write(root.path().join("sub").join("deeper").join("c.txt"), b"abc").unwrap(); // 3 bytes

        let cancel = AtomicBool::new(false);
        let outcome = dir_size_cancellable(root.path(), &cancel).unwrap().unwrap();

        assert_eq!(outcome.total, 5 + 6 + 3);
        assert_eq!(outcome.files, 3);
        assert_eq!(outcome.unreadable_dirs, 0);
    }

    #[test]
    #[cfg(unix)]
    fn does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let outer = tempdir().unwrap();
        let root = tempdir().unwrap();
        fs::write(root.path().join("real.txt"), b"12345").unwrap(); // 5 bytes
        // symlinkの実体はscan対象rootの外に置く。root内にも実ディレクトリとして
        // 存在してしまうと「辿らない」ことの検証にならない。
        fs::create_dir(outer.path().join("target_dir")).unwrap();
        fs::write(
            outer.path().join("target_dir").join("big.txt"),
            b"0123456789",
        )
        .unwrap(); // 10 bytes, must not be counted
        symlink(
            outer.path().join("target_dir"),
            root.path().join("link_to_dir"),
        )
        .unwrap();
        symlink(
            root.path().join("real.txt"),
            root.path().join("link_to_file.txt"),
        )
        .unwrap();

        let cancel = AtomicBool::new(false);
        let outcome = dir_size_cancellable(root.path(), &cancel).unwrap().unwrap();

        // real.txtのみが合算対象。symlink先(target_dir/big.txtとreal.txt自体への
        // link_to_file.txt)は辿らない・加算しない。
        assert_eq!(outcome.total, 5);
        assert_eq!(outcome.files, 1);
    }

    #[test]
    fn cancel_flag_returns_none_immediately() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("a.txt"), b"hello").unwrap();

        let cancel = AtomicBool::new(true);
        let outcome = dir_size_cancellable(root.path(), &cancel).unwrap();

        assert!(outcome.is_none());
    }

    #[test]
    fn missing_root_is_an_error() {
        let root = tempdir().unwrap();
        let missing = root.path().join("does-not-exist");

        let cancel = AtomicBool::new(false);
        let result = dir_size_cancellable(&missing, &cancel);

        assert!(result.is_err());
    }

    #[test]
    #[cfg(unix)]
    fn unreadable_subdirectory_is_counted_and_skipped() {
        use std::os::unix::fs::PermissionsExt;

        struct RestorePermissions(PathBuf, u32);
        impl Drop for RestorePermissions {
            fn drop(&mut self) {
                let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(self.1));
            }
        }

        let root = tempdir().unwrap();
        let blocked = root.path().join("blocked");
        fs::create_dir(&blocked).unwrap();
        fs::write(blocked.join("hidden.txt"), b"hidden-content").unwrap();
        fs::write(root.path().join("visible.txt"), b"12345").unwrap(); // 5 bytes

        let original_mode = fs::metadata(&blocked).unwrap().permissions().mode();
        let _restore = RestorePermissions(blocked.clone(), original_mode);
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&blocked).is_ok() {
            // rootで実行しているなど、permissionが効かない環境ではskip。
            return;
        }

        let cancel = AtomicBool::new(false);
        let outcome = dir_size_cancellable(root.path(), &cancel).unwrap().unwrap();

        assert_eq!(outcome.total, 5);
        assert_eq!(outcome.files, 1);
        assert_eq!(outcome.unreadable_dirs, 1);
    }
}
