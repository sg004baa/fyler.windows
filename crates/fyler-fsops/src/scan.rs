//! baselineスキャン: 実FS → BaselineTree(ID採番)。
//!
//! root自体の検証・列挙失敗だけをfatalとし、子subtreeのアクセス失敗は
//! [`BaselineTree`]のaccess sidecarへ記録して兄弟の走査を継続する。非Unicode名は
//! lossy表示の警告だけを残し、編集可能なentryへ偽装しない。

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

use anyhow::{Context, anyhow, bail};
use fyler_core::fileinfo::EntryMeta;
use fyler_core::id::IdAllocator;
use fyler_core::options::{SortKey, SortOrder};
use fyler_core::path::TreePath;
use fyler_core::scanwarn::{ScanErrorKind, ScanStage, ScanWarning};
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
    scan_with_id_resolver(root, options, None, |_: &TreePath| ids.allocate())
}

/// 指定した表示対象オプションで、キャンセル可能な全再帰スキャンを行う。
///
/// 既存の全再帰経路との互換用APIであり、lazy baselineの初期構築には
/// [`scan_baseline_shallow_cancellable_with`]を使う。
pub fn scan_baseline_cancellable_with(
    root: &Path,
    mut resolve_id: impl FnMut(&TreePath) -> anyhow::Result<fyler_core::id::EntryId> + Send,
    options: &ScanOptions,
    mut progress: impl FnMut(usize) + Send,
    cancel: &AtomicBool,
) -> anyhow::Result<Option<BaselineTree>> {
    scan_subtree_cancellable(
        ScanStart {
            root,
            directory: root,
            relative: &TreePath::root(),
        },
        None,
        &mut resolve_id,
        options,
        &mut progress,
        cancel,
    )
}

/// ルート直下だけを列挙し、子ディレクトリを未ロードとして返す。
pub fn scan_baseline_shallow_with(
    root: &Path,
    ids: &mut IdAllocator,
    options: &ScanOptions,
) -> anyhow::Result<BaselineTree> {
    let cancel = AtomicBool::new(false);
    scan_baseline_shallow_cancellable_with(root, |_| Ok(ids.allocate()), options, |_| {}, &cancel)?
        .ok_or_else(|| anyhow!("Uncancellable shallow scan was cancelled"))
}

/// ルート直下だけをキャンセル可能に列挙する。
pub fn scan_baseline_shallow_cancellable_with(
    root: &Path,
    mut resolve_id: impl FnMut(&TreePath) -> anyhow::Result<fyler_core::id::EntryId> + Send,
    options: &ScanOptions,
    mut progress: impl FnMut(usize) + Send,
    cancel: &AtomicBool,
) -> anyhow::Result<Option<BaselineTree>> {
    if cancel.load(AtomicOrdering::Relaxed) {
        return Ok(None);
    }
    validate_root(root)?;
    let mut control = ScanControl::enabled(cancel, &mut progress);
    let Some(read) =
        read_sorted_entries_cancellable(root, options, &mut control).map_err(|failure| {
            anyhow!(failure.error).context(format!(
                "Failed while {}: {}",
                failure.stage,
                failure.path.display()
            ))
        })?
    else {
        return Ok(None);
    };
    let mut tree = BaselineTree::new(root);
    apply_read_access_state(&mut tree, &TreePath::root(), &read);
    for entry in read.entries {
        if control.cancelled() {
            return Ok(None);
        }
        let path = TreePath::root().child(entry.name);
        let kind = entry.kind;
        tree.insert_with_meta(
            BaselineEntry {
                id: resolve_id(&path)?,
                path: path.clone(),
                kind,
            },
            entry.meta,
        );
        if kind == EntryKind::Dir {
            tree.mark_unloaded(path);
        }
    }
    Ok(Some(tree))
}

/// 未ロードディレクトリの直下1階層を列挙してDFS位置へ挿入する。
pub fn load_directory(
    root: &Path,
    dir: &TreePath,
    ids: &mut IdAllocator,
    previous: &BaselineTree,
    options: &ScanOptions,
) -> anyhow::Result<BaselineTree> {
    validate_load_target(root, dir, previous)?;
    if !previous.is_unloaded(dir) {
        bail!("Load target is already loaded: {dir}");
    }
    let read = read_sorted_entries(&dir.to_fs_path(root), options).map_err(|failure| {
        anyhow!(failure.error).context(format!(
            "Failed while {}: {}",
            failure.stage,
            failure.path.display()
        ))
    })?;
    let mut loaded = BaselineTree::new(root);
    apply_read_access_state(&mut loaded, dir, &read);
    for entry in read.entries {
        let path = dir.child(entry.name);
        let id = previous
            .get_by_path(&path)
            .map(|entry| entry.id)
            .unwrap_or_else(|| ids.allocate());
        let kind = entry.kind;
        loaded.insert_with_meta(
            BaselineEntry {
                id,
                path: path.clone(),
                kind,
            },
            entry.meta,
        );
        if kind == EntryKind::Dir {
            loaded.mark_unloaded(path);
        }
    }
    Ok(splice_loaded_subtree(previous, dir, &loaded))
}

/// 未ロードディレクトリ以下を再帰的かつキャンセル可能にロードする。
pub fn load_directory_recursive_cancellable(
    root: &Path,
    dir: &TreePath,
    mut resolve_id: impl FnMut(&TreePath) -> anyhow::Result<fyler_core::id::EntryId> + Send,
    previous: &BaselineTree,
    options: &ScanOptions,
    mut progress: impl FnMut(usize) + Send,
    cancel: &AtomicBool,
) -> anyhow::Result<Option<BaselineTree>> {
    validate_load_target(root, dir, previous)?;
    let Some(loaded) = scan_subtree_cancellable(
        ScanStart {
            root,
            directory: &dir.to_fs_path(root),
            relative: dir,
        },
        Some(previous),
        &mut |path| {
            previous
                .get_by_path(path)
                .map(|entry| Ok(entry.id))
                .unwrap_or_else(|| resolve_id(path))
        },
        options,
        &mut progress,
        cancel,
    )?
    else {
        return Ok(None);
    };
    Ok(Some(splice_loaded_subtree(previous, dir, &loaded)))
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
    scan_with_coverage_id_resolver(root, options, previous, |path| {
        previous
            .get_by_path(path)
            .map(|entry| entry.id)
            .unwrap_or_else(|| ids.allocate())
    })
}

/// watcherが報告した変更パスから影響ディレクトリだけを再スキャンする。
///
/// 影響外のエントリ・ID・メタデータは`previous`から引き継ぐ。新規パスは必ず
/// 実FSを列挙する領域にだけ現れ、部分再構築でも全再スキャンと同じDFS順で到達する。
/// したがって新規IDの採番順も[`rescan_preserving_ids_with`]と一致する。
///
/// 変更パスをルート相対UTF-8パスへ変換できない場合や、部分再構築中に親子関係の
/// raceを検出した場合は、安全のため全再スキャンへ戻る。子ディレクトリのアクセス
/// 失敗は全再スキャンせず、その部分木を`previous`から引き継いで不完全と記録する。
/// 非影響の不完全ディレクトリは再評価しないため、実FSで既に回復していてもmarkerを
/// 引き継ぐ。定期probeまたはその範囲のwatchイベントで再評価され、最終的に収束する。
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
    let mut affected = HashSet::new();
    for path in changed_paths {
        if previous
            .unloaded_dirs()
            .iter()
            .any(|unloaded| unloaded.is_strict_ancestor_of(path))
        {
            continue;
        }
        let mut ancestor = path.parent().unwrap_or_else(TreePath::root);
        loop {
            let is_existing_dir = previous
                .get_by_path(&ancestor)
                .is_some_and(|entry| entry.kind == EntryKind::Dir);
            if ancestor.is_root() || is_existing_dir {
                affected.insert(ancestor);
                break;
            }
            ancestor = ancestor.parent().unwrap_or_else(TreePath::root);
        }

        if previous
            .get_by_path(path)
            .is_some_and(|entry| entry.kind == EntryKind::Dir)
        {
            affected.insert(path.clone());
        }
    }

    if affected.is_empty() {
        return Ok(previous.clone());
    }

    validate_root(root)?;

    if let Some(tree) = refresh_metadata_if_structure_unchanged(root, previous, &affected, options)?
    {
        return Ok(tree);
    }

    let mut tree = BaselineTree::new(root);
    rebuild_directory(
        root,
        &TreePath::root(),
        ids,
        previous,
        &affected,
        options,
        &mut tree,
    )?;
    Ok(tree)
}

