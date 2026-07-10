//! baselineスキャン: 実FS → BaselineTree(ID採番)。

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use fyler_core::fileinfo::EntryMeta;
use fyler_core::id::IdAllocator;
use fyler_core::options::{SortKey, SortOrder};
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
    /// 種別グループ内で使うソートキー。
    pub key: SortKey,
    /// `true`ならソートキー部分だけを降順にする。
    pub reverse: bool,
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

/// watcherが報告した変更パスから影響ディレクトリだけを再スキャンする。
///
/// 影響外のエントリ・ID・メタデータは`previous`から引き継ぐ。新規パスは必ず
/// 実FSを列挙する領域にだけ現れ、部分再構築でも全再スキャンと同じDFS順で到達する。
/// したがって新規IDの採番順も[`rescan_preserving_ids_with`]と一致する。
///
/// 変更パスをルート相対UTF-8パスへ変換できない場合や、部分再構築中の列挙が
/// ファイルシステムとの競合で失敗した場合は、安全のため全再スキャンへ戻る。
pub fn rescan_changed_preserving_ids_with(
    root: &Path,
    ids: &mut IdAllocator,
    previous: &BaselineTree,
    changed_paths: &BTreeSet<PathBuf>,
    options: &ScanOptions,
) -> anyhow::Result<BaselineTree> {
    if changed_paths.is_empty() {
        return rescan_preserving_ids_with(root, ids, previous, options);
    }

    let Some(changed_paths) = changed_paths
        .iter()
        .map(|path| to_relative_tree_path(root, path))
        .collect::<Option<Vec<_>>>()
    else {
        return rescan_preserving_ids_with(root, ids, previous, options);
    };

    match rebuild_changed(root, ids, previous, &changed_paths, options) {
        Ok(tree) => Ok(tree),
        Err(_) => rescan_preserving_ids_with(root, ids, previous, options),
    }
}

fn to_relative_tree_path(root: &Path, path: &Path) -> Option<TreePath> {
    let relative = path.strip_prefix(root).ok()?;
    relative
        .components()
        .map(|component| component.as_os_str().to_str().map(ToOwned::to_owned))
        .collect::<Option<Vec<_>>>()
        .map(TreePath::from_components)
}

fn rebuild_changed(
    root: &Path,
    ids: &mut IdAllocator,
    previous: &BaselineTree,
    changed_paths: &[TreePath],
    options: &ScanOptions,
) -> anyhow::Result<BaselineTree> {
    validate_root(root)?;

    let previous_by_path = previous
        .entries()
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.path.clone(), index))
        .collect::<HashMap<_, _>>();
    let mut children_of: HashMap<TreePath, Vec<usize>> = HashMap::new();
    for (index, entry) in previous.entries().iter().enumerate() {
        children_of
            .entry(entry.path.parent().unwrap_or_else(TreePath::root))
            .or_default()
            .push(index);
    }

    let mut affected = HashSet::new();
    for path in changed_paths {
        let mut ancestor = path.parent().unwrap_or_else(TreePath::root);
        loop {
            let is_existing_dir = previous_by_path
                .get(&ancestor)
                .is_some_and(|index| previous.entries()[*index].kind == EntryKind::Dir);
            if ancestor.is_root() || is_existing_dir {
                affected.insert(ancestor);
                break;
            }
            ancestor = ancestor.parent().unwrap_or_else(TreePath::root);
        }

        if previous_by_path
            .get(path)
            .is_some_and(|index| previous.entries()[*index].kind == EntryKind::Dir)
        {
            affected.insert(path.clone());
        }
    }

    let mut tree = BaselineTree::new(root);
    rebuild_directory(
        root,
        &TreePath::root(),
        ids,
        previous,
        &previous_by_path,
        &children_of,
        &affected,
        options,
        &mut tree,
    )?;
    Ok(tree)
}

