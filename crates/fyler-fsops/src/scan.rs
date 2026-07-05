//! baselineスキャン: 実FS → BaselineTree(ID採番)。

use std::cmp::Ordering;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use fyler_core::id::IdAllocator;
use fyler_core::options::SortOrder;
use fyler_core::path::TreePath;
use fyler_core::tree::{BaselineEntry, BaselineTree, EntryKind};

#[cfg(windows)]
use std::os::windows::fs::MetadataExt;
#[cfg(windows)]
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

/// baselineスキャン時の表示対象オプション。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScanOptions {
    /// `true`ならdotfileとWindowsのhidden属性を持つエントリもbaselineへ含める。
    pub show_hidden: bool,
    /// ディレクトリ優先または種別混在のソート順。
    pub sort: SortOrder,
}

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
    scan_baseline_with(root, ids, &ScanOptions::default())
}

/// 指定した表示対象オプションでルート以下をスキャンする。
///
/// 隠しエントリを除外する場合、そのディレクトリの中にも潜らず、baselineへ
/// 子孫を混入させない。
pub fn scan_baseline_with(
    root: &Path,
    ids: &mut IdAllocator,
    options: &ScanOptions,
) -> anyhow::Result<BaselineTree> {
    scan_with_id_resolver(root, options, |_: &TreePath| ids.allocate())
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
    rescan_preserving_ids_with(root, ids, previous, &ScanOptions::default())
}

/// 指定した表示対象オプションで再スキャンし、同じパスのIDを維持する。
pub fn rescan_preserving_ids_with(
    root: &Path,
    ids: &mut IdAllocator,
    previous: &BaselineTree,
    options: &ScanOptions,
) -> anyhow::Result<BaselineTree> {
    let previous_ids: HashMap<TreePath, _> = previous
        .entries()
        .iter()
        .map(|entry| (entry.path.clone(), entry.id))
        .collect();
    scan_with_id_resolver(root, options, |path| {
        previous_ids
            .get(path)
            .copied()
            .unwrap_or_else(|| ids.allocate())
    })
}

fn scan_with_id_resolver(
    root: &Path,
    options: &ScanOptions,
    mut resolve_id: impl FnMut(&TreePath) -> fyler_core::id::EntryId,
) -> anyhow::Result<BaselineTree> {
    let root_metadata = fs::symlink_metadata(crate::long_path::to_fs(root))
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
    scan_directory(root, &TreePath::root(), options, &mut resolve_id, &mut tree)?;
    Ok(tree)
}

fn scan_directory(
    directory: &Path,
    relative: &TreePath,
    options: &ScanOptions,
    resolve_id: &mut impl FnMut(&TreePath) -> fyler_core::id::EntryId,
    tree: &mut BaselineTree,
) -> anyhow::Result<()> {
    let mut stack = vec![ScanFrame {
        entries: read_sorted_entries(directory, options)?,
        index: 0,
        relative: relative.clone(),
    }];

    while let Some(frame) = stack.last_mut() {
        if frame.index >= frame.entries.len() {
            stack.pop();
            continue;
        }

        let entry = frame.entries[frame.index].clone();
        frame.index += 1;

        let name = entry.file_name.to_str().with_context(|| {
            format!(
                "UTF-8として表現できないファイル名です: {}",
                entry.path.display()
            )
        })?;
        let path = frame.relative.child(name);
        let kind = kind_from_metadata(&entry.metadata);

        tree.insert(BaselineEntry {
            id: resolve_id(&path),
            path: path.clone(),
            kind,
        });

        if kind == EntryKind::Dir {
            stack.push(ScanFrame {
                entries: read_sorted_entries(&entry.path, options)?,
                index: 0,
                relative: path,
            });
        }
    }

    Ok(())
}

struct ScanFrame {
    entries: Vec<ScannedEntry>,
    index: usize,
    relative: TreePath,
}

#[derive(Clone)]
struct ScannedEntry {
    path: PathBuf,
    file_name: OsString,
    metadata: Metadata,
}