fn refresh_metadata_if_structure_unchanged(
    root: &Path,
    previous: &BaselineTree,
    affected: &HashSet<TreePath>,
    options: &ScanOptions,
) -> anyhow::Result<Option<BaselineTree>> {
    // 不完全dir自身がprobe対象ならmarker解消を判定する必要があるため、sidecarを
    // そのままcloneするfast pathではなく再構築へ送る。
    if affected
        .iter()
        .any(|relative| previous.incomplete_dirs().contains_key(relative))
    {
        return Ok(None);
    }

    let mut updates = Vec::new();
    for relative in affected {
        if previous.is_unloaded(relative) {
            continue;
        }
        let scanned = match read_sorted_entries(&relative.to_fs_path(root), options) {
            Ok(read) if read.incomplete_kind.is_none() && read.warnings.is_empty() => read.entries,
            Ok(_) | Err(_) => return Ok(None),
        };
        let parent = if relative.is_root() {
            None
        } else {
            let Some(parent) = previous.index_by_path(relative) else {
                return Ok(None);
            };
            Some(parent)
        };
        let children = previous.child_indices(parent);
        if scanned.len() != children.len() {
            return Ok(None);
        }

        for (scanned, index) in scanned.into_iter().zip(children) {
            let name = scanned.file_name.to_str().with_context(|| {
                format!(
                    "File name cannot be represented as UTF-8: {}",
                    scanned.path.display()
                )
            })?;
            let previous_entry = &previous.entries()[index];
            if previous_entry.path != relative.child(name) || previous_entry.kind != scanned.kind {
                return Ok(None);
            }
            updates.push((previous_entry.id, scanned.meta));
        }
    }

    Ok(Some(previous.clone_with_meta_updates(updates)))
}

fn rebuild_directory(
    root: &Path,
    relative: &TreePath,
    ids: &mut IdAllocator,
    previous: &BaselineTree,
    affected: &HashSet<TreePath>,
    options: &ScanOptions,
    tree: &mut BaselineTree,
) -> anyhow::Result<()> {
    if previous.is_unloaded(relative) {
        preserve_previous_access_state(previous, relative, tree);
        tree.mark_unloaded(relative.clone());
        return Ok(());
    }

    let was_directory = relative.is_root()
        || previous
            .get_by_path(relative)
            .is_some_and(|entry| entry.kind == EntryKind::Dir);
    let should_scan = affected.contains(relative) || !was_directory;

    if should_scan {
        let directory = relative.to_fs_path(root);
        let read = match read_sorted_entries(&directory, options) {
            Ok(read) => read,
            Err(failure) if !relative.is_root() => {
                preserve_previous_subtree_after_failure(previous, relative, tree);
                let kind = classify_io_error(&failure.error);
                tree.mark_incomplete(relative.clone(), kind.clone());
                tree.push_warning(ScanWarning {
                    path: failure.path,
                    stage: failure.stage,
                    kind,
                });
                return Ok(());
            }
            Err(failure) => {
                return Err(anyhow!(failure.error).context(format!(
                    "Failed while {}: {}",
                    failure.stage,
                    failure.path.display()
                )));
            }
        };
        apply_read_access_state(tree, relative, &read);
        for entry in read.entries {
            let name = entry.file_name.to_str().with_context(|| {
                format!(
                    "File name cannot be represented as UTF-8: {}",
                    entry.path.display()
                )
            })?;
            let path = relative.child(name);
            let id = previous
                .get_by_path(&path)
                .map(|entry| entry.id)
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
                if previous.get_by_path(&path).is_none() {
                    tree.mark_unloaded(path);
                } else {
                    rebuild_directory(root, &path, ids, previous, affected, options, tree)
                        .with_context(|| {
                            format!(
                                "Failed to rebuild changed directory: {}",
                                child_directory.display()
                            )
                        })?;
                }
            }
        }
    } else {
        preserve_previous_access_state(previous, relative, tree);
        let parent = previous.index_by_path(relative);
        for index in previous.child_indices(parent) {
            let entry = previous.entries()[index].clone();
            let id = entry.id;
            let kind = entry.kind;
            let path = entry.path.clone();
            if let Some(meta) = previous.meta(id).copied() {
                tree.insert_with_meta(entry, meta);
            } else {
                tree.insert(entry);
            }

            if kind == EntryKind::Dir {
                rebuild_directory(root, &path, ids, previous, affected, options, tree)?;
            }
        }
    }

    Ok(())
}

fn scan_with_coverage_id_resolver(
    root: &Path,
    options: &ScanOptions,
    previous: &BaselineTree,
    mut resolve_id: impl FnMut(&TreePath) -> fyler_core::id::EntryId,
) -> anyhow::Result<BaselineTree> {
    validate_root(root)?;
    let mut tree = BaselineTree::new(root);
    scan_directory_coverage(
        root,
        &TreePath::root(),
        options,
        previous,
        &mut resolve_id,
        &mut tree,
    )?;
    Ok(tree)
}

fn scan_directory_coverage(
    directory: &Path,
    relative: &TreePath,
    options: &ScanOptions,
    previous: &BaselineTree,
    resolve_id: &mut impl FnMut(&TreePath) -> fyler_core::id::EntryId,
    tree: &mut BaselineTree,
) -> anyhow::Result<()> {
    let read = read_sorted_entries(directory, options).map_err(|failure| {
        anyhow!(failure.error).context(format!(
            "Failed while {}: {}",
            failure.stage,
            failure.path.display()
        ))
    })?;
    apply_read_access_state(tree, relative, &read);
    for entry in read.entries {
        let path = relative.child(entry.name);
        let kind = entry.kind;
        tree.insert_with_meta(
            BaselineEntry {
                id: resolve_id(&path),
                path: path.clone(),
                kind,
            },
            entry.meta,
        );
        if kind != EntryKind::Dir {
            continue;
        }
        if previous.is_unloaded(&path) || previous.get_by_path(&path).is_none() {
            tree.mark_unloaded(path);
            continue;
        }
        match scan_directory_coverage(&entry.path, &path, options, previous, resolve_id, tree) {
            Ok(()) => {}
            Err(error) => {
                preserve_previous_subtree(previous, &path, tree);
                let kind = error
                    .downcast_ref::<io::Error>()
                    .map(classify_io_error)
                    .unwrap_or_else(|| ScanErrorKind::Other(error.to_string()));
                tree.mark_incomplete(path.clone(), kind.clone());
                tree.push_warning(ScanWarning {
                    path: entry.path,
                    stage: ScanStage::EnumerateDir,
                    kind,
                });
            }
        }
    }
    Ok(())
}

fn scan_with_id_resolver(
    root: &Path,
    options: &ScanOptions,
    previous: Option<&BaselineTree>,
    mut resolve_id: impl FnMut(&TreePath) -> fyler_core::id::EntryId,
) -> anyhow::Result<BaselineTree> {
    validate_root(root)?;

    let mut tree = BaselineTree::new(root);
    scan_directory(
        root,
        &TreePath::root(),
        options,
        previous,
        &mut resolve_id,
        &mut tree,
    )?;
    Ok(tree)
}

struct ScanStart<'a> {
    root: &'a Path,
    directory: &'a Path,
    relative: &'a TreePath,
}

fn scan_subtree_cancellable(
    start: ScanStart<'_>,
    previous: Option<&BaselineTree>,
    resolve_id: &mut (impl FnMut(&TreePath) -> anyhow::Result<fyler_core::id::EntryId> + ?Sized),
    options: &ScanOptions,
    progress: &mut dyn FnMut(usize),
    cancel: &AtomicBool,
) -> anyhow::Result<Option<BaselineTree>> {
    if cancel.load(AtomicOrdering::Relaxed) {
        return Ok(None);
    }
    validate_root(start.root)?;
    let mut tree = BaselineTree::new(start.root);
    let mut control = ScanControl::enabled(cancel, progress);
    if !scan_directory_cancellable(
        start.directory,
        start.relative,
        options,
        previous,
        resolve_id,
        &mut tree,
        &mut control,
    )? {
        return Ok(None);
    }
    Ok(Some(tree))
}

