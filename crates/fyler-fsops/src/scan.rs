//! baselineスキャン: 実FS → BaselineTree(ID採番)。

use std::fs::{self, Metadata};
use std::path::Path;

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
    scan_directory(root, &TreePath::root(), ids, &mut tree)?;
    Ok(tree)
}

fn scan_directory(
    directory: &Path,
    relative: &TreePath,
    ids: &mut IdAllocator,
    tree: &mut BaselineTree,
) -> anyhow::Result<()> {
    let mut entries = fs::read_dir(directory)
        .with_context(|| format!("ディレクトリを列挙できません: {}", directory.display()))?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| {
            format!(
                "ディレクトリエントリを取得できません: {}",
                directory.display()
            )
        })?;

    // read_dirの順序は未規定なので、表示とID採番をセッションごとに安定させる。
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let file_name = entry.file_name();
        let name = file_name.to_str().with_context(|| {
            format!(
                "UTF-8として表現できないファイル名です: {}",
                entry.path().display()
            )
        })?;
        let path = relative.child(name);
        let metadata = fs::symlink_metadata(entry.path()).with_context(|| {
            format!(
                "エントリのメタデータを取得できません: {}",
                entry.path().display()
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
            id: ids.allocate(),
            path: path.clone(),
            kind,
        });

        if kind == EntryKind::Dir {
            scan_directory(&entry.path(), &path, ids, tree)?;
        }
    }

    Ok(())
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