fn read_sorted_entries(
    directory: &Path,
    options: &ScanOptions,
) -> anyhow::Result<Vec<ScannedEntry>> {
    let read_dir = fs::read_dir(crate::long_path::to_fs(directory))
        .with_context(|| format!("ディレクトリを列挙できません: {}", directory.display()))?;
    let mut entries = Vec::new();
    for entry in read_dir {
        let entry = entry.with_context(|| {
            format!(
                "ディレクトリエントリを取得できません: {}",
                directory.display()
            )
        })?;
        let path = entry.path();
        let file_name = entry.file_name();
        if !options.show_hidden && is_hidden(&path, &file_name)? {
            continue;
        }
        let metadata = fs::symlink_metadata(crate::long_path::to_fs(&path))
            .with_context(|| format!("エントリのメタデータを取得できません: {}", path.display()))?;
        entries.push(ScannedEntry {
            path,
            file_name,
            metadata,
        });
    }

    // read_dirの順序は未規定なので、設定された自然順で表示とID採番を
    // セッションごとに安定させる。同値時は元のOsStringで順序を確定する。
    entries.sort_by(|left, right| {
        let kind_order = match options.sort {
            SortOrder::DirsFirst => {
                let left_is_dir = kind_from_metadata(&left.metadata) == EntryKind::Dir;
                let right_is_dir = kind_from_metadata(&right.metadata) == EntryKind::Dir;
                right_is_dir.cmp(&left_is_dir)
            }
            SortOrder::Mixed => Ordering::Equal,
        };
        kind_order
            .then_with(|| natural_cmp_case_insensitive(&left.file_name, &right.file_name))
            .then_with(|| left.file_name.cmp(&right.file_name))
    });
    Ok(entries)
}

fn is_hidden(path: &Path, file_name: &OsStr) -> anyhow::Result<bool> {
    if file_name.to_string_lossy().starts_with('.') {
        return Ok(true);
    }

    #[cfg(windows)]
    {
        const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
        Ok(crate::winattr::get(path)? & FILE_ATTRIBUTE_HIDDEN != 0)
    }

    #[cfg(not(windows))]
    {
        let _ = path;
        Ok(false)
    }
}

fn natural_cmp_case_insensitive(left: &OsStr, right: &OsStr) -> Ordering {
    let left = left.to_string_lossy().to_lowercase();
    let right = right.to_string_lossy().to_lowercase();
    natural_cmp_bytes(left.as_bytes(), right.as_bytes())
}