fn scan_directory_cancellable(
    directory: &Path,
    relative: &TreePath,
    options: &ScanOptions,
    previous: Option<&BaselineTree>,
    resolve_id: &mut (impl FnMut(&TreePath) -> anyhow::Result<fyler_core::id::EntryId> + ?Sized),
    tree: &mut BaselineTree,
    control: &mut ScanControl<'_>,
) -> anyhow::Result<bool> {
    if control.cancelled() {
        return Ok(false);
    }
    let root_read =
        read_sorted_entries_cancellable(directory, options, control).map_err(|failure| {
            anyhow!(failure.error).context(format!(
                "Failed while {}: {}",
                failure.stage,
                failure.path.display()
            ))
        })?;
    let Some(root_read) = root_read else {
        return Ok(false);
    };
    apply_read_access_state(tree, relative, &root_read);
    let mut stack = vec![ScanFrame {
        entries: root_read.entries.into_iter(),
        relative: relative.clone(),
    }];

    while let Some(frame) = stack.last_mut() {
        if control.cancelled() {
            return Ok(false);
        }
        let Some(entry) = frame.entries.next() else {
            stack.pop();
            continue;
        };
        let path = frame.relative.child(entry.name);
        let kind = entry.kind;
        tree.insert_with_meta(
            BaselineEntry {
                id: resolve_id(&path)?,
                path: path.clone(),
                kind,
            },
            entry.meta,
        );

        if kind == EntryKind::Dir {
            if control.cancelled() {
                return Ok(false);
            }
            match read_sorted_entries_cancellable(&entry.path, options, control) {
                Ok(Some(read)) => {
                    apply_read_access_state(tree, &path, &read);
                    stack.push(ScanFrame {
                        entries: read.entries.into_iter(),
                        relative: path,
                    });
                }
                Ok(None) => return Ok(false),
                Err(failure) => {
                    let kind = classify_io_error(&failure.error);
                    if let Some(previous) = previous {
                        preserve_previous_subtree(previous, &path, tree);
                    }
                    if previous.is_some_and(|previous| previous.is_unloaded(&path)) {
                        tree.mark_unloaded(path.clone());
                    } else {
                        tree.mark_incomplete(path.clone(), kind.clone());
                    }
                    tree.push_warning(ScanWarning {
                        path: failure.path,
                        stage: failure.stage,
                        kind,
                    });
                }
            }
        }
    }
    Ok(true)
}

struct ScanControl<'a> {
    cancel: &'a AtomicBool,
    progress: &'a mut dyn FnMut(usize),
    entries: usize,
}

impl<'a> ScanControl<'a> {
    fn enabled(cancel: &'a AtomicBool, progress: &'a mut dyn FnMut(usize)) -> Self {
        Self {
            cancel,
            progress,
            entries: 0,
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(AtomicOrdering::Relaxed)
    }

    fn record_entry(&mut self) -> bool {
        self.entries += 1;
        if self.entries % 1000 == 0 {
            (self.progress)(self.entries);
        }
        !self.cancelled()
    }
}

fn validate_load_target(
    root: &Path,
    dir: &TreePath,
    previous: &BaselineTree,
) -> anyhow::Result<()> {
    if previous.root != root {
        bail!(
            "Baseline root does not match load root: {} != {}",
            previous.root.display(),
            root.display()
        );
    }
    if !dir.is_root()
        && !previous
            .get_by_path(dir)
            .is_some_and(|entry| entry.kind == EntryKind::Dir)
    {
        bail!("Load target is not a baseline directory: {dir}");
    }
    Ok(())
}

fn splice_loaded_subtree(
    previous: &BaselineTree,
    dir: &TreePath,
    loaded: &BaselineTree,
) -> BaselineTree {
    let mut tree = BaselineTree::new(&previous.root);
    for entry in previous.entries() {
        if dir.is_strict_ancestor_of(&entry.path) {
            continue;
        }
        copy_entry(previous, entry, &mut tree);
        if &entry.path == dir {
            for loaded_entry in loaded.entries() {
                copy_entry(loaded, loaded_entry, &mut tree);
            }
        }
    }
    if dir.is_root() {
        for entry in loaded.entries() {
            copy_entry(loaded, entry, &mut tree);
        }
    }

    for (path, kind) in previous.incomplete_dirs() {
        if path != dir && !dir.is_strict_ancestor_of(path) {
            tree.mark_incomplete(path.clone(), kind.clone());
        }
    }
    for (path, kind) in loaded.incomplete_dirs() {
        tree.mark_incomplete(path.clone(), kind.clone());
    }
    for path in previous.unloaded_dirs() {
        if path != dir && !dir.is_strict_ancestor_of(path) {
            tree.mark_unloaded(path.clone());
        }
    }
    for path in loaded.unloaded_dirs() {
        tree.mark_unloaded(path.clone());
    }

    let fs_dir = dir.to_fs_path(&previous.root);
    for warning in previous.scan_warnings() {
        if !warning.path.starts_with(&fs_dir) {
            tree.push_warning(warning.clone());
        }
    }
    for warning in loaded.scan_warnings() {
        tree.push_warning(warning.clone());
    }
    tree
}

fn copy_entry(source: &BaselineTree, entry: &BaselineEntry, target: &mut BaselineTree) {
    if let Some(meta) = source.meta(entry.id).copied() {
        target.insert_with_meta(entry.clone(), meta);
    } else {
        target.insert(entry.clone());
    }
}

fn validate_root(root: &Path) -> anyhow::Result<()> {
    let root_metadata = fs::symlink_metadata(crate::long_path::to_fs(root))
        .with_context(|| format!("Failed to get root metadata: {}", root.display()))?;
    if is_link_or_reparse(&root_metadata) {
        bail!(
            "Root cannot be a symlink, junction, or reparse point: {}",
            root.display()
        );
    }
    if !root_metadata.is_dir() {
        bail!("Root is not a directory: {}", root.display());
    }
    Ok(())
}

fn scan_directory(
    directory: &Path,
    relative: &TreePath,
    options: &ScanOptions,
    previous: Option<&BaselineTree>,
    resolve_id: &mut impl FnMut(&TreePath) -> fyler_core::id::EntryId,
    tree: &mut BaselineTree,
) -> anyhow::Result<()> {
    let root_read = read_sorted_entries(directory, options).map_err(|failure| {
        anyhow!(failure.error).context(format!(
            "Failed while {}: {}",
            failure.stage,
            failure.path.display()
        ))
    })?;
    apply_read_access_state(tree, relative, &root_read);
    let mut stack = vec![ScanFrame {
        entries: root_read.entries.into_iter(),
        relative: relative.clone(),
    }];

    while let Some(frame) = stack.last_mut() {
        let Some(entry) = frame.entries.next() else {
            stack.pop();
            continue;
        };

        let path = frame.relative.child(entry.name);
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
            match read_sorted_entries(&entry.path, options) {
                Ok(read) => {
                    apply_read_access_state(tree, &path, &read);
                    stack.push(ScanFrame {
                        entries: read.entries.into_iter(),
                        relative: path,
                    });
                }
                Err(failure) => {
                    let kind = classify_io_error(&failure.error);
                    if let Some(previous) = previous {
                        preserve_previous_subtree(previous, &path, tree);
                    }
                    tree.mark_incomplete(path.clone(), kind.clone());
                    tree.push_warning(ScanWarning {
                        path: failure.path,
                        stage: failure.stage,
                        kind,
                    });
                }
            }
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
    name: String,
    sort_key: String,
    extension_key: String,
    kind: EntryKind,
    meta: EntryMeta,
}

struct ReadEntries {
    entries: Vec<ScannedEntry>,
    warnings: Vec<ScanWarning>,
    incomplete_kind: Option<ScanErrorKind>,
}

struct DirectoryReadFailure {
    path: PathBuf,
    stage: ScanStage,
    error: io::Error,
}

#[cfg(any(test, feature = "test-support"))]
type FaultHook = Box<dyn FnMut(&str, &Path) -> Option<io::Error>>;

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static FAULT_INJECTION: std::cell::RefCell<Option<FaultHook>> =
        std::cell::RefCell::new(None);
}

#[cfg(any(test, feature = "test-support"))]
fn fault_point(stage: &str, path: &Path) -> io::Result<()> {
    FAULT_INJECTION.with(|hook| {
        let mut hook = hook.borrow_mut();
        if let Some(error) = hook.as_mut().and_then(|hook| hook(stage, path)) {
            Err(error)
        } else {
            Ok(())
        }
    })
}

#[cfg(not(any(test, feature = "test-support")))]
fn fault_point(_stage: &str, _path: &Path) -> io::Result<()> {
    Ok(())
}

/// scanの失敗点を差し替えるテスト専用フック。
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub fn with_test_fault<R>(
    hook: impl FnMut(&str, &Path) -> Option<io::Error> + 'static,
    run: impl FnOnce() -> R,
) -> R {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            FAULT_INJECTION.with(|hook| *hook.borrow_mut() = None);
        }
    }

    FAULT_INJECTION.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
    let _reset = Reset;
    run()
}

