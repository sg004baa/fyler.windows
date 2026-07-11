//! delete/overwrite undo用backup payloadのコピー。

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use fyler_core::tree::EntryKind;
use fyler_core::undo::BackupRef;

/// 対象entryをbackup dir配下のpayloadへ退避コピーする。成功したら [`BackupRef`] を返す。
///
/// File/Symlink/Dir(再帰)対応。コピー中に失敗した場合は、作成途中のpayloadを
/// best-effortで削除してからErrを返す。承認済みforward applyの一部としてだけ呼ぶ。
pub fn backup_entry(
    source: &Path,
    backup_dir: &Path,
    step_index: usize,
) -> anyhow::Result<BackupRef> {
    let basename = source
        .file_name()
        .context("Backup source has no basename")?;
    let basename_text = basename
        .to_str()
        .context("Backup source basename is not UTF-8")?;
    let step_dir = backup_dir.join("payload").join(step_index.to_string());
    let payload = step_dir.join(basename);
    let payload_rel = format!("payload/{step_index}/{basename_text}");

    let metadata = fs::symlink_metadata(crate::long_path::to_fs(source)).with_context(|| {
        format!(
            "Failed to get metadata for backup source: {}",
            source.display()
        )
    })?;
    let kind = crate::scan::kind_from_metadata(&metadata);

    let copy_result = (|| {
        fs::create_dir_all(crate::long_path::to_fs(&step_dir)).with_context(|| {
            format!(
                "Failed to create backup payload directory: {}",
                step_dir.display()
            )
        })?;
        copy_entry(source, &payload, kind)
    })();

    match copy_result {
        Ok(()) => Ok(BackupRef { payload_rel, kind }),
        Err(error) => {
            discard_step_dir(&step_dir);
            Err(error)
        }
    }
}

/// [`BackupRef`] の payload を target へ復元コピーする。target の親は存在前提。
///
/// target が既に存在する場合はErrを返し、上書きしない。
pub fn restore_entry(backup_dir: &Path, backup: &BackupRef, target: &Path) -> anyhow::Result<()> {
    ensure_restore_target_vacant(target)?;
    let source = payload_path(backup_dir, backup)?;
    let restore_result = copy_entry(&source, target, backup.kind);
    match restore_result {
        Ok(()) => Ok(()),
        Err(error) => {
            discard_path(target);
            Err(error)
        }
    }
}

pub(crate) fn discard_backup_payload(backup_dir: &Path, backup: &BackupRef) {
    if let Ok(path) = payload_path(backup_dir, backup) {
        discard_path(&path);
        if let Some(step_dir) = path.parent() {
            discard_step_dir(step_dir);
        }
    }
}

fn copy_entry(source: &Path, target: &Path, kind: EntryKind) -> anyhow::Result<()> {
    match kind {
        EntryKind::Dir => crate::apply::copy_tree(source, target)
            .map(|_| ())
            .map_err(|failure| anyhow!(failure.error)),
        EntryKind::File | EntryKind::Symlink => {
            crate::apply::copy_single_entry(source, target, kind)
        }
    }
}

fn payload_path(backup_dir: &Path, backup: &BackupRef) -> anyhow::Result<PathBuf> {
    let mut path = backup_dir.to_path_buf();
    for component in backup.payload_rel.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            bail!(
                "Invalid backup payload relative path: {}",
                backup.payload_rel
            );
        }
        path.push(component);
    }
    Ok(path)
}

fn ensure_restore_target_vacant(target: &Path) -> anyhow::Result<()> {
    match fs::symlink_metadata(crate::long_path::to_fs(target)) {
        Ok(_) => bail!("Restore destination already exists: {}", target.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("Failed to check restore destination: {}", target.display())),
    }
}

fn discard_step_dir(step_dir: &Path) {
    let _ = fs::remove_dir_all(crate::long_path::to_fs(step_dir));
}

fn discard_path(path: &Path) {
    let Ok(metadata) = fs::symlink_metadata(crate::long_path::to_fs(path)) else {
        return;
    };
    let kind = crate::scan::kind_from_metadata(&metadata);
    match kind {
        EntryKind::Dir => {
            let _ = fs::remove_dir_all(crate::long_path::to_fs(path));
        }
        EntryKind::File | EntryKind::Symlink => {
            let _ = crate::apply::remove_non_directory_entry(path, kind);
        }
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn backs_up_and_restores_file_roundtrip() {
        let root = tempdir().unwrap();
        let backup = tempdir().unwrap();
        let source = root.path().join("source.txt");
        let restored = root.path().join("restored.txt");
        fs::write(&source, b"content").unwrap();

        let reference = backup_entry(&source, backup.path(), 0).unwrap();
        restore_entry(backup.path(), &reference, &restored).unwrap();

        assert_eq!(reference.payload_rel, "payload/0/source.txt");
        assert_eq!(reference.kind, EntryKind::File);
        assert_eq!(fs::read(&source).unwrap(), b"content");
        assert_eq!(fs::read(&restored).unwrap(), b"content");
    }

    #[test]
    fn backs_up_and_restores_directory_roundtrip() {
        let root = tempdir().unwrap();
        let backup = tempdir().unwrap();
        let source = root.path().join("dir");
        let restored = root.path().join("restored");
        fs::create_dir(&source).unwrap();
        fs::create_dir(source.join("nested")).unwrap();
        fs::write(source.join("nested/file.txt"), b"nested").unwrap();

        let reference = backup_entry(&source, backup.path(), 1).unwrap();
        restore_entry(backup.path(), &reference, &restored).unwrap();

        assert_eq!(reference.payload_rel, "payload/1/dir");
        assert_eq!(reference.kind, EntryKind::Dir);
        assert_eq!(
            fs::read(restored.join("nested/file.txt")).unwrap(),
            b"nested"
        );
    }

    #[cfg(unix)]
    #[test]
    fn backs_up_and_restores_symlink_roundtrip() {
        let root = tempdir().unwrap();
        let backup = tempdir().unwrap();
        fs::write(root.path().join("target.txt"), b"target").unwrap();
        let source = root.path().join("link.txt");
        let restored = root.path().join("restored-link.txt");
        std::os::unix::fs::symlink("target.txt", &source).unwrap();

        let reference = backup_entry(&source, backup.path(), 2).unwrap();
        restore_entry(backup.path(), &reference, &restored).unwrap();

        assert_eq!(reference.kind, EntryKind::Symlink);
        assert_eq!(
            fs::read_link(&restored).unwrap(),
            PathBuf::from("target.txt")
        );
    }

    #[test]
    fn restore_refuses_occupied_target() {
        let root = tempdir().unwrap();
        let backup = tempdir().unwrap();
        let source = root.path().join("source.txt");
        let occupied = root.path().join("occupied.txt");
        fs::write(&source, b"source").unwrap();
        fs::write(&occupied, b"occupied").unwrap();
        let reference = backup_entry(&source, backup.path(), 0).unwrap();

        let error = restore_entry(backup.path(), &reference, &occupied).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Restore destination already exists")
        );
        assert_eq!(fs::read(&occupied).unwrap(), b"occupied");
    }
}