fn natural_cmp_bytes(mut left: &[u8], mut right: &[u8]) -> Ordering {
    while !left.is_empty() && !right.is_empty() {
        let left_is_digit = left[0].is_ascii_digit();
        let right_is_digit = right[0].is_ascii_digit();
        if left_is_digit && right_is_digit {
            let left_end = left
                .iter()
                .position(|byte| !byte.is_ascii_digit())
                .unwrap_or(left.len());
            let right_end = right
                .iter()
                .position(|byte| !byte.is_ascii_digit())
                .unwrap_or(right.len());
            let left_digits = &left[..left_end];
            let right_digits = &right[..right_end];
            let left_significant =
                &left_digits[left_digits.iter().take_while(|byte| **byte == b'0').count()..];
            let right_significant = &right_digits[right_digits
                .iter()
                .take_while(|byte| **byte == b'0')
                .count()..];
            let ordering = left_significant
                .len()
                .cmp(&right_significant.len())
                .then_with(|| left_significant.cmp(right_significant));
            if ordering != Ordering::Equal {
                return ordering;
            }
            left = &left[left_end..];
            right = &right[right_end..];
            continue;
        }

        let left_end = left
            .iter()
            .position(|byte| byte.is_ascii_digit())
            .unwrap_or(left.len());
        let right_end = right
            .iter()
            .position(|byte| byte.is_ascii_digit())
            .unwrap_or(right.len());
        let left_end = left_end.max(1);
        let right_end = right_end.max(1);
        let ordering = left[..left_end].cmp(&right[..right_end]);
        if ordering != Ordering::Equal {
            return ordering;
        }
        left = &left[left_end..];
        right = &right[right_end..];
    }

    left.len().cmp(&right.len())
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

pub(crate) fn kind_from_metadata(metadata: &Metadata) -> EntryKind {
    if is_link_or_reparse(metadata) {
        EntryKind::Symlink
    } else if metadata.is_dir() {
        EntryKind::Dir
    } else {
        EntryKind::File
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
            .entries()
            .iter()
            .find(|entry| entry.path == TreePath::parse("kept.txt"))
            .unwrap()
            .id;

        fs::remove_file(root.path().join("removed.txt")).unwrap();
        fs::write(root.path().join("new.txt"), b"new").unwrap();
        let rescanned = rescan_preserving_ids(root.path(), &mut ids, &previous).unwrap();

        assert_eq!(
            rescanned
                .entries()
                .iter()
                .find(|entry| entry.path == TreePath::parse("kept.txt"))
                .unwrap()
                .id,
            kept_id
        );
        let new_id = rescanned
            .entries()
            .iter()
            .find(|entry| entry.path == TreePath::parse("new.txt"))
            .unwrap()
            .id;
        assert_ne!(new_id, kept_id);
        assert!(
            rescanned
                .entries()
                .iter()
                .all(|entry| entry.path != TreePath::parse("removed.txt"))
        );
    }

    #[test]
    fn hidden_dot_entries_follow_scan_options() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("visible.txt"), b"visible").unwrap();
        fs::write(root.path().join(".hidden.txt"), b"hidden").unwrap();
        fs::create_dir(root.path().join(".hidden-dir")).unwrap();
        fs::write(root.path().join(".hidden-dir").join("child.txt"), b"child").unwrap();

        let mut hidden_ids = IdAllocator::new();
        let hidden = scan_baseline(root.path(), &mut hidden_ids).unwrap();
        assert_eq!(
            hidden
                .entries()
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>(),
            [TreePath::parse("visible.txt")]
        );

        let mut shown_ids = IdAllocator::new();
        let shown = scan_baseline_with(
            root.path(),
            &mut shown_ids,
            &ScanOptions {
                show_hidden: true,
                ..ScanOptions::default()
            },
        )
        .unwrap();
        assert!(
            shown
                .entries()
                .iter()
                .any(|entry| entry.path == TreePath::parse(".hidden.txt"))
        );
        assert!(
            shown
                .entries()
                .iter()
                .any(|entry| entry.path == TreePath::parse(".hidden-dir/child.txt"))
        );
    }

    #[test]
    fn scan_sorts_directories_first_then_names_in_natural_order() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("10.txt"), b"10").unwrap();
        fs::write(root.path().join("2.txt"), b"2").unwrap();
        fs::write(root.path().join("1.txt"), b"1").unwrap();
        fs::create_dir(root.path().join("20-dir")).unwrap();
        fs::create_dir(root.path().join("3-dir")).unwrap();

        let mut ids = IdAllocator::new();
        let baseline = scan_baseline(root.path(), &mut ids).unwrap();
        let paths = baseline
            .entries()
            .iter()
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>();

        assert_eq!(
            paths,
            [
                TreePath::parse("3-dir"),
                TreePath::parse("20-dir"),
                TreePath::parse("1.txt"),
                TreePath::parse("2.txt"),
                TreePath::parse("10.txt"),
            ]
        );
    }

    #[test]
    fn mixed_sort_interleaves_directories_and_files_in_natural_order() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("10.txt"), b"10").unwrap();
        fs::create_dir(root.path().join("2-dir")).unwrap();
        fs::write(root.path().join("1.txt"), b"1").unwrap();
        fs::create_dir(root.path().join("20-dir")).unwrap();

        let mut ids = IdAllocator::new();
        let baseline = scan_baseline_with(
            root.path(),
            &mut ids,
            &ScanOptions {
                sort: SortOrder::Mixed,
                ..ScanOptions::default()
            },
        )
        .unwrap();
        let paths = baseline
            .entries()
            .iter()
            .map(|entry| entry.path.clone())
            .collect::<Vec<_>>();

        assert_eq!(
            paths,
            [
                TreePath::parse("1.txt"),
                TreePath::parse("2-dir"),
                TreePath::parse("10.txt"),
                TreePath::parse("20-dir"),
            ]
        );
    }

    #[test]
    fn natural_sort_is_case_insensitive_and_numeric_aware() {
        assert_eq!(
            natural_cmp_case_insensitive(OsStr::new("FILE2.txt"), OsStr::new("file10.TXT")),
            Ordering::Less
        );
        assert_eq!(
            natural_cmp_case_insensitive(OsStr::new("b.txt"), OsStr::new("A.txt")),
            Ordering::Greater
        );
    }
}