fn classify_io_error(error: &io::Error) -> ScanErrorKind {
    match error.kind() {
        io::ErrorKind::PermissionDenied => ScanErrorKind::PermissionDenied,
        io::ErrorKind::NotFound => ScanErrorKind::NotFound,
        io::ErrorKind::TimedOut => ScanErrorKind::TimedOut,
        _ => ScanErrorKind::Other(error.to_string()),
    }
}

fn apply_read_access_state(tree: &mut BaselineTree, relative: &TreePath, read: &ReadEntries) {
    if let Some(kind) = &read.incomplete_kind {
        tree.mark_incomplete(relative.clone(), kind.clone());
    }
    for warning in &read.warnings {
        tree.push_warning(warning.clone());
    }
}

fn preserve_previous_subtree(
    previous: &BaselineTree,
    directory: &TreePath,
    tree: &mut BaselineTree,
) {
    for entry in previous
        .entries()
        .iter()
        .filter(|entry| directory.is_strict_ancestor_of(&entry.path))
    {
        if let Some(meta) = previous.meta(entry.id).copied() {
            tree.insert_with_meta(entry.clone(), meta);
        } else {
            tree.insert(entry.clone());
        }
    }

    for (path, kind) in previous.incomplete_dirs() {
        if path == directory || directory.is_strict_ancestor_of(path) {
            tree.mark_incomplete(path.clone(), kind.clone());
        }
    }
    for path in previous.unloaded_dirs() {
        if path == directory || directory.is_strict_ancestor_of(path) {
            tree.mark_unloaded(path.clone());
        }
    }

    let fs_directory = directory.to_fs_path(&previous.root);
    for warning in previous.scan_warnings() {
        if warning.path != fs_directory && warning.path.starts_with(&fs_directory) {
            tree.push_warning(warning.clone());
        }
    }
}

fn preserve_previous_access_state(
    previous: &BaselineTree,
    directory: &TreePath,
    tree: &mut BaselineTree,
) {
    if let Some(kind) = previous.incomplete_dirs().get(directory) {
        tree.mark_incomplete(directory.clone(), kind.clone());
    }
    if previous.is_unloaded(directory) {
        tree.mark_unloaded(directory.clone());
    }

    let fs_directory = directory.to_fs_path(&previous.root);
    for warning in previous.scan_warnings() {
        if warning.path == fs_directory || warning.path.parent() == Some(fs_directory.as_path()) {
            tree.push_warning(warning.clone());
        }
    }
}

fn preserve_previous_subtree_after_failure(
    previous: &BaselineTree,
    directory: &TreePath,
    tree: &mut BaselineTree,
) {
    for entry in previous
        .entries()
        .iter()
        .filter(|entry| directory.is_strict_ancestor_of(&entry.path))
    {
        if let Some(meta) = previous.meta(entry.id).copied() {
            tree.insert_with_meta(entry.clone(), meta);
        } else {
            tree.insert(entry.clone());
        }
    }

    for (path, kind) in previous.incomplete_dirs() {
        if directory.is_strict_ancestor_of(path) {
            tree.mark_incomplete(path.clone(), kind.clone());
        }
    }
    for path in previous.unloaded_dirs() {
        if directory.is_strict_ancestor_of(path) {
            tree.mark_unloaded(path.clone());
        }
    }

    let fs_directory = directory.to_fs_path(&previous.root);
    for warning in previous.scan_warnings() {
        if warning.path != fs_directory && warning.path.starts_with(&fs_directory) {
            tree.push_warning(warning.clone());
        }
    }
}

fn read_sorted_entries(
    directory: &Path,
    options: &ScanOptions,
) -> Result<ReadEntries, DirectoryReadFailure> {
    read_sorted_entries_impl(directory, options, None)
        .map(|read| read.expect("read without cancellation control cannot be cancelled"))
}

fn read_sorted_entries_cancellable(
    directory: &Path,
    options: &ScanOptions,
    control: &mut ScanControl<'_>,
) -> Result<Option<ReadEntries>, DirectoryReadFailure> {
    read_sorted_entries_impl(directory, options, Some(control))
}

fn read_sorted_entries_impl(
    directory: &Path,
    options: &ScanOptions,
    mut control: Option<&mut ScanControl<'_>>,
) -> Result<Option<ReadEntries>, DirectoryReadFailure> {
    if control.as_deref().is_some_and(ScanControl::cancelled) {
        return Ok(None);
    }
    let read_dir = fault_point("enumerate_dir", directory)
        .and_then(|()| fs::read_dir(crate::long_path::to_fs(directory)))
        .map_err(|error| DirectoryReadFailure {
            path: directory.to_path_buf(),
            stage: ScanStage::EnumerateDir,
            error,
        })?;
    let mut entries = Vec::new();
    let mut warnings = Vec::new();
    let mut incomplete_kind = None;
    for entry in read_dir {
        if control.as_deref().is_some_and(ScanControl::cancelled) {
            return Ok(None);
        }
        fault_point("dir_entry", directory).map_err(|error| DirectoryReadFailure {
            path: directory.to_path_buf(),
            stage: ScanStage::DirEntry,
            error,
        })?;
        let entry = entry.map_err(|error| DirectoryReadFailure {
            path: directory.to_path_buf(),
            stage: ScanStage::DirEntry,
            error,
        })?;
        let file_name = entry.file_name();
        // `DirEntry::path()`はWindowsで`\\?\`付きの親パス由来になる(read_dirへ
        // long_path::to_fs適用済みのため)。警告・診断・降下パスへprefixを漏らさない
        // よう、呼び出し側の論理パスから組み立てる(絶対ルール3)。
        let path = directory.join(&file_name);
        let metadata = match fault_point("metadata", &path).and_then(|()| entry.metadata()) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // 列挙とmetadata取得の間に消えたraceは次回scanで自然に収束する。
                continue;
            }
            Err(error) => {
                let kind = classify_io_error(&error);
                incomplete_kind.get_or_insert_with(|| kind.clone());
                warnings.push(ScanWarning {
                    path,
                    stage: ScanStage::Metadata,
                    kind,
                });
                continue;
            }
        };
        if !options.show_hidden && is_hidden(&file_name, &metadata) {
            continue;
        }
        let Some(name) = file_name.to_str() else {
            incomplete_kind.get_or_insert(ScanErrorKind::NonUnicodeName);
            warnings.push(ScanWarning {
                path: PathBuf::from(path.to_string_lossy().into_owned()),
                stage: ScanStage::Name,
                kind: ScanErrorKind::NonUnicodeName,
            });
            continue;
        };
        let name = name.to_owned();
        let sort_key = name.to_lowercase();
        let extension_key = extension_sort_key(&sort_key).to_owned();
        let kind = kind_from_metadata(&metadata);
        let meta = meta_from_metadata(&metadata);
        entries.push(ScannedEntry {
            path,
            file_name,
            name,
            sort_key,
            extension_key,
            kind,
            meta,
        });
        if let Some(control) = control.as_deref_mut()
            && !control.record_entry()
        {
            return Ok(None);
        }
    }

    if control.as_deref().is_some_and(ScanControl::cancelled) {
        return Ok(None);
    }
    // read_dirの順序は未規定なので、設定された自然順で表示とID採番を
    // セッションごとに安定させる。同値時は元のOsStringで順序を確定する。
    entries.sort_by(|left, right| compare_scanned(left, right, options));
    Ok(Some(ReadEntries {
        entries,
        warnings,
        incomplete_kind,
    }))
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

