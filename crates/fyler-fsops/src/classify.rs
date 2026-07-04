//! Move操作の内部3分類(DESIGN.md「操作種別の内部分類」)。
//!
//! `std::fs::rename` は別ボリューム間で失敗する。MoveFileExWもディレクトリは
//! 同一ドライブが必要。そのため実行前に必ず分類する。

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use fyler_core::tree::EntryKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveClass {
    /// 同一ボリューム内のrename。**原子的**。
    SameVolumeRename,
    /// 別ボリュームへのファイル移動 = copy + delete。**非原子的**。
    CrossVolumeFileMove,
    /// 別ボリュームへのディレクトリ移動 = 再帰copy + delete。**非原子的**で
    /// 途中失敗時の挙動が異なる(どこまでコピー/削除できたかをprogressで報告する)。
    CrossVolumeDirectoryMove,
}

/// 移動元と移動先のボリュームを比較して分類する。
///
/// 実装契約(Windows): ボリューム判定は `GetVolumePathNameW` 等で
/// 実際のマウントポイントを比較する(ドライブレターの文字比較だけでは
/// junction・マウントされたボリュームで誤判定する)。
///
/// 移動先はまだ存在しないため、その親から存在する最も近い祖先まで遡って
/// ボリュームを判定する。存在する祖先が見つからない場合やmetadataを取得
/// できない場合は、renameを試行せずエラーを返す。
pub fn classify_move(from: &Path, to: &Path, kind: EntryKind) -> anyhow::Result<MoveClass> {
    let same_volume = same_volume(from, to)?;
    Ok(match (same_volume, kind) {
        (true, _) => MoveClass::SameVolumeRename,
        (false, EntryKind::Dir) => MoveClass::CrossVolumeDirectoryMove,
        (false, EntryKind::File | EntryKind::Symlink) => MoveClass::CrossVolumeFileMove,
    })
}

#[cfg(windows)]
fn same_volume(from: &Path, to: &Path) -> anyhow::Result<bool> {
    let from_probe = nearest_existing_path(from)?;
    let to_parent = parent_or_current_dir(to);
    let to_probe = nearest_existing_directory(to_parent)?;
    let from_volume = volume_mount_point(&from_probe)?;
    let to_volume = volume_mount_point(&to_probe)?;

    Ok(windows_paths_equal(&from_volume, &to_volume))
}

#[cfg(not(windows))]
fn same_volume(from: &Path, to: &Path) -> anyhow::Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let from_parent = nearest_existing_directory(parent_or_current_dir(from))?;
    let to_parent = nearest_existing_directory(parent_or_current_dir(to))?;
    let from_device = fs::metadata(&from_parent)
        .with_context(|| {
            format!(
                "移動元の親ディレクトリのmetadataを取得できません: {}",
                from_parent.display()
            )
        })?
        .dev();
    let to_device = fs::metadata(&to_parent)
        .with_context(|| {
            format!(
                "移動先の親ディレクトリのmetadataを取得できません: {}",
                to_parent.display()
            )
        })?
        .dev();

    Ok(from_device == to_device)
}

fn parent_or_current_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn nearest_existing_directory(start: &Path) -> anyhow::Result<PathBuf> {
    for candidate in start.ancestors() {
        let candidate = if candidate.as_os_str().is_empty() {
            Path::new(".")
        } else {
            candidate
        };
        match fs::metadata(candidate) {
            Ok(metadata) if metadata.is_dir() => return Ok(candidate.to_path_buf()),
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "ボリューム判定用のディレクトリを確認できません: {}",
                        candidate.display()
                    )
                });
            }
        }
    }

    bail!(
        "ボリューム判定に使える既存の祖先ディレクトリがありません: {}",
        start.display()
    )
}

#[cfg(windows)]
fn nearest_existing_path(start: &Path) -> anyhow::Result<PathBuf> {
    for candidate in start.ancestors() {
        let candidate = if candidate.as_os_str().is_empty() {
            Path::new(".")
        } else {
            candidate
        };
        match fs::symlink_metadata(candidate) {
            Ok(_) => return Ok(candidate.to_path_buf()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "ボリューム判定用のパスを確認できません: {}",
                        candidate.display()
                    )
                });
            }
        }
    }

    bail!(
        "ボリューム判定に使える既存の移動元パスがありません: {}",
        start.display()
    )
}

#[cfg(windows)]
fn volume_mount_point(path: &Path) -> anyhow::Result<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    use windows::Win32::Storage::FileSystem::GetVolumePathNameW;
    use windows::core::PCWSTR;

    let path_wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut volume_wide = vec![0_u16; 32_768];
    unsafe { GetVolumePathNameW(PCWSTR::from_raw(path_wide.as_ptr()), &mut volume_wide) }
        .with_context(|| {
            format!(
                "ボリュームのマウントポイントを取得できません: {}",
                path.display()
            )
        })?;

    let length = volume_wide
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(volume_wide.len());
    Ok(PathBuf::from(OsString::from_wide(&volume_wide[..length])))
}

#[cfg(windows)]
fn windows_paths_equal(left: &Path, right: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;

    left.as_os_str()
        .encode_wide()
        .map(|unit| unit.to_ascii_lowercase())
        .eq(right
            .as_os_str()
            .encode_wide()
            .map(|unit| unit.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[cfg(not(windows))]
    #[test]
    fn classifies_paths_in_same_tempdir_as_same_volume() {
        let root = tempdir().unwrap();
        let source = root.path().join("from.txt");
        fs::write(&source, b"content").unwrap();

        let class = classify_move(
            &source,
            &root.path().join("missing/to.txt"),
            EntryKind::File,
        )
        .unwrap();

        assert_eq!(class, MoveClass::SameVolumeRename);
    }
}