#[allow(clippy::too_many_arguments)]
fn rebuild_directory(
    root: &Path,
    relative: &TreePath,
    ids: &mut IdAllocator,
    previous: &BaselineTree,
    previous_by_path: &HashMap<TreePath, usize>,
    children_of: &HashMap<TreePath, Vec<usize>>,
    affected: &HashSet<TreePath>,
    options: &ScanOptions,
    tree: &mut BaselineTree,
) -> anyhow::Result<()> {
    let was_directory = relative.is_root()
        || previous_by_path
            .get(relative)
            .is_some_and(|index| previous.entries()[*index].kind == EntryKind::Dir);
    let should_scan = affected.contains(relative) || !was_directory;

    if should_scan {
        let directory = relative.to_fs_path(root);
        for entry in read_sorted_entries(&directory, options)? {
            let name = entry.file_name.to_str().with_context(|| {
                format!(
                    "UTF-8として表現できないファイル名です: {}",
                    entry.path.display()
                )
            })?;
            let path = relative.child(name);
            let id = previous_by_path
                .get(&path)
                .map(|index| previous.entries()[*index].id)
                .unwrap_or_else(|| ids.allocate());
            let kind = entry.kind;
            let child_directory = entry.path;
            tree.insert_with_meta(
                BaselineEntry {
                    id,
                    path: path.clone(),
                    kind,
                },
                entry.meta,
            );

            if kind == EntryKind::Dir {
                rebuild_directory(
                    root,
                    &path,
                    ids,
                    previous,
                    previous_by_path,
                    children_of,
                    affected,
                    options,
                    tree,
                )
                .with_context(|| {
                    format!(
                        "変更ディレクトリの再構築に失敗しました: {}",
                        child_directory.display()
                    )
                })?;
            }
        }
    } else if let Some(children) = children_of.get(relative) {
        for index in children {
            let entry = previous.entries()[*index].clone();
            let id = entry.id;
            let kind = entry.kind;
            let path = entry.path.clone();
            if let Some(meta) = previous.meta(id).copied() {
                tree.insert_with_meta(entry, meta);
            } else {
                tree.insert(entry);
            }

            if kind == EntryKind::Dir {
                rebuild_directory(
                    root,
                    &path,
                    ids,
                    previous,
                    previous_by_path,
                    children_of,
                    affected,
                    options,
                    tree,
                )?;
            }
        }
    }

    Ok(())
}

fn scan_with_id_resolver(
    root: &Path,
    options: &ScanOptions,
    mut resolve_id: impl FnMut(&TreePath) -> fyler_core::id::EntryId,
) -> anyhow::Result<BaselineTree> {
    validate_root(root)?;

    let mut tree = BaselineTree::new(root);
    scan_directory(root, &TreePath::root(), options, &mut resolve_id, &mut tree)?;
    Ok(tree)
}

fn validate_root(root: &Path) -> anyhow::Result<()> {
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
    Ok(())
}

fn scan_directory(
    directory: &Path,
    relative: &TreePath,
    options: &ScanOptions,
    resolve_id: &mut impl FnMut(&TreePath) -> fyler_core::id::EntryId,
    tree: &mut BaselineTree,
) -> anyhow::Result<()> {
    let mut stack = vec![ScanFrame {
        entries: read_sorted_entries(directory, options)?.into_iter(),
        relative: relative.clone(),
    }];

    while let Some(frame) = stack.last_mut() {
        let Some(entry) = frame.entries.next() else {
            stack.pop();
            continue;
        };

        let name = entry.file_name.to_str().with_context(|| {
            format!(
                "UTF-8として表現できないファイル名です: {}",
                entry.path.display()
            )
        })?;
        let path = frame.relative.child(name);
        let kind = entry.kind;

        tree.insert_with_meta(
            BaselineEntry {
                id: resolve_id(&path),
                path: path.clone(),
                kind,
            },
            entry.meta,
        );

        if kind == EntryKind::Dir {
            stack.push(ScanFrame {
                entries: read_sorted_entries(&entry.path, options)?.into_iter(),
                relative: path,
            });
        }
    }

    Ok(())
}

struct ScanFrame {
    entries: std::vec::IntoIter<ScannedEntry>,
    relative: TreePath,
}

