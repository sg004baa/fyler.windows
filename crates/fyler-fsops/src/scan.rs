//! baselineスキャン: 実FS → BaselineTree(ID採番)。

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use fyler_core::id::IdAllocator;
use fyler_core::path::TreePath;
use fyler_core::tree::{BaselineEntry, BaselineTree, EntryKind};

#[cfg(windows)]
use std::os::windows::fs::MetadataExt;
#[cfg(windows)]
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

/// ルート以下をスキャンしてBaselineTreeを構築する。
///
/// 実装契約:
/// - IDは `ids` から採番する(セッション内一意。永続化しない)。
///   reconcile時の再スキャンでは、**変化しなかったエントリのIDを維持する**必要が
///   あるため、既存baselineとの突き合わせ版(差分スキャン)もM3で必要になる
/// - symlink / junction / reparse point は**中に潜らず**、リンク自体を
///   `EntryKind::Symlink` の1エントリとして扱う(DESIGN.md「validateで弾くもの」)
/// - OneDriveプレースホルダ([`crate::onedrive`])のhydrationを発生させない
///   (メタデータ列挙のみ。内容・サイズの取得でリモートアクセスを誘発しない)
/// - collapsedなディレクトリの中もbaselineには**含める**(diffのDelete判定と
///   collapsed move追従に必要)。ただし深い階層の遅延スキャンにするかはM1で判断し、
///   遅延にする場合はEditContext/diffの契約と整合させること
pub fn scan_baseline(root: &Path, ids: &mut IdAllocator) -> anyhow::Result<BaselineTree> {
    scan_with_id_resolver(root, |_: &TreePath| ids.allocate())
}

/// 実FSを再スキャンし、同じパスに存在し続けるエントリのIDを維持する。
///
/// 前回baselineにないパスだけを [`IdAllocator`] から新規採番する。走査順、
/// symlink非潜行、エントリ種別の判定は [`scan_baseline`] と共通である。
pub fn rescan_preserving_ids(
    root: &Path,
    ids: &mut IdAllocator,
    previous: &BaselineTree,
) -> anyhow::Result<BaselineTree> {
    let previous_ids: HashMap<TreePath, _> = previous
        .entries
        .iter()
        .map(|entry| (entry.path.clone(), entry.id))
        .collect();
    scan_with_id_resolver(root, |path| {
        previous_ids
            .get(path)
            .copied()
            .unwrap_or_else(|| ids.allocate())
    })
}

fn scan_with_id_resolver(
    root: &Path,
    mut resolve_id: impl FnMut(&TreePath) -> fyler_core::id::EntryId,
) -> anyhow::Result<BaselineTree> {
    let root_metadata = fs::symlink_metadata(root)
        .with_context(|| format!("表示ルートのメタデータを取得できません: {}", root.display()))?;
    if is_link_or_reparse(&root_metadata) {
        bail!(
            "表示ルートにはsymlink/junction/reparse pointを指定できません: {}",
            root.display()
        );
    }
    if !root_metadata.is_dir() {
        bail!("表示ルートがディレクトリではありません: {}", root.display());
    }

    let mut tree = BaselineTree::new(root);
    scan_directory(root, &TreePath::root(), &mut resolve_id, &mut tree)?;
    Ok(tree)
}

fn scan_directory(
    directory: &Path,
    relative: &TreePath,
    resolve_id: &mut impl FnMut(&TreePath) -> fyler_core::id::EntryId,
    tree: &mut BaselineTree,
) -> anyhow::Result<()> {
    let mut stack = vec![ScanFrame {
        entries: read_sorted_entries(directory)?,
        index: 0,
        relative: relative.clone(),
    }];

    while let Some(frame) = stack.last_mut() {
        if frame.index >= frame.entries.len() {
            stack.pop();
            continue;
        }

        let (entry_path, file_name) = frame.entries[frame.index].clone();
        frame.index += 1;

        let name = file_name.to_str().with_context(|| {
            format!(
                "UTF-8として表現できないファイル名です: {}",
                entry_path.display()
            )
        })?;
        let path = frame.relative.child(name);
        let metadata = fs::symlink_metadata(&entry_path).with_context(|| {
            format!(
                "エントリのメタデータを取得できません: {}",
                entry_path.display()
            )
        })?;

        let kind = if is_link_or_reparse(&metadata) {
            EntryKind::Symlink
        } else if metadata.is_dir() {
            EntryKind::Dir
        } else {
            EntryKind::File
        };

        tree.insert(BaselineEntry {
            id: resolve_id(&path),
            path: path.clone(),
            kind,
        });

        if kind == EntryKind::Dir {
            stack.push(ScanFrame {
                entries: read_sorted_entries(&entry_path)?,
                index: 0,
                relative: path,
            });
        }
    }

    Ok(())
}

struct ScanFrame {
    entries: Vec<(PathBuf, OsString)>,
    index: usize,
    relative: TreePath,
}

fn read_sorted_entries(directory: &Path) -> anyhow::Result<Vec<(PathBuf, OsString)>> {
    let mut entries = fs::read_dir(directory)
        .with_context(|| format!("ディレクトリを列挙できません: {}", directory.display()))?
        .map(|entry| entry.map(|entry| (entry.path(), entry.file_name())))
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| {
            format!(
                "ディレクトリエントリを取得できません: {}",
                directory.display()
            )
        })?;

    // read_dirの順序は未規定なので、表示とID採番をセッションごとに安定させる。
    entries.sort_by_key(|(_, file_name)| file_name.clone());
    Ok(entries)
}

fn is_link_or_reparse(metadata: &Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }

    #[cfg(windows)]
    {
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
    }

    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn rescan_preserves_existing_ids_and_allocates_new_ones() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("kept.txt"), b"kept").unwrap();
        fs::write(root.path().join("removed.txt"), b"removed").unwrap();
        let mut ids = IdAllocator::new();
        let previous = scan_baseline(root.path(), &mut ids).unwrap();
        let kept_id = previous
            .entries
            .iter()
            .find(|entry| entry.path == TreePath::parse("kept.txt"))
            .unwrap()
            .id;

        fs::remove_file(root.path().join("removed.txt")).unwrap();
        fs::write(root.path().join("new.txt"), b"new").unwrap();
        let rescanned = rescan_preserving_ids(root.path(), &mut ids, &previous).unwrap();

        assert_eq!(
            rescanned
                .entries
                .iter()
                .find(|entry| entry.path == TreePath::parse("kept.txt"))
                .unwrap()
                .id,
            kept_id
        );
        let new_id = rescanned
            .entries
            .iter()
            .find(|entry| entry.path == TreePath::parse("new.txt"))
            .unwrap()
            .id;
        assert_ne!(new_id, kept_id);
        assert!(
            rescanned
                .entries
                .iter()
                .all(|entry| entry.path != TreePath::parse("removed.txt"))
        );
    }
}