pub(crate) fn is_hidden(file_name: &OsStr, metadata: &Metadata) -> bool {
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
    use std::cell::RefCell;
    use std::collections::BTreeSet;
    use std::fs;
    use std::rc::Rc;
    use std::time::{Duration, Instant, SystemTime};

    use tempfile::tempdir;

    use super::*;

    fn with_fault<R>(hook: FaultHook, run: impl FnOnce() -> R) -> R {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                FAULT_INJECTION.with(|hook| *hook.borrow_mut() = None);
            }
        }

        FAULT_INJECTION.with(|slot| *slot.borrow_mut() = Some(hook));
        let _reset = Reset;
        run()
    }

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
            name: name.to_owned(),
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
    #[ignore = "environment-dependent performance measurement"]
    fn bench_partial_rescan_deep_leaf_on_50k_entries() {
        const DIRECTORY_COUNT: usize = 200;
        const FILES_PER_DIRECTORY: usize = 250;
        const ITERATIONS: usize = 20;

        let root = tempdir().unwrap();
        for directory_index in 0..DIRECTORY_COUNT {
            let directory = root.path().join(format!("dir-{directory_index:03}"));
            fs::create_dir(&directory).unwrap();
            for file_index in 0..FILES_PER_DIRECTORY {
                fs::write(
                    directory.join(format!("file-{file_index:03}.txt")),
                    b"baseline",
                )
                .unwrap();
            }
        }

        let mut ids = IdAllocator::new();
        let scan_started = Instant::now();
        let previous = scan_baseline(root.path(), &mut ids).unwrap();
        let scan_elapsed = scan_started.elapsed();
        let changed = root.path().join("dir-199").join("file-249.txt");
        fs::write(&changed, b"changed content").unwrap();
        let changed_paths = BTreeSet::from([changed]);

        let started = Instant::now();
        for _ in 0..ITERATIONS {
            let mut iteration_ids = allocator_after(&previous);
            let rescanned = rescan_changed_preserving_ids_with(
                root.path(),
                &mut iteration_ids,
                &previous,
                &changed_paths,
                &ScanOptions::default(),
            )
            .unwrap();
            std::hint::black_box(rescanned);
        }
        let elapsed = started.elapsed();

        eprintln!(
            "50k initial scan: {:.3} ms; partial rescan: {:.3} ms/iteration ({ITERATIONS} iterations)",
            scan_elapsed.as_secs_f64() * 1_000.0,
            elapsed.as_secs_f64() * 1_000.0 / ITERATIONS as f64,
        );
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
    fn partial_rescan_marks_new_nested_directory_unloaded() {
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
                .get_by_path(&TreePath::parse("parent/new"))
                .is_some()
        );
        assert!(partial.is_unloaded(&TreePath::parse("parent/new")));
        assert!(
            partial
                .get_by_path(&TreePath::parse("parent/new/nested/leaf.txt"))
                .is_none()
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
        assert_ne!(moved_directory.id, old_directory_id);
        assert!(partial.is_unloaded(&TreePath::parse("b/sub")));
        assert!(
            partial
                .get_by_path(&TreePath::parse("b/sub/child.txt"))
                .is_none()
        );
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

    #[test]
    fn dir_entry_failure_discards_partial_children_and_keeps_siblings() {
        let root = tempdir().unwrap();
        let blocked = root.path().join("blocked");
        fs::create_dir(&blocked).unwrap();
        fs::write(blocked.join("a.txt"), b"a").unwrap();
        fs::write(blocked.join("b.txt"), b"b").unwrap();
        fs::create_dir(root.path().join("readable")).unwrap();
        fs::write(root.path().join("readable/ok.txt"), b"ok").unwrap();
        let blocked_for_hook = blocked.clone();
        let mut seen = 0;

        let baseline = with_fault(
            Box::new(move |stage, path| {
                if stage == "dir_entry" && path == blocked_for_hook {
                    seen += 1;
                    (seen == 2).then(|| io::Error::other("injected"))
                } else {
                    None
                }
            }),
            || scan_baseline(root.path(), &mut IdAllocator::new()).unwrap(),
        );

        assert!(baseline.get_by_path(&TreePath::parse("blocked")).is_some());
        assert!(
            baseline
                .get_by_path(&TreePath::parse("blocked/a.txt"))
                .is_none()
        );
        assert!(
            baseline
                .get_by_path(&TreePath::parse("blocked/b.txt"))
                .is_none()
        );
        assert!(
            baseline
                .get_by_path(&TreePath::parse("readable/ok.txt"))
                .is_some()
        );
        assert_eq!(
            baseline.incomplete_dirs().get(&TreePath::parse("blocked")),
            Some(&ScanErrorKind::Other("injected".to_owned()))
        );
        assert_eq!(baseline.scan_warnings()[0].stage, ScanStage::DirEntry);
    }

    #[test]
    fn root_enumeration_failure_remains_fatal() {
        let root = tempdir().unwrap();
        let root_path = root.path().to_path_buf();

        let result = with_fault(
            Box::new(move |stage, path| {
                (stage == "enumerate_dir" && path == root_path).then(|| {
                    io::Error::new(io::ErrorKind::PermissionDenied, "injected root failure")
                })
            }),
            || scan_baseline(root.path(), &mut IdAllocator::new()),
        );

        let error = result.unwrap_err().to_string();
        assert!(error.contains("Failed while enumerating directory"));
    }

    #[test]
    fn io_errors_are_classified_without_exposing_io_types_to_core() {
        assert_eq!(
            classify_io_error(&io::Error::from(io::ErrorKind::PermissionDenied)),
            ScanErrorKind::PermissionDenied
        );
        assert_eq!(
            classify_io_error(&io::Error::from(io::ErrorKind::NotFound)),
            ScanErrorKind::NotFound
        );
        assert_eq!(
            classify_io_error(&io::Error::from(io::ErrorKind::TimedOut)),
            ScanErrorKind::TimedOut
        );
        assert!(matches!(
            classify_io_error(&io::Error::other("other")),
            ScanErrorKind::Other(message) if message == "other"
        ));
    }

    #[test]
    fn metadata_failures_mark_parent_but_not_found_races_are_silent() {
        let root = tempdir().unwrap();
        let failed = root.path().join("failed.txt");
        let raced = root.path().join("raced.txt");
        fs::write(&failed, b"failed").unwrap();
        fs::write(&raced, b"raced").unwrap();
        fs::write(root.path().join("kept.txt"), b"kept").unwrap();
        let failed_for_hook = failed.clone();
        let raced_for_hook = raced.clone();

        let baseline = with_fault(
            Box::new(move |stage, path| {
                if stage != "metadata" {
                    return None;
                }
                if path == failed_for_hook {
                    Some(io::Error::new(io::ErrorKind::PermissionDenied, "injected"))
                } else if path == raced_for_hook {
                    Some(io::Error::new(io::ErrorKind::NotFound, "injected race"))
                } else {
                    None
                }
            }),
            || scan_baseline(root.path(), &mut IdAllocator::new()).unwrap(),
        );

        assert!(
            baseline
                .get_by_path(&TreePath::parse("failed.txt"))
                .is_none()
        );
        assert!(
            baseline
                .get_by_path(&TreePath::parse("raced.txt"))
                .is_none()
        );
        assert!(baseline.get_by_path(&TreePath::parse("kept.txt")).is_some());
        assert_eq!(
            baseline.incomplete_dirs().get(&TreePath::root()),
            Some(&ScanErrorKind::PermissionDenied)
        );
        assert_eq!(baseline.scan_warnings().len(), 1);
        assert_eq!(baseline.scan_warnings()[0].path, failed);
        assert_eq!(baseline.scan_warnings()[0].stage, ScanStage::Metadata);
    }

    #[test]
    fn rescan_preserves_unreadable_known_subtree_and_recovers_ids_and_metadata() {
        let root = tempdir().unwrap();
        let directory = root.path().join("directory");
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("child.txt"), b"child").unwrap();
        let mut ids = IdAllocator::new();
        let previous = scan_baseline(root.path(), &mut ids).unwrap();
        let child = previous
            .get_by_path(&TreePath::parse("directory/child.txt"))
            .unwrap();
        let child_id = child.id;
        let child_meta = previous.meta(child_id).copied();
        let directory_for_hook = directory.clone();

        let degraded = with_fault(
            Box::new(move |stage, path| {
                (stage == "enumerate_dir" && path == directory_for_hook).then(|| {
                    io::Error::new(io::ErrorKind::PermissionDenied, "injected access denied")
                })
            }),
            || rescan_preserving_ids(root.path(), &mut ids, &previous).unwrap(),
        );

        assert_eq!(degraded.entries(), previous.entries());
        assert_eq!(degraded.meta(child_id).copied(), child_meta);
        assert_eq!(
            degraded
                .incomplete_dirs()
                .get(&TreePath::parse("directory")),
            Some(&ScanErrorKind::PermissionDenied)
        );

        let recovered = rescan_preserving_ids(root.path(), &mut ids, &degraded).unwrap();
        assert!(recovered.incomplete_dirs().is_empty());
        assert!(recovered.scan_warnings().is_empty());
        assert_eq!(
            recovered
                .get_by_path(&TreePath::parse("directory/child.txt"))
                .unwrap()
                .id,
            child_id
        );
    }

    #[test]
    fn changed_rescan_carries_unreadable_affected_subtree_without_full_scan() {
        let root = tempdir().unwrap();
        let directory = root.path().join("directory");
        let child = directory.join("child.txt");
        fs::create_dir(&directory).unwrap();
        fs::write(&child, b"child").unwrap();
        let mut ids = IdAllocator::new();
        let complete = scan_baseline(root.path(), &mut ids).unwrap();
        let complete_child_id = complete
            .get_by_path(&TreePath::parse("directory/child.txt"))
            .unwrap()
            .id;
        let directory_for_hook = directory.clone();
        let previous = with_fault(
            Box::new(move |stage, path| {
                (stage == "enumerate_dir" && path == directory_for_hook).then(|| {
                    io::Error::new(io::ErrorKind::PermissionDenied, "injected access denied")
                })
            }),
            || rescan_preserving_ids(root.path(), &mut ids, &complete).unwrap(),
        );
        assert!(
            previous
                .incomplete_dirs()
                .contains_key(&TreePath::parse("directory"))
        );
        let directory_for_hook = directory.clone();
        let enumerated = Rc::new(RefCell::new(Vec::new()));
        let enumerated_for_hook = Rc::clone(&enumerated);
        let changed = BTreeSet::from([child]);

        let degraded = with_fault(
            Box::new(move |stage, path| {
                if stage != "enumerate_dir" {
                    return None;
                }
                enumerated_for_hook.borrow_mut().push(path.to_path_buf());
                (path == directory_for_hook).then(|| {
                    io::Error::new(io::ErrorKind::PermissionDenied, "injected access denied")
                })
            }),
            || {
                rescan_changed_preserving_ids_with(
                    root.path(),
                    &mut ids,
                    &previous,
                    &changed,
                    &ScanOptions::default(),
                )
                .unwrap()
            },
        );

        assert_eq!(
            enumerated.borrow().iter().cloned().collect::<BTreeSet<_>>(),
            BTreeSet::from([directory])
        );
        assert_eq!(degraded.entries(), previous.entries());
        assert_eq!(
            degraded
                .get_by_path(&TreePath::parse("directory/child.txt"))
                .unwrap()
                .id,
            complete_child_id
        );
        assert!(
            degraded
                .incomplete_dirs()
                .contains_key(&TreePath::parse("directory"))
        );
    }

    #[test]
    fn changed_rescan_repopulates_recovered_incomplete_directory_and_preserves_ids() {
        let root = tempdir().unwrap();
        let directory = root.path().join("directory");
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("kept.txt"), b"kept").unwrap();
        let mut ids = IdAllocator::new();
        let complete = scan_baseline(root.path(), &mut ids).unwrap();
        let kept_id = complete
            .get_by_path(&TreePath::parse("directory/kept.txt"))
            .unwrap()
            .id;
        let directory_for_hook = directory.clone();
        let degraded = with_fault(
            Box::new(move |stage, path| {
                (stage == "enumerate_dir" && path == directory_for_hook).then(|| {
                    io::Error::new(io::ErrorKind::PermissionDenied, "injected access denied")
                })
            }),
            || rescan_preserving_ids(root.path(), &mut ids, &complete).unwrap(),
        );
        fs::write(directory.join("recovered.txt"), b"recovered").unwrap();

        let recovered = rescan_changed_preserving_ids_with(
            root.path(),
            &mut ids,
            &degraded,
            &BTreeSet::from([directory.clone()]),
            &ScanOptions::default(),
        )
        .unwrap();

        assert!(recovered.incomplete_dirs().is_empty());
        assert!(recovered.scan_warnings().is_empty());
        assert_eq!(
            recovered
                .get_by_path(&TreePath::parse("directory/kept.txt"))
                .unwrap()
                .id,
            kept_id
        );
        assert!(
            recovered
                .get_by_path(&TreePath::parse("directory/recovered.txt"))
                .is_some()
        );
    }

    #[test]
    fn changed_rescan_with_incomplete_sibling_only_enumerates_affected_directories() {
        let root = tempdir().unwrap();
        let blocked = root.path().join("blocked");
        let unaffected = root.path().join("unaffected");
        fs::create_dir(&blocked).unwrap();
        fs::write(blocked.join("kept.txt"), b"kept").unwrap();
        fs::create_dir(&unaffected).unwrap();
        fs::write(unaffected.join("stable.txt"), b"stable").unwrap();
        fs::write(root.path().join("changed.txt"), b"before").unwrap();
        let blocked_for_hook = blocked.clone();
        let mut ids = IdAllocator::new();
        let previous = with_fault(
            Box::new(move |stage, path| {
                (stage == "enumerate_dir" && path == blocked_for_hook).then(|| {
                    io::Error::new(io::ErrorKind::PermissionDenied, "injected access denied")
                })
            }),
            || scan_baseline(root.path(), &mut ids).unwrap(),
        );
        assert!(
            previous
                .incomplete_dirs()
                .contains_key(&TreePath::parse("blocked"))
        );

        fs::write(root.path().join("changed.txt"), b"after").unwrap();
        let enumerated = Rc::new(RefCell::new(Vec::new()));
        let enumerated_for_hook = Rc::clone(&enumerated);
        let changed = BTreeSet::from([root.path().join("changed.txt")]);
        let rescanned = with_fault(
            Box::new(move |stage, path| {
                if stage == "enumerate_dir" {
                    enumerated_for_hook.borrow_mut().push(path.to_path_buf());
                }
                None
            }),
            || {
                rescan_changed_preserving_ids_with(
                    root.path(),
                    &mut ids,
                    &previous,
                    &changed,
                    &ScanOptions::default(),
                )
                .unwrap()
            },
        );

        assert_eq!(&*enumerated.borrow(), &[root.path().to_path_buf()]);
        assert!(
            rescanned
                .incomplete_dirs()
                .contains_key(&TreePath::parse("blocked"))
        );
        assert_eq!(rescanned.scan_warnings(), previous.scan_warnings());
    }

    #[test]
    fn partial_rescan_matches_full_scan_with_unaffected_incomplete_directory() {
        let root = tempdir().unwrap();
        let blocked = root.path().join("blocked");
        fs::create_dir(&blocked).unwrap();
        fs::write(blocked.join("kept.txt"), b"kept").unwrap();
        fs::write(root.path().join("changed.txt"), b"before").unwrap();
        let mut ids = IdAllocator::new();
        let complete = scan_baseline(root.path(), &mut ids).unwrap();
        let blocked_for_hook = blocked.clone();
        let previous = with_fault(
            Box::new(move |stage, path| {
                (stage == "enumerate_dir" && path == blocked_for_hook).then(|| {
                    io::Error::new(io::ErrorKind::PermissionDenied, "injected access denied")
                })
            }),
            || rescan_preserving_ids(root.path(), &mut ids, &complete).unwrap(),
        );
        let changed = root.path().join("changed.txt");
        fs::write(&changed, b"after").unwrap();
        let changed_paths = BTreeSet::from([changed]);
        let mut partial_ids = allocator_after(&previous);
        let partial = rescan_changed_preserving_ids_with(
            root.path(),
            &mut partial_ids,
            &previous,
            &changed_paths,
            &ScanOptions::default(),
        )
        .unwrap();
        let blocked_for_hook = blocked.clone();
        let mut full_ids = allocator_after(&previous);
        let full = with_fault(
            Box::new(move |stage, path| {
                (stage == "enumerate_dir" && path == blocked_for_hook).then(|| {
                    io::Error::new(io::ErrorKind::PermissionDenied, "injected access denied")
                })
            }),
            || {
                rescan_preserving_ids_with(
                    root.path(),
                    &mut full_ids,
                    &previous,
                    &ScanOptions::default(),
                )
                .unwrap()
            },
        );

        assert_eq!(partial, full);
        assert_eq!(partial.incomplete_dirs(), full.incomplete_dirs());
        assert_eq!(partial.scan_warnings(), full.scan_warnings());
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_name_is_skipped_and_marks_parent_incomplete() {
        use std::os::unix::ffi::OsStringExt;

        let root = tempdir().unwrap();
        fs::write(
            root.path().join(OsString::from_vec(vec![b'x', 0xff])),
            b"bad",
        )
        .unwrap();
        fs::write(root.path().join("good.txt"), b"good").unwrap();

        let baseline = scan_baseline(root.path(), &mut IdAllocator::new()).unwrap();

        assert_eq!(baseline.entries().len(), 1);
        assert_eq!(baseline.entries()[0].path, TreePath::parse("good.txt"));
        assert_eq!(
            baseline.incomplete_dirs().get(&TreePath::root()),
            Some(&ScanErrorKind::NonUnicodeName)
        );
        assert_eq!(baseline.scan_warnings()[0].stage, ScanStage::Name);
        assert_eq!(
            baseline.scan_warnings()[0].kind,
            ScanErrorKind::NonUnicodeName
        );
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_sibling_does_not_hide_readable_subtree() {
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
        fs::write(blocked.join("hidden.txt"), b"hidden").unwrap();
        fs::create_dir_all(root.path().join("readable/deep")).unwrap();
        fs::write(root.path().join("readable/deep/ok.txt"), b"ok").unwrap();
        let original_mode = fs::metadata(&blocked).unwrap().permissions().mode();
        let _restore = RestorePermissions(blocked.clone(), original_mode);
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&blocked).is_ok() {
            return;
        }

        let baseline = scan_baseline(root.path(), &mut IdAllocator::new()).unwrap();

        assert!(
            baseline
                .get_by_path(&TreePath::parse("readable/deep/ok.txt"))
                .is_some()
        );
        assert!(baseline.get_by_path(&TreePath::parse("blocked")).is_some());
        assert!(
            baseline
                .get_by_path(&TreePath::parse("blocked/hidden.txt"))
                .is_none()
        );
        assert_eq!(
            baseline.incomplete_dirs().get(&TreePath::parse("blocked")),
            Some(&ScanErrorKind::PermissionDenied)
        );
    }

    #[cfg(unix)]
    #[test]
    fn deeply_unreadable_subtree_keeps_readable_ancestors() {
        use std::os::unix::fs::PermissionsExt;

        struct RestorePermissions(PathBuf, u32);
        impl Drop for RestorePermissions {
            fn drop(&mut self) {
                let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(self.1));
            }
        }

        let root = tempdir().unwrap();
        let blocked = root.path().join("top/middle/blocked");
        fs::create_dir_all(&blocked).unwrap();
        fs::write(blocked.join("hidden.txt"), b"hidden").unwrap();
        fs::write(root.path().join("top/visible.txt"), b"visible").unwrap();
        let original_mode = fs::metadata(&blocked).unwrap().permissions().mode();
        let _restore = RestorePermissions(blocked.clone(), original_mode);
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&blocked).is_ok() {
            return;
        }

        let baseline = scan_baseline(root.path(), &mut IdAllocator::new()).unwrap();

        for path in ["top", "top/middle", "top/middle/blocked", "top/visible.txt"] {
            assert!(baseline.get_by_path(&TreePath::parse(path)).is_some());
        }
        assert!(
            baseline
                .get_by_path(&TreePath::parse("top/middle/blocked/hidden.txt"))
                .is_none()
        );
        assert!(
            baseline
                .incomplete_dirs()
                .contains_key(&TreePath::parse("top/middle/blocked"))
        );
    }

    #[test]
    fn shallow_scan_enumerates_root_once_and_marks_child_dirs_unloaded() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("a/nested")).unwrap();
        fs::create_dir(root.path().join("b")).unwrap();
        fs::write(root.path().join("a/child.txt"), b"child").unwrap();
        let root_path = root.path().to_path_buf();
        let enumerated = Rc::new(RefCell::new(Vec::new()));
        let observed = Rc::clone(&enumerated);

        let baseline = with_fault(
            Box::new(move |stage, path| {
                if stage == "enumerate_dir" {
                    observed.borrow_mut().push(path.to_path_buf());
                }
                None
            }),
            || {
                scan_baseline_shallow_with(
                    &root_path,
                    &mut IdAllocator::new(),
                    &ScanOptions::default(),
                )
                .unwrap()
            },
        );

        assert_eq!(&*enumerated.borrow(), &[root.path().to_path_buf()]);
        assert_eq!(baseline.entries().len(), 2);
        assert_eq!(
            baseline.unloaded_dirs(),
            &BTreeSet::from([TreePath::parse("a"), TreePath::parse("b")])
        );
    }

    #[test]
    fn cancellable_full_scan_matches_existing_recursive_scan() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("dir/nested")).unwrap();
        fs::write(root.path().join("dir/nested/file.txt"), b"x").unwrap();
        let mut regular_ids = IdAllocator::new();
        let regular = scan_baseline(root.path(), &mut regular_ids).unwrap();
        let mut cancellable_ids = IdAllocator::new();

        let cancellable = scan_baseline_cancellable_with(
            root.path(),
            |_| Ok(cancellable_ids.allocate()),
            &ScanOptions::default(),
            |_| {},
            &AtomicBool::new(false),
        )
        .unwrap()
        .unwrap();

        assert_eq!(cancellable, regular);
    }

    #[test]
    fn shallow_cancellable_scan_stops_at_progress_boundary() {
        let root = tempdir().unwrap();
        for index in 0..1100 {
            fs::write(root.path().join(format!("file-{index:04}.txt")), b"x").unwrap();
        }
        let cancel = AtomicBool::new(false);
        let mut progress = Vec::new();

        let result = scan_baseline_shallow_cancellable_with(
            root.path(),
            |_| Ok(fyler_core::id::EntryId(1)),
            &ScanOptions::default(),
            |count| {
                progress.push(count);
                cancel.store(true, AtomicOrdering::Relaxed);
            },
            &cancel,
        )
        .unwrap();

        assert!(result.is_none());
        assert_eq!(progress, [1000]);
    }

    #[test]
    fn load_directory_preserves_ids_inserts_at_dfs_position_and_propagates_unloaded() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("a/nested")).unwrap();
        fs::write(root.path().join("a/nested/leaf.txt"), b"leaf").unwrap();
        fs::write(root.path().join("a/child.txt"), b"child").unwrap();
        fs::write(root.path().join("z.txt"), b"top").unwrap();
        let mut ids = IdAllocator::new();
        let shallow =
            scan_baseline_shallow_with(root.path(), &mut ids, &ScanOptions::default()).unwrap();
        let a_id = shallow.get_by_path(&TreePath::parse("a")).unwrap().id;
        let z_id = shallow.get_by_path(&TreePath::parse("z.txt")).unwrap().id;

        let loaded = load_directory(
            root.path(),
            &TreePath::parse("a"),
            &mut ids,
            &shallow,
            &ScanOptions::default(),
        )
        .unwrap();

        assert_eq!(loaded.get_by_path(&TreePath::parse("a")).unwrap().id, a_id);
        assert_eq!(
            loaded.get_by_path(&TreePath::parse("z.txt")).unwrap().id,
            z_id
        );
        let full = scan_baseline(root.path(), &mut IdAllocator::new()).unwrap();
        let expected = full
            .entries()
            .iter()
            .filter(|entry| !TreePath::parse("a/nested").is_strict_ancestor_of(&entry.path))
            .map(|entry| (entry.path.clone(), entry.kind))
            .collect::<Vec<_>>();
        let actual = loaded
            .entries()
            .iter()
            .map(|entry| (entry.path.clone(), entry.kind))
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
        assert!(!loaded.is_unloaded(&TreePath::parse("a")));
        assert!(loaded.is_unloaded(&TreePath::parse("a/nested")));
    }

    #[test]
    fn load_directory_failure_keeps_previous_unloaded_for_retry() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("lazy")).unwrap();
        let mut ids = IdAllocator::new();
        let previous =
            scan_baseline_shallow_with(root.path(), &mut ids, &ScanOptions::default()).unwrap();
        let blocked = root.path().join("lazy");

        let result = with_fault(
            Box::new(move |stage, path| {
                (stage == "enumerate_dir" && path == blocked)
                    .then(|| io::Error::new(io::ErrorKind::PermissionDenied, "blocked"))
            }),
            || {
                load_directory(
                    root.path(),
                    &TreePath::parse("lazy"),
                    &mut ids,
                    &previous,
                    &ScanOptions::default(),
                )
            },
        );

        assert!(result.is_err());
        assert!(previous.is_unloaded(&TreePath::parse("lazy")));
        assert!(previous.incomplete_dirs().is_empty());
    }

    #[test]
    fn recursive_load_fully_loads_and_reports_progress() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("lazy/nested")).unwrap();
        for index in 0..1001 {
            fs::write(
                root.path().join(format!("lazy/nested/file-{index:04}.txt")),
                b"x",
            )
            .unwrap();
        }
        let mut ids = IdAllocator::new();
        let shallow =
            scan_baseline_shallow_with(root.path(), &mut ids, &ScanOptions::default()).unwrap();
        let previous = load_directory(
            root.path(),
            &TreePath::parse("lazy"),
            &mut ids,
            &shallow,
            &ScanOptions::default(),
        )
        .unwrap();
        assert!(previous.is_unloaded(&TreePath::parse("lazy/nested")));
        let mut progress = Vec::new();

        let loaded = load_directory_recursive_cancellable(
            root.path(),
            &TreePath::parse("lazy"),
            |_| Ok(ids.allocate()),
            &previous,
            &ScanOptions::default(),
            |count| progress.push(count),
            &AtomicBool::new(false),
        )
        .unwrap()
        .unwrap();

        assert!(loaded.unloaded_dirs().is_empty());
        assert!(
            loaded
                .get_by_path(&TreePath::parse("lazy/nested/file-1000.txt"))
                .is_some()
        );
        assert_eq!(progress, [1000]);
    }

    #[test]
    fn recursive_load_cancellation_returns_none_without_partial_tree() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("lazy")).unwrap();
        for index in 0..1100 {
            fs::write(root.path().join(format!("lazy/file-{index:04}.txt")), b"x").unwrap();
        }
        let mut ids = IdAllocator::new();
        let previous =
            scan_baseline_shallow_with(root.path(), &mut ids, &ScanOptions::default()).unwrap();
        let cancel = AtomicBool::new(false);

        let loaded = load_directory_recursive_cancellable(
            root.path(),
            &TreePath::parse("lazy"),
            |_| Ok(ids.allocate()),
            &previous,
            &ScanOptions::default(),
            |_| cancel.store(true, AtomicOrdering::Relaxed),
            &cancel,
        )
        .unwrap();

        assert!(loaded.is_none());
        assert!(previous.is_unloaded(&TreePath::parse("lazy")));
    }

    #[test]
    fn changed_path_inside_unloaded_subtree_enumerates_nothing() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("lazy/deep")).unwrap();
        fs::write(root.path().join("lazy/deep/file.txt"), b"x").unwrap();
        let mut ids = IdAllocator::new();
        let previous =
            scan_baseline_shallow_with(root.path(), &mut ids, &ScanOptions::default()).unwrap();
        let count = Rc::new(RefCell::new(0usize));
        let observed = Rc::clone(&count);

        let rescanned = with_fault(
            Box::new(move |stage, _| {
                if stage == "enumerate_dir" {
                    *observed.borrow_mut() += 1;
                }
                None
            }),
            || {
                rescan_changed_preserving_ids_with(
                    root.path(),
                    &mut ids,
                    &previous,
                    &BTreeSet::from([root.path().join("lazy/deep/file.txt")]),
                    &ScanOptions::default(),
                )
                .unwrap()
            },
        );

        assert_eq!(*count.borrow(), 0);
        assert_eq!(rescanned, previous);
        assert_eq!(rescanned.unloaded_dirs(), previous.unloaded_dirs());
    }

    #[test]
    fn coverage_rescan_enumerates_loaded_dirs_only_and_carries_unloaded_marks() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("loaded/lazy")).unwrap();
        fs::write(root.path().join("loaded/file.txt"), b"old").unwrap();
        let mut ids = IdAllocator::new();
        let shallow =
            scan_baseline_shallow_with(root.path(), &mut ids, &ScanOptions::default()).unwrap();
        let previous = load_directory(
            root.path(),
            &TreePath::parse("loaded"),
            &mut ids,
            &shallow,
            &ScanOptions::default(),
        )
        .unwrap();
        fs::write(root.path().join("loaded/file.txt"), b"new content").unwrap();
        let enumerated = Rc::new(RefCell::new(Vec::new()));
        let observed = Rc::clone(&enumerated);

        let rescanned = with_fault(
            Box::new(move |stage, path| {
                if stage == "enumerate_dir" {
                    observed.borrow_mut().push(path.to_path_buf());
                }
                None
            }),
            || {
                rescan_changed_preserving_ids_with(
                    root.path(),
                    &mut ids,
                    &previous,
                    &BTreeSet::from([root.path().join("loaded/file.txt")]),
                    &ScanOptions::default(),
                )
                .unwrap()
            },
        );

        assert_eq!(&*enumerated.borrow(), &[root.path().join("loaded")]);
        assert!(rescanned.is_unloaded(&TreePath::parse("loaded/lazy")));
    }

    #[test]
    fn options_rescan_does_not_descend_unloaded_and_marks_new_hidden_dir_unloaded() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("loaded/lazy")).unwrap();
        fs::create_dir(root.path().join(".new-hidden")).unwrap();
        let mut ids = IdAllocator::new();
        let shallow =
            scan_baseline_shallow_with(root.path(), &mut ids, &ScanOptions::default()).unwrap();
        let previous = load_directory(
            root.path(),
            &TreePath::parse("loaded"),
            &mut ids,
            &shallow,
            &ScanOptions::default(),
        )
        .unwrap();
        let enumerated = Rc::new(RefCell::new(Vec::new()));
        let observed = Rc::clone(&enumerated);

        let rescanned = with_fault(
            Box::new(move |stage, path| {
                if stage == "enumerate_dir" {
                    observed.borrow_mut().push(path.to_path_buf());
                }
                None
            }),
            || {
                rescan_preserving_ids_with(
                    root.path(),
                    &mut ids,
                    &previous,
                    &ScanOptions {
                        show_hidden: true,
                        reverse: true,
                        ..ScanOptions::default()
                    },
                )
                .unwrap()
            },
        );

        assert_eq!(enumerated.borrow().len(), 2);
        assert!(
            !enumerated
                .borrow()
                .contains(&root.path().join("loaded/lazy"))
        );
        assert!(rescanned.is_unloaded(&TreePath::parse("loaded/lazy")));
        assert!(rescanned.is_unloaded(&TreePath::parse(".new-hidden")));
    }

    #[test]
    #[ignore = "環境依存性能計測"]
    fn bench_lazy_loaded_range_on_deep_50k_tree() {
        let root = tempdir().unwrap();
        let expanded = root.path().join("expanded");
        fs::create_dir(&expanded).unwrap();
        for index in 0..100 {
            fs::write(expanded.join(format!("file-{index:03}.txt")), b"x").unwrap();
        }
        for branch in 0..50 {
            let branch = root.path().join(format!("branch-{branch:02}"));
            for nested in 0..10 {
                let nested = branch.join(format!("nested-{nested:02}"));
                fs::create_dir_all(&nested).unwrap();
                for file in 0..100 {
                    fs::write(nested.join(format!("file-{file:03}.txt")), b"x").unwrap();
                }
            }
        }

        let options = ScanOptions::default();
        let shallow_started = Instant::now();
        let mut lazy_ids = IdAllocator::new();
        let shallow = scan_baseline_shallow_with(root.path(), &mut lazy_ids, &options).unwrap();
        let shallow_elapsed = shallow_started.elapsed();

        let full_started = Instant::now();
        let full = scan_baseline_with(root.path(), &mut IdAllocator::new(), &options).unwrap();
        let full_elapsed = full_started.elapsed();

        let load_started = Instant::now();
        let loaded = load_directory(
            root.path(),
            &TreePath::parse("expanded"),
            &mut lazy_ids,
            &shallow,
            &options,
        )
        .unwrap();
        let load_elapsed = load_started.elapsed();

        let watch_started = Instant::now();
        let unchanged = rescan_changed_preserving_ids_with(
            root.path(),
            &mut lazy_ids,
            &loaded,
            &BTreeSet::from([root.path().join("branch-00/nested-00/file-000.txt")]),
            &options,
        )
        .unwrap();
        let watch_elapsed = watch_started.elapsed();

        let expected_loaded = shallow.entries().len() + 100;
        assert_eq!(loaded.entries().len(), expected_loaded);
        assert_eq!(unchanged, loaded);
        assert!(full.entries().len() >= 50_000);
        eprintln!(
            "lazy loaded-range bench: full_entries={}, loaded_entries={}, shallow={shallow_elapsed:?}, full={full_elapsed:?}, load_100={load_elapsed:?}, unloaded_watch={watch_elapsed:?}",
            full.entries().len(),
            loaded.entries().len(),
        );
    }
}