#[derive(Debug, Clone)]
struct ScannedEntry {
    path: PathBuf,
    file_name: OsString,
    sort_key: String,
    extension_key: String,
    kind: EntryKind,
    meta: EntryMeta,
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
        let metadata = entry
            .metadata()
            .with_context(|| format!("エントリのメタデータを取得できません: {}", path.display()))?;
        if !options.show_hidden && is_hidden(&file_name, &metadata) {
            continue;
        }
        let name = file_name.to_str().with_context(|| {
            format!("UTF-8として表現できないファイル名です: {}", path.display())
        })?;
        let sort_key = name.to_lowercase();
        let extension_key = extension_sort_key(&sort_key).to_owned();
        let kind = kind_from_metadata(&metadata);
        let meta = meta_from_metadata(&metadata);
        entries.push(ScannedEntry {
            path,
            file_name,
            sort_key,
            extension_key,
            kind,
            meta,
        });
    }

    // read_dirの順序は未規定なので、設定された自然順で表示とID採番を
    // セッションごとに安定させる。同値時は元のOsStringで順序を確定する。
    entries.sort_by(|left, right| compare_scanned(left, right, options));
    Ok(entries)
}

fn compare_scanned(left: &ScannedEntry, right: &ScannedEntry, options: &ScanOptions) -> Ordering {
    kind_order(left, right, options)
        .then_with(|| compare_sort_key(left, right, options))
        .then_with(|| natural_cmp_bytes(left.sort_key.as_bytes(), right.sort_key.as_bytes()))
        .then_with(|| left.file_name.cmp(&right.file_name))
}

fn kind_order(left: &ScannedEntry, right: &ScannedEntry, options: &ScanOptions) -> Ordering {
    match options.sort {
        SortOrder::DirsFirst => {
            let left_is_dir = left.kind == EntryKind::Dir;
            let right_is_dir = right.kind == EntryKind::Dir;
            right_is_dir.cmp(&left_is_dir)
        }
        SortOrder::Mixed => Ordering::Equal,
    }
}

fn compare_sort_key(left: &ScannedEntry, right: &ScannedEntry, options: &ScanOptions) -> Ordering {
    let ordering = match options.key {
        SortKey::Name => natural_cmp_bytes(left.sort_key.as_bytes(), right.sort_key.as_bytes()),
        SortKey::Date => {
            return compare_optional_last(left.meta.modified, right.meta.modified, options.reverse);
        }
        SortKey::Size => {
            return compare_optional_last(left.meta.size, right.meta.size, options.reverse);
        }
        SortKey::Extension => left.extension_key.cmp(&right.extension_key),
    };

    if options.reverse {
        ordering.reverse()
    } else {
        ordering
    }
}

fn compare_optional_last<T: Ord>(left: Option<T>, right: Option<T>, reverse: bool) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => {
            let ordering = left.cmp(&right);
            if reverse {
                ordering.reverse()
            } else {
                ordering
            }
        }
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

fn extension_sort_key(lowercase_name: &str) -> &str {
    match lowercase_name.rfind('.') {
        Some(index) if index > 0 => &lowercase_name[index + 1..],
        _ => "",
    }
}

fn is_hidden(file_name: &OsStr, metadata: &Metadata) -> bool {
    if file_name.as_encoded_bytes().first() == Some(&b'.') {
        return true;
    }

    #[cfg(windows)]
    {
        const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
        metadata.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0
    }

    #[cfg(not(windows))]
    {
        let _ = metadata;
        false
    }
}

#[cfg(test)]
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

fn meta_from_metadata(metadata: &Metadata) -> EntryMeta {
    EntryMeta {
        size: (!metadata.is_dir()).then_some(metadata.len()),
        modified: metadata.modified().ok(),
        is_placeholder: is_placeholder(metadata),
    }
}

