//! undo receipt用の実体識別子と属性fingerprint採取。

use std::fs;
use std::path::Path;

use anyhow::Context;
use fyler_core::tree::EntryKind;
use fyler_core::undo::{FileIdentity, Fingerprint, ManifestEntry};

/// 実体識別子を採取する。symlinkは辿らず、link自身のidentityを返す。
pub fn capture_identity(path: &Path) -> anyhow::Result<FileIdentity> {
    capture_identity_impl(path)
        .with_context(|| format!("実体識別子を採取できません: {}", path.display()))
}

#[cfg(windows)]
fn capture_identity_impl(path: &Path) -> anyhow::Result<FileIdentity> {
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;

    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdInfo,
        GetFileInformationByHandleEx, OPEN_EXISTING,
    };
    use windows::core::PCWSTR;

    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    let fs_path = crate::long_path::to_fs(path);
    let wide_path: Vec<u16> = fs_path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe {
        CreateFileW(
            PCWSTR::from_raw(wide_path.as_ptr()),
            FILE_READ_ATTRIBUTES.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }?;
    let handle = OwnedHandle(handle);

    let mut info = FILE_ID_INFO::default();
    unsafe {
        GetFileInformationByHandleEx(
            handle.0,
            FileIdInfo,
            (&raw mut info).cast::<c_void>(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    }?;

    Ok(FileIdentity {
        volume: info.VolumeSerialNumber,
        file: u128::from_le_bytes(info.FileId.Identifier),
    })
}

#[cfg(unix)]
fn capture_identity_impl(path: &Path) -> anyhow::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::symlink_metadata(crate::long_path::to_fs(path))?;
    Ok(FileIdentity {
        volume: metadata.dev(),
        file: u128::from(metadata.ino()),
    })
}

#[cfg(not(any(unix, windows)))]
fn capture_identity_impl(path: &Path) -> anyhow::Result<FileIdentity> {
    let _ = path;
    anyhow::bail!("このプラットフォームでは実体識別子の採取に未対応です")
}

/// 属性のみのfingerprintを採取する。内容は読まない(placeholder hydration防止)。
///
/// Fileはsize+mtime、Dirは両方None、Symlinkは`read_link`したlink targetを記録する。
pub fn capture_fingerprint(path: &Path) -> anyhow::Result<Fingerprint> {
    let fs_path = crate::long_path::to_fs(path);
    let metadata = fs::symlink_metadata(&fs_path)
        .with_context(|| format!("fingerprintのmetadataを取得できません: {}", path.display()))?;
    let kind = crate::scan::kind_from_metadata(&metadata);
    match kind {
        EntryKind::File => Ok(Fingerprint {
            kind,
            size: Some(metadata.len()),
            mtime: metadata.modified().ok(),
            link_target: None,
        }),
        EntryKind::Dir => Ok(Fingerprint {
            kind,
            size: None,
            mtime: None,
            link_target: None,
        }),
        EntryKind::Symlink => Ok(Fingerprint {
            kind,
            size: None,
            mtime: None,
            link_target: Some(fs::read_link(&fs_path).with_context(|| {
                format!("symlinkのリンク先を読み取れません: {}", path.display())
            })?),
        }),
    }
}

/// ディレクトリ子孫のmanifestを採取する。symlinkへは潜らない。
///
/// `rel_path`は`/`区切り・ソート済みの決定的順序で返す。非UTF-8名はErr。
pub fn capture_manifest(dir: &Path) -> anyhow::Result<Vec<ManifestEntry>> {
    let root = dir.to_path_buf();
    let mut entries = Vec::new();
    collect_manifest_entries(&root, &mut Vec::new(), &mut entries)?;
    entries.sort_by(|left, right| left.rel_path.cmp(&right.rel_path));
    Ok(entries)
}

fn collect_manifest_entries(
    directory: &Path,
    relative: &mut Vec<String>,
    entries: &mut Vec<ManifestEntry>,
) -> anyhow::Result<()> {
    let read_dir = fs::read_dir(crate::long_path::to_fs(directory)).with_context(|| {
        format!(
            "manifest対象ディレクトリを列挙できません: {}",
            directory.display()
        )
    })?;
    for entry in read_dir {
        let entry = entry.with_context(|| {
            format!(
                "manifest対象ディレクトリのエントリを取得できません: {}",
                directory.display()
            )
        })?;
        let name = entry.file_name();
        let name = name.to_str().with_context(|| {
            format!(
                "manifest対象にUTF-8として表現できない名前があります: {}",
                entry.path().display()
            )
        })?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(crate::long_path::to_fs(&path)).with_context(|| {
            format!("manifest対象のmetadataを取得できません: {}", path.display())
        })?;
        let kind = crate::scan::kind_from_metadata(&metadata);

        relative.push(name.to_owned());
        entries.push(ManifestEntry {
            rel_path: relative.join("/"),
            kind,
            size: (kind != EntryKind::Dir).then_some(metadata.len()),
            mtime: (kind != EntryKind::Dir)
                .then(|| metadata.modified().ok())
                .flatten(),
        });

        if kind == EntryKind::Dir {
            collect_manifest_entries(&path, relative, entries)?;
        }
        relative.pop();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn identity_is_stable_for_same_file_and_rename() {
        let root = tempdir().unwrap();
        let first = root.path().join("first.txt");
        let renamed = root.path().join("renamed.txt");
        let other = root.path().join("other.txt");
        fs::write(&first, b"first").unwrap();
        fs::write(&other, b"other").unwrap();

        let before = capture_identity(&first).unwrap();
        assert_eq!(capture_identity(&first).unwrap(), before);
        assert_ne!(capture_identity(&other).unwrap(), before);

        fs::rename(&first, &renamed).unwrap();
        assert_eq!(capture_identity(&renamed).unwrap(), before);
    }

    #[test]
    fn fingerprint_records_file_and_directory_attributes_without_content() {
        let root = tempdir().unwrap();
        let file = root.path().join("file.txt");
        let dir = root.path().join("dir");
        fs::write(&file, b"hello").unwrap();
        fs::create_dir(&dir).unwrap();

        let file_fp = capture_fingerprint(&file).unwrap();
        assert_eq!(file_fp.kind, EntryKind::File);
        assert_eq!(file_fp.size, Some(5));
        assert!(matches!(file_fp.mtime, Some(time) if time <= SystemTime::now()));
        assert_eq!(file_fp.link_target, None);

        let dir_fp = capture_fingerprint(&dir).unwrap();
        assert_eq!(dir_fp.kind, EntryKind::Dir);
        assert_eq!(dir_fp.size, None);
        assert_eq!(dir_fp.mtime, None);
        assert_eq!(dir_fp.link_target, None);
    }

    #[cfg(unix)]
    #[test]
    fn fingerprint_records_symlink_target_without_descending() {
        use std::path::PathBuf;

        let root = tempdir().unwrap();
        fs::write(root.path().join("target.txt"), b"target").unwrap();
        let link = root.path().join("link.txt");
        std::os::unix::fs::symlink("target.txt", &link).unwrap();

        let fingerprint = capture_fingerprint(&link).unwrap();

        assert_eq!(fingerprint.kind, EntryKind::Symlink);
        assert_eq!(fingerprint.size, None);
        assert_eq!(fingerprint.mtime, None);
        assert_eq!(fingerprint.link_target, Some(PathBuf::from("target.txt")));
    }

    #[test]
    fn manifest_records_nested_entries_in_deterministic_order() {
        let root = tempdir().unwrap();
        let dir = root.path().join("dir");
        fs::create_dir(&dir).unwrap();
        fs::create_dir(dir.join("nested")).unwrap();
        fs::write(dir.join("z.txt"), b"zz").unwrap();
        fs::write(dir.join("nested/a.txt"), b"a").unwrap();

        let manifest = capture_manifest(&dir).unwrap();
        let paths = manifest
            .iter()
            .map(|entry| (entry.rel_path.as_str(), entry.kind, entry.size))
            .collect::<Vec<_>>();

        assert_eq!(
            paths,
            [
                ("nested", EntryKind::Dir, None),
                ("nested/a.txt", EntryKind::File, Some(1)),
                ("z.txt", EntryKind::File, Some(2)),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn manifest_records_symlink_itself_without_descending() {
        let root = tempdir().unwrap();
        let dir = root.path().join("dir");
        let outside = root.path().join("outside");
        fs::create_dir(&dir).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("child.txt"), b"child").unwrap();
        std::os::unix::fs::symlink("../outside", dir.join("link")).unwrap();

        let manifest = capture_manifest(&dir).unwrap();

        assert_eq!(manifest.len(), 1);
        assert_eq!(manifest[0].rel_path, "link");
        assert_eq!(manifest[0].kind, EntryKind::Symlink);
    }
}