fn is_placeholder(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        let attributes = metadata.file_attributes();
        let placeholder_attributes = crate::onedrive::FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
            | crate::onedrive::FILE_ATTRIBUTE_RECALL_ON_OPEN
            | crate::onedrive::FILE_ATTRIBUTE_OFFLINE;
        attributes & placeholder_attributes != 0
    }

    #[cfg(not(windows))]
    {
        let _ = metadata;
        false
    }
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
    use std::collections::BTreeSet;
    use std::fs;
    use std::time::{Duration, SystemTime};

    use tempfile::tempdir;

    use super::*;

    fn allocator_after(previous: &BaselineTree) -> IdAllocator {
        let next = previous
            .entries()
            .iter()
            .map(|entry| entry.id.0)
            .max()
            .unwrap_or(0)
            + 1;
        let mut ids = IdAllocator::new();
        for _ in 1..next {
            ids.allocate();
        }
        ids
    }

    fn scanned_entry(
        name: &str,
        kind: EntryKind,
        size: Option<u64>,
        modified_seconds: Option<u64>,
    ) -> ScannedEntry {
        let sort_key = name.to_lowercase();
        let extension_key = extension_sort_key(&sort_key).to_owned();
        ScannedEntry {
            path: PathBuf::from(name),
            file_name: OsString::from(name),
            sort_key,
            extension_key,
            kind,
            meta: EntryMeta {
                size,
                modified: modified_seconds
                    .map(|seconds| SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)),
                is_placeholder: false,
            },
        }
    }

    fn sorted_names(mut entries: Vec<ScannedEntry>, options: ScanOptions) -> Vec<String> {
        entries.sort_by(|left, right| compare_scanned(left, right, &options));
        entries
            .into_iter()
            .map(|entry| entry.file_name.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn compare_scanned_keeps_dirs_first_group_before_key_and_reverse() {
        let entries = vec![
            scanned_entry("z.txt", EntryKind::File, Some(10), Some(10)),
            scanned_entry("a", EntryKind::Dir, None, Some(1)),
            scanned_entry("b.txt", EntryKind::File, Some(1), Some(1)),
        ];

        assert_eq!(
            sorted_names(
                entries,
                ScanOptions {
                    key: SortKey::Size,
                    reverse: true,
                    ..ScanOptions::default()
                },
            ),
            ["a", "z.txt", "b.txt"]
        );
    }

    #[test]
    fn compare_scanned_reverses_name_key_only_and_keeps_tiebreak_stable() {
        let entries = vec![
            scanned_entry("file2.txt", EntryKind::File, Some(1), Some(1)),
            scanned_entry("file10.txt", EntryKind::File, Some(1), Some(1)),
            scanned_entry("File2.txt", EntryKind::File, Some(1), Some(1)),
        ];

        assert_eq!(
            sorted_names(
                entries,
                ScanOptions {
                    sort: SortOrder::Mixed,
                    key: SortKey::Name,
                    reverse: true,
                    ..ScanOptions::default()
                },
            ),
            ["file10.txt", "File2.txt", "file2.txt"]
        );
    }

    #[test]
    fn compare_scanned_sorts_date_and_keeps_none_last_even_when_reversed() {
        let entries = vec![
            scanned_entry("none.txt", EntryKind::File, Some(1), None),
            scanned_entry("old.txt", EntryKind::File, Some(1), Some(10)),
            scanned_entry("new.txt", EntryKind::File, Some(1), Some(20)),
        ];

        assert_eq!(
            sorted_names(
                entries.clone(),
                ScanOptions {
                    sort: SortOrder::Mixed,
                    key: SortKey::Date,
                    ..ScanOptions::default()
                },
            ),
            ["old.txt", "new.txt", "none.txt"]
        );
        assert_eq!(
            sorted_names(
                entries,
                ScanOptions {
                    sort: SortOrder::Mixed,
                    key: SortKey::Date,
                    reverse: true,
                    ..ScanOptions::default()
                },
            ),
            ["new.txt", "old.txt", "none.txt"]
        );
    }

    #[test]
    fn compare_scanned_sorts_size_and_keeps_none_last_even_when_reversed() {
        let entries = vec![
            scanned_entry("none", EntryKind::Dir, None, Some(1)),
            scanned_entry("small.txt", EntryKind::File, Some(1), Some(1)),
            scanned_entry("large.txt", EntryKind::File, Some(10), Some(1)),
        ];

        assert_eq!(
            sorted_names(
                entries.clone(),
                ScanOptions {
                    sort: SortOrder::Mixed,
                    key: SortKey::Size,
                    ..ScanOptions::default()
                },
            ),
            ["small.txt", "large.txt", "none"]
        );
        assert_eq!(
            sorted_names(
                entries,
                ScanOptions {
                    sort: SortOrder::Mixed,
                    key: SortKey::Size,
                    reverse: true,
                    ..ScanOptions::default()
                },
            ),
            ["large.txt", "small.txt", "none"]
        );
    }

    #[test]
    fn compare_scanned_sorts_extension_by_precomputed_lowercase_key() {
        let entries = vec![
            scanned_entry("beta.TXT", EntryKind::File, Some(1), Some(1)),
            scanned_entry(".profile", EntryKind::File, Some(1), Some(1)),
            scanned_entry("README", EntryKind::File, Some(1), Some(1)),
            scanned_entry("alpha.rs", EntryKind::File, Some(1), Some(1)),
            scanned_entry("zeta.RS", EntryKind::File, Some(1), Some(1)),
        ];

        assert_eq!(extension_sort_key(".profile"), "");
        assert_eq!(extension_sort_key("archive.tar.gz"), "gz");
        assert_eq!(
            sorted_names(
                entries,
                ScanOptions {
                    sort: SortOrder::Mixed,
                    key: SortKey::Extension,
                    ..ScanOptions::default()
                },
            ),
            [".profile", "README", "alpha.rs", "zeta.RS", "beta.TXT"]
        );
    }

    fn assert_partial_matches_full(
        root: &Path,
        previous: &BaselineTree,
        changed_paths: impl IntoIterator<Item = PathBuf>,
    ) -> BaselineTree {
        let changed_paths = changed_paths.into_iter().collect::<BTreeSet<_>>();
        let mut partial_ids = allocator_after(previous);
        let mut full_ids = allocator_after(previous);

        let partial = rescan_changed_preserving_ids_with(
            root,
            &mut partial_ids,
            previous,
            &changed_paths,
            &ScanOptions::default(),
        )
        .unwrap();
        let full =
            rescan_preserving_ids_with(root, &mut full_ids, previous, &ScanOptions::default())
                .unwrap();

        assert_eq!(partial, full);
        partial
    }

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
    fn partial_rescan_matches_full_for_file_content_change() {
        let root = tempdir().unwrap();
        let file = root.path().join("file.txt");
        fs::write(&file, b"old").unwrap();
        let mut ids = IdAllocator::new();
        let previous = scan_baseline(root.path(), &mut ids).unwrap();

        fs::write(&file, b"new content").unwrap();
        let partial = assert_partial_matches_full(root.path(), &previous, [file]);
        let entry = partial
            .entries()
            .iter()
            .find(|entry| entry.path == TreePath::parse("file.txt"))
            .unwrap();

        assert_eq!(partial.meta(entry.id).unwrap().size, Some(11));
    }

    #[test]
    fn partial_rescan_matches_full_for_new_nested_directory_tree() {
        let root = tempdir().unwrap();
        let parent = root.path().join("parent");
        fs::create_dir(&parent).unwrap();
        fs::write(root.path().join("sibling.txt"), b"sibling").unwrap();
        let mut ids = IdAllocator::new();
        let previous = scan_baseline(root.path(), &mut ids).unwrap();

        let leaf = parent.join("new").join("nested").join("leaf.txt");
        fs::create_dir_all(leaf.parent().unwrap()).unwrap();
        fs::write(&leaf, b"leaf").unwrap();
        let partial = assert_partial_matches_full(root.path(), &previous, [leaf]);

        assert!(
            partial
                .entries()
                .iter()
                .any(|entry| entry.path == TreePath::parse("parent/new/nested/leaf.txt"))
        );
    }

    #[test]
    fn partial_rescan_matches_full_for_directory_tree_deletion() {
        let root = tempdir().unwrap();
        let deleted = root.path().join("deleted");
        fs::create_dir_all(deleted.join("nested")).unwrap();
        fs::write(deleted.join("nested").join("child.txt"), b"child").unwrap();
        fs::write(root.path().join("kept.txt"), b"kept").unwrap();
        let mut ids = IdAllocator::new();
        let previous = scan_baseline(root.path(), &mut ids).unwrap();

        fs::remove_dir_all(&deleted).unwrap();
        let partial = assert_partial_matches_full(root.path(), &previous, [deleted]);

        assert!(partial.entries().iter().all(|entry| {
            !TreePath::parse("deleted").is_strict_ancestor_of(&entry.path)
                && entry.path != TreePath::parse("deleted")
        }));
    }

    #[test]
    fn partial_rescan_matches_full_for_rename_inside_directory() {
        let root = tempdir().unwrap();
        let directory = root.path().join("directory");
        fs::create_dir(&directory).unwrap();
        let old = directory.join("old.txt");
        let new = directory.join("new.txt");
        fs::write(&old, b"content").unwrap();
        let mut ids = IdAllocator::new();
        let previous = scan_baseline(root.path(), &mut ids).unwrap();

        fs::rename(&old, &new).unwrap();
        let partial = assert_partial_matches_full(root.path(), &previous, [old, new]);

        assert!(
            partial
                .entries()
                .iter()
                .any(|entry| entry.path == TreePath::parse("directory/new.txt"))
        );
    }

    #[test]
    fn partial_rescan_matches_full_for_file_to_directory_kind_change() {
        let root = tempdir().unwrap();
        let changed = root.path().join("changed");
        fs::write(&changed, b"file").unwrap();
        let mut ids = IdAllocator::new();
        let previous = scan_baseline(root.path(), &mut ids).unwrap();
        let previous_id = previous.entries()[0].id;

        fs::remove_file(&changed).unwrap();
        fs::create_dir(&changed).unwrap();
        fs::write(changed.join("child.txt"), b"child").unwrap();
        let partial = assert_partial_matches_full(root.path(), &previous, [changed]);
        let changed = partial
            .entries()
            .iter()
            .find(|entry| entry.path == TreePath::parse("changed"))
            .unwrap();

        assert_eq!(changed.id, previous_id);
        assert_eq!(changed.kind, EntryKind::Dir);
        assert!(
            partial
                .entries()
                .iter()
                .any(|entry| entry.path == TreePath::parse("changed/child.txt"))
        );
    }

    #[test]
    fn partial_rescan_matches_full_for_change_below_excluded_hidden_directory() {
        let root = tempdir().unwrap();
        let hidden = root.path().join(".hidden");
        fs::create_dir(&hidden).unwrap();
        fs::write(hidden.join("old.txt"), b"old").unwrap();
        fs::write(root.path().join("visible.txt"), b"visible").unwrap();
        let mut ids = IdAllocator::new();
        let previous = scan_baseline(root.path(), &mut ids).unwrap();

        let changed = hidden.join("new.txt");
        fs::write(&changed, b"new").unwrap();
        let partial = assert_partial_matches_full(root.path(), &previous, [changed]);

        assert_eq!(partial, previous);
    }

    #[test]
    fn partial_rescan_falls_back_for_path_outside_root() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("old.txt"), b"old").unwrap();
        let mut ids = IdAllocator::new();
        let previous = scan_baseline(root.path(), &mut ids).unwrap();
        fs::write(root.path().join("new.txt"), b"new").unwrap();
        let outside = tempdir().unwrap();

        let partial =
            assert_partial_matches_full(root.path(), &previous, [outside.path().join("event")]);

        assert!(
            partial
                .entries()
                .iter()
                .any(|entry| entry.path == TreePath::parse("new.txt"))
        );
    }

    #[test]
    fn partial_rescan_falls_back_for_empty_change_set() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("old.txt"), b"old").unwrap();
        let mut ids = IdAllocator::new();
        let previous = scan_baseline(root.path(), &mut ids).unwrap();
        fs::write(root.path().join("new.txt"), b"new").unwrap();

        let partial = assert_partial_matches_full(root.path(), &previous, []);

        assert!(
            partial
                .entries()
                .iter()
                .any(|entry| entry.path == TreePath::parse("new.txt"))
        );
    }

    #[test]
    fn partial_rescan_matches_full_for_case_only_rename() {
        let root = tempdir().unwrap();
        let old = root.path().join("Foo.txt");
        let new = root.path().join("foo.txt");
        fs::write(&old, b"content").unwrap();
        let mut ids = IdAllocator::new();
        let previous = scan_baseline(root.path(), &mut ids).unwrap();
        let old_id = previous
            .entries()
            .iter()
            .find(|entry| entry.path == TreePath::parse("Foo.txt"))
            .unwrap()
            .id;

        fs::rename(&old, &new).unwrap();
        let partial = assert_partial_matches_full(root.path(), &previous, [old, new]);
        assert!(
            partial
                .entries()
                .iter()
                .all(|entry| entry.path != TreePath::parse("Foo.txt"))
        );
        let renamed = partial
            .entries()
            .iter()
            .find(|entry| entry.path == TreePath::parse("foo.txt"))
            .unwrap();

        assert_ne!(renamed.id, old_id);
    }

    #[test]
    fn partial_rescan_matches_full_for_directory_move_between_siblings() {
        let root = tempdir().unwrap();
        let old = root.path().join("a").join("sub");
        let new = root.path().join("b").join("sub");
        fs::create_dir_all(&old).unwrap();
        fs::create_dir(root.path().join("b")).unwrap();
        fs::write(old.join("child.txt"), b"child").unwrap();
        let mut ids = IdAllocator::new();
        let previous = scan_baseline(root.path(), &mut ids).unwrap();
        let old_directory_id = previous
            .entries()
            .iter()
            .find(|entry| entry.path == TreePath::parse("a/sub"))
            .unwrap()
            .id;
        let old_child_id = previous
            .entries()
            .iter()
            .find(|entry| entry.path == TreePath::parse("a/sub/child.txt"))
            .unwrap()
            .id;

        fs::rename(&old, &new).unwrap();
        let partial = assert_partial_matches_full(root.path(), &previous, [old, new]);
        assert!(partial.entries().iter().all(|entry| {
            entry.path != TreePath::parse("a/sub")
                && !TreePath::parse("a/sub").is_strict_ancestor_of(&entry.path)
        }));
        let moved_directory = partial
            .entries()
            .iter()
            .find(|entry| entry.path == TreePath::parse("b/sub"))
            .unwrap();
        let moved_child = partial
            .entries()
            .iter()
            .find(|entry| entry.path == TreePath::parse("b/sub/child.txt"))
            .unwrap();

        assert_ne!(moved_directory.id, old_directory_id);
        assert_ne!(moved_child.id, old_child_id);
    }

    #[test]
    fn partial_rescan_preserves_ids_across_consecutive_rescans() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("stable.txt"), b"stable").unwrap();
        let mut ids = IdAllocator::new();
        let initial = scan_baseline(root.path(), &mut ids).unwrap();
        let stable_id = initial
            .entries()
            .iter()
            .find(|entry| entry.path == TreePath::parse("stable.txt"))
            .unwrap()
            .id;

        let first_path = root.path().join("first.txt");
        fs::write(&first_path, b"first").unwrap();
        let first = assert_partial_matches_full(root.path(), &initial, [first_path]);
        let first_id = first
            .entries()
            .iter()
            .find(|entry| entry.path == TreePath::parse("first.txt"))
            .unwrap()
            .id;

        let second_path = root.path().join("second.txt");
        fs::write(&second_path, b"second").unwrap();
        let second = assert_partial_matches_full(root.path(), &first, [second_path]);
        let stable_after_second = second
            .entries()
            .iter()
            .find(|entry| entry.path == TreePath::parse("stable.txt"))
            .unwrap();
        let first_after_second = second
            .entries()
            .iter()
            .find(|entry| entry.path == TreePath::parse("first.txt"))
            .unwrap();

        assert_eq!(stable_after_second.id, stable_id);
        assert_eq!(first_after_second.id, first_id);
        assert!(
            second
                .entries()
                .iter()
                .any(|entry| entry.path == TreePath::parse("second.txt"))
        );
    }

    #[test]
    fn partial_rescan_matches_full_with_hidden_entries_shown() {
        let root = tempdir().unwrap();
        let hidden = root.path().join(".hidden");
        fs::create_dir(&hidden).unwrap();
        fs::write(hidden.join("existing.txt"), b"existing").unwrap();
        let options = ScanOptions {
            show_hidden: true,
            ..ScanOptions::default()
        };
        let mut ids = IdAllocator::new();
        let previous = scan_baseline_with(root.path(), &mut ids, &options).unwrap();
        let existing_id = previous
            .entries()
            .iter()
            .find(|entry| entry.path == TreePath::parse(".hidden/existing.txt"))
            .unwrap()
            .id;

        let added = hidden.join("added.txt");
        fs::write(&added, b"added").unwrap();
        let changed_paths = BTreeSet::from([added]);
        let mut partial_ids = allocator_after(&previous);
        let mut full_ids = allocator_after(&previous);
        let partial = rescan_changed_preserving_ids_with(
            root.path(),
            &mut partial_ids,
            &previous,
            &changed_paths,
            &options,
        )
        .unwrap();
        let full =
            rescan_preserving_ids_with(root.path(), &mut full_ids, &previous, &options).unwrap();

        assert_eq!(partial, full);
        assert!(
            partial
                .entries()
                .iter()
                .any(|entry| entry.path == TreePath::parse(".hidden/added.txt"))
        );
        assert_eq!(
            partial
                .entries()
                .iter()
                .find(|entry| entry.path == TreePath::parse(".hidden/existing.txt"))
                .unwrap()
                .id,
            existing_id
        );
    }

    #[test]
    fn partial_rescan_matches_full_when_root_itself_changed() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("removed.txt"), b"removed").unwrap();
        fs::write(root.path().join("kept.txt"), b"kept").unwrap();
        let mut ids = IdAllocator::new();
        let previous = scan_baseline(root.path(), &mut ids).unwrap();

        fs::remove_file(root.path().join("removed.txt")).unwrap();
        fs::write(root.path().join("added.txt"), b"added").unwrap();
        let partial =
            assert_partial_matches_full(root.path(), &previous, [root.path().to_path_buf()]);

        assert!(
            partial
                .entries()
                .iter()
                .all(|entry| entry.path != TreePath::parse("removed.txt"))
        );
        assert!(
            partial
                .entries()
                .iter()
                .any(|entry| entry.path == TreePath::parse("added.txt"))
        );
    }

    #[test]
    fn scan_stores_metadata_for_files_and_directories() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("directory")).unwrap();
        fs::write(root.path().join("file.txt"), b"content").unwrap();
        let mut ids = IdAllocator::new();

        let baseline = scan_baseline(root.path(), &mut ids).unwrap();
        let directory = baseline
            .entries()
            .iter()
            .find(|entry| entry.path == TreePath::parse("directory"))
            .unwrap();
        let file = baseline
            .entries()
            .iter()
            .find(|entry| entry.path == TreePath::parse("file.txt"))
            .unwrap();

        assert_eq!(baseline.meta(directory.id).unwrap().size, None);
        assert!(baseline.meta(directory.id).unwrap().modified.is_some());
        assert_eq!(baseline.meta(file.id).unwrap().size, Some(7));
        assert!(baseline.meta(file.id).unwrap().modified.is_some());
        assert!(!baseline.meta(file.id).unwrap().is_placeholder);
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

    #[cfg(unix)]
    #[test]
    fn excluded_hidden_non_utf8_name_does_not_fail_scan() {
        use std::os::unix::ffi::OsStringExt;

        let root = tempdir().unwrap();
        let hidden_non_utf8 = OsString::from_vec(vec![b'.', 0xff]);
        fs::write(root.path().join(hidden_non_utf8), b"hidden").unwrap();
        let mut ids = IdAllocator::new();

        let baseline = scan_baseline(root.path(), &mut ids).unwrap();

        assert!(baseline.entries().is_empty());
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
