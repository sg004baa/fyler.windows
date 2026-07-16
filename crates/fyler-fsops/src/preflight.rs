//! plan確定時の実FS衝突preflight走査(読み取り専用)。
//!
//! baselineに現れない実体(隠しファイル等)への上書きを、apply前に検出して
//! ユーザーへ提示する。applyの`ensure_target_vacant`はTOCTOU最終防衛線として残る。

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use fyler_core::path::TreePath;
use fyler_core::plan::{FsOperation, OperationPlan};
use fyler_core::transfer::{DropEffect, ImportPlan, TransferKind, TransferPlan};

/// preflight走査の結果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreflightConflicts {
    /// 移動先に既存のファイル/シンボリックリンクがある操作の移動先パス。
    /// 承認されればapply時にごみ箱へ退避して上書きする。plan順。
    pub overwritable: Vec<TreePath>,
    /// 移動先に既存のディレクトリがある操作の移動先パス。上書き不可。plan順。
    pub blocked: Vec<TreePath>,
}

/// pane間transferのpreflight結果。パスはすべて絶対パス。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransferPreflight {
    /// 既存のファイル/シンボリックリンクで、承認後にごみ箱へ退避できる移動先。
    pub overwritable: Vec<PathBuf>,
    /// 実行不能な操作に関係するパス。plan順で、同じパスは1回だけ格納する。
    pub blocked: Vec<PathBuf>,
}

/// planの各操作の移動先を実FSと照合し、衝突を分類して返す。
///
/// 走査は読み取り専用(`fs::symlink_metadata` のみ。プレースホルダのhydrationを
/// 誘発しない)。planを順にシミュレートし、先行するDelete/Moveで空く移動先は
/// 衝突としない。
pub fn scan_plan_conflicts(root: &Path, plan: &OperationPlan) -> PreflightConflicts {
    let mut conflicts = PreflightConflicts::default();
    let mut vacated = HashSet::new();

    for operation in &plan.ops {
        let target = match operation {
            FsOperation::Delete { path, .. } => {
                vacated.insert(case_folded_key(path));
                continue;
            }
            FsOperation::Move { from, to, .. } => {
                vacated.insert(case_folded_key(from));
                if crate::apply::is_case_only_rename(from, to) {
                    continue;
                }
                to
            }
            FsOperation::Create { path, .. } => path,
            FsOperation::Copy { to, .. } => to,
        };

        // Windowsのcase-insensitive解決では、削除予定の`Foo.txt`が
        // `symlink_metadata("foo.txt")`にも一致する。Unicode小文字化した
        // ルート相対パスで先行操作の空きを追跡して、この誤検出を避ける。
        // Linuxでは過剰に寛容だが、preflightは助言であり、apply直前の
        // ensure_target_vacantがTOCTOU最終防衛線になるため許容する。
        if vacated.contains(&case_folded_key(target)) {
            continue;
        }

        let target_path = crate::long_path::to_fs(&target.to_fs_path(root));
        match fs::symlink_metadata(&target_path) {
            Ok(metadata)
                if crate::scan::kind_from_metadata(&metadata)
                    == fyler_core::tree::EntryKind::Dir =>
            {
                conflicts.blocked.push(target.clone());
            }
            Ok(_) => conflicts.overwritable.push(target.clone()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            // 戻り値にI/Oエラーを表す経路はないため、判定不能な場合はapply直前の
            // ensure_target_vacantへ委ねる。ここでは上書き承認を付与しない。
            Err(_) => {}
        }
    }

    conflicts
}

/// pane間transferを絶対パスへ展開し、実FSとplan内の干渉を検査する。
///
/// 読み取り専用であり、実体確認は`fs::symlink_metadata`だけを使って
/// OneDriveプレースホルダのhydrationを誘発しない。先行Moveで空くパスは
/// 対象ディレクトリのcase sensitivityを反映した絶対パスキーで追跡する。
pub fn preflight_transfer(plan: &TransferPlan) -> TransferPreflight {
    let from_root = absolute_path(&plan.from_root);
    let to_root = absolute_path(&plan.to_root);
    let paths = plan
        .ops
        .iter()
        .map(|op| (op.from.to_fs_path(&from_root), op.to.to_fs_path(&to_root)))
        .collect::<Vec<_>>();
    let mut result = TransferPreflight::default();
    let mut vacated = HashSet::new();

    for (index, op) in plan.ops.iter().enumerate() {
        let (source, target) = &paths[index];

        match fs::symlink_metadata(crate::long_path::to_fs(source)) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                push_unique(&mut result.blocked, source.clone());
            }
            Err(_) => push_unique(&mut result.blocked, source.clone()),
        }

        let case_only = is_case_only_path_change(source, target)
            && !target.parent().is_some_and(directory_is_case_sensitive);
        let same_path = paths_equal(source, target) && !case_only;
        if same_path {
            push_unique(&mut result.blocked, target.clone());
        }
        if op.entry_kind == fyler_core::tree::EntryKind::Dir && is_strict_ancestor(source, target) {
            push_unique(&mut result.blocked, target.clone());
        }
        if op.kind == TransferKind::Move
            && (paths_equal(source, &to_root) || is_strict_ancestor(source, &to_root))
        {
            push_unique(&mut result.blocked, source.clone());
        }

        if op.kind == TransferKind::Move {
            vacated.insert(absolute_case_key(source));
        }

        if same_path || case_only || vacated.contains(&absolute_case_key(target)) {
            continue;
        }

        match fs::symlink_metadata(crate::long_path::to_fs(target)) {
            Ok(metadata)
                if crate::scan::kind_from_metadata(&metadata)
                    == fyler_core::tree::EntryKind::Dir =>
            {
                push_unique(&mut result.blocked, target.clone());
            }
            Ok(_) => push_unique(&mut result.overwritable, target.clone()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            // 判定不能な対象は承認可能にせず、破壊を避ける。
            Err(_) => push_unique(&mut result.blocked, target.clone()),
        }
    }

    // v1はop間依存を持たない。from/toの同一・祖先・子孫関係は、実行順に
    // よって意味が変わるため、関係する両操作をblockedにする。
    for left in 0..paths.len() {
        for right in left + 1..paths.len() {
            let (left_from, left_to) = &paths[left];
            let (right_from, right_to) = &paths[right];
            let interferes = [left_from, left_to].into_iter().any(|left_path| {
                [right_from, right_to]
                    .into_iter()
                    .any(|right_path| paths_overlap(left_path, right_path))
            });
            if interferes {
                push_unique(&mut result.blocked, left_to.clone());
                push_unique(&mut result.blocked, right_to.clone());
            }
        }
    }

    result
}

/// 外部source(clipboard・inbound drop)取り込みのpreflight結果。パスはすべて絶対パス。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportPreflight {
    /// 既存のファイル/シンボリックリンクで、承認後にごみ箱へ退避できる移動先。
    pub overwritable: Vec<PathBuf>,
    /// 実行不能な操作に関係するパス(source消滅・自己子孫・dirへの上書き・
    /// op間干渉等)。plan順で、同じパスは1回だけ格納する。
    pub blocked: Vec<PathBuf>,
}

/// [`ImportPlan`]を実FSと照合し、衝突・自己子孫・source消滅・op間干渉を検査する。
///
/// 読み取り専用(`fs::symlink_metadata`だけを使い、OneDriveプレースホルダの
/// hydrationを誘発しない)。[`preflight_transfer`]と同じ意味論: 先行するMoveで
/// 空くパスは、対象ディレクトリのcase sensitivityを反映した絶対パスキーで追跡する。
pub fn preflight_import(plan: &ImportPlan) -> ImportPreflight {
    let mut result = ImportPreflight::default();
    let mut vacated = HashSet::new();

    for op in &plan.ops {
        let source = absolute_path(&op.source);
        let target = absolute_path(&op.target);

        let source_kind = match fs::symlink_metadata(crate::long_path::to_fs(&source)) {
            Ok(metadata) => Some(crate::scan::kind_from_metadata(&metadata)),
            Err(_) => {
                push_unique(&mut result.blocked, source.clone());
                None
            }
        };

        let case_only = is_case_only_path_change(&source, &target)
            && !target.parent().is_some_and(directory_is_case_sensitive);
        let same_path = paths_equal(&source, &target) && !case_only;
        if same_path {
            push_unique(&mut result.blocked, target.clone());
        }
        if source_kind == Some(fyler_core::tree::EntryKind::Dir)
            && is_strict_ancestor(&source, &target)
        {
            push_unique(&mut result.blocked, target.clone());
        }
        if plan.effect == DropEffect::Move {
            vacated.insert(absolute_case_key(&source));
        }

        if same_path || case_only || vacated.contains(&absolute_case_key(&target)) {
            continue;
        }

        match fs::symlink_metadata(crate::long_path::to_fs(&target)) {
            Ok(metadata)
                if crate::scan::kind_from_metadata(&metadata)
                    == fyler_core::tree::EntryKind::Dir =>
            {
                push_unique(&mut result.blocked, target.clone());
            }
            Ok(_) => push_unique(&mut result.overwritable, target.clone()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            // 判定不能な対象は承認可能にせず、破壊を避ける。
            Err(_) => push_unique(&mut result.blocked, target.clone()),
        }
    }

    // v1はop間依存を持たない。from/toの同一・祖先・子孫関係は、実行順に
    // よって意味が変わるため、関係する両操作をblockedにする。
    for left in 0..plan.ops.len() {
        for right in left + 1..plan.ops.len() {
            let left_from = absolute_path(&plan.ops[left].source);
            let left_to = absolute_path(&plan.ops[left].target);
            let right_from = absolute_path(&plan.ops[right].source);
            let right_to = absolute_path(&plan.ops[right].target);
            let interferes = [&left_from, &left_to].into_iter().any(|left_path| {
                [&right_from, &right_to]
                    .into_iter()
                    .any(|right_path| paths_overlap(left_path, right_path))
            });
            if interferes {
                push_unique(&mut result.blocked, left_to.clone());
                push_unique(&mut result.blocked, right_to.clone());
            }
        }
    }

    result
}

fn absolute_path(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|existing| paths_equal(existing, &path)) {
        paths.push(path);
    }
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    paths_equal(left, right) || is_strict_ancestor(left, right) || is_strict_ancestor(right, left)
}

fn is_strict_ancestor(ancestor: &Path, descendant: &Path) -> bool {
    ancestor.components().count() < descendant.components().count()
        && descendant
            .ancestors()
            .skip(1)
            .any(|candidate| paths_equal(ancestor, candidate))
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    absolute_case_key(left) == absolute_case_key(right)
}

fn absolute_case_key(path: &Path) -> String {
    let text = normalized_path_text(path);
    if path.parent().is_some_and(directory_is_case_sensitive) {
        text
    } else {
        text.to_lowercase()
    }
}

fn normalized_path_text(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("\0")
}

fn directory_is_case_sensitive(directory: &Path) -> bool {
    nearest_existing_directory(directory)
        .and_then(|path| crate::case::dir_is_case_sensitive(&path).ok())
        .unwrap_or(false)
}

fn nearest_existing_directory(start: &Path) -> Option<PathBuf> {
    start.ancestors().find_map(|candidate| {
        fs::symlink_metadata(crate::long_path::to_fs(candidate))
            .ok()
            .filter(|metadata| {
                crate::scan::kind_from_metadata(metadata) == fyler_core::tree::EntryKind::Dir
            })
            .map(|_| candidate.to_path_buf())
    })
}

fn is_case_only_path_change(from: &Path, to: &Path) -> bool {
    from.parent()
        .zip(to.parent())
        .is_some_and(|(left, right)| paths_equal(left, right))
        && from.file_name() != to.file_name()
        && from
            .file_name()
            .zip(to.file_name())
            .is_some_and(|(left, right)| {
                left.to_string_lossy().to_lowercase() == right.to_string_lossy().to_lowercase()
            })
}

fn case_folded_key(path: &TreePath) -> String {
    path.to_string().to_lowercase()
}

#[cfg(test)]
mod tests {
    use fyler_core::id::EntryId;
    use fyler_core::transfer::{ImportOp, TransferKind, TransferOp, TransferPlan};
    use fyler_core::tree::EntryKind;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn classifies_existing_file_as_overwritable() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("hidden.txt"), b"existing").unwrap();
        let plan = OperationPlan {
            ops: vec![FsOperation::Create {
                path: TreePath::parse("hidden.txt"),
                kind: EntryKind::File,
            }],
        };

        let conflicts = scan_plan_conflicts(root.path(), &plan);

        assert_eq!(conflicts.overwritable, [TreePath::parse("hidden.txt")]);
        assert!(conflicts.blocked.is_empty());
    }

    #[test]
    fn classifies_existing_directory_as_blocked() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("a"), b"source").unwrap();
        fs::create_dir(root.path().join("d")).unwrap();
        let plan = OperationPlan {
            ops: vec![FsOperation::Move {
                id: EntryId(1),
                from: TreePath::parse("a"),
                to: TreePath::parse("d"),
            }],
        };

        let conflicts = scan_plan_conflicts(root.path(), &plan);

        assert!(conflicts.overwritable.is_empty());
        assert_eq!(conflicts.blocked, [TreePath::parse("d")]);
    }

    #[cfg(unix)]
    #[test]
    fn classifies_directory_symlink_as_overwritable() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("directory")).unwrap();
        std::os::unix::fs::symlink("directory", root.path().join("link")).unwrap();
        let plan = OperationPlan {
            ops: vec![FsOperation::Create {
                path: TreePath::parse("link"),
                kind: EntryKind::File,
            }],
        };

        let conflicts = scan_plan_conflicts(root.path(), &plan);

        assert_eq!(conflicts.overwritable, [TreePath::parse("link")]);
        assert!(conflicts.blocked.is_empty());
    }

    #[test]
    fn ignores_target_vacated_by_earlier_delete() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("a.txt"), b"source").unwrap();
        fs::write(root.path().join("b.txt"), b"existing").unwrap();
        let plan = OperationPlan {
            ops: vec![
                FsOperation::Delete {
                    id: EntryId(2),
                    path: TreePath::parse("b.txt"),
                },
                FsOperation::Move {
                    id: EntryId(1),
                    from: TreePath::parse("a.txt"),
                    to: TreePath::parse("b.txt"),
                },
            ],
        };

        assert_eq!(
            scan_plan_conflicts(root.path(), &plan),
            PreflightConflicts::default()
        );
    }

    #[test]
    fn ignores_case_only_rename() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("Foo.txt"), b"source").unwrap();
        let plan = OperationPlan {
            ops: vec![FsOperation::Move {
                id: EntryId(1),
                from: TreePath::parse("Foo.txt"),
                to: TreePath::parse("foo.txt"),
            }],
        };

        assert_eq!(
            scan_plan_conflicts(root.path(), &plan),
            PreflightConflicts::default()
        );
    }

    #[test]
    fn reports_no_conflict_for_vacant_target() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("a.txt"), b"source").unwrap();
        let plan = OperationPlan {
            ops: vec![FsOperation::Move {
                id: EntryId(1),
                from: TreePath::parse("a.txt"),
                to: TreePath::parse("missing.txt"),
            }],
        };

        assert_eq!(
            scan_plan_conflicts(root.path(), &plan),
            PreflightConflicts::default()
        );
    }

    fn transfer_plan(from_root: &Path, to_root: &Path, ops: Vec<TransferOp>) -> TransferPlan {
        TransferPlan {
            from_root: from_root.to_path_buf(),
            to_root: to_root.to_path_buf(),
            ops,
        }
    }

    fn transfer_op(kind: TransferKind, from: &str, to: &str, entry_kind: EntryKind) -> TransferOp {
        TransferOp {
            kind,
            from: TreePath::parse(from),
            to: TreePath::parse(to),
            entry_kind,
        }
    }

    #[test]
    fn transfer_classifies_file_and_symlink_as_overwritable_and_directory_as_blocked() {
        let root = tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(source.join("a.txt"), b"a").unwrap();
        fs::write(source.join("b.txt"), b"b").unwrap();
        fs::write(source.join("c.txt"), b"c").unwrap();
        fs::write(target.join("file.txt"), b"existing").unwrap();
        fs::create_dir(target.join("directory")).unwrap();

        #[cfg(unix)]
        std::os::unix::fs::symlink("file.txt", target.join("link")).unwrap();
        #[cfg(not(unix))]
        fs::write(target.join("link"), b"link stand-in").unwrap();

        let plan = transfer_plan(
            &source,
            &target,
            vec![
                transfer_op(TransferKind::Copy, "a.txt", "file.txt", EntryKind::File),
                transfer_op(TransferKind::Copy, "b.txt", "link", EntryKind::File),
                transfer_op(TransferKind::Copy, "c.txt", "directory", EntryKind::File),
            ],
        );

        let result = preflight_transfer(&plan);

        assert_eq!(
            result.overwritable,
            [
                absolute_path(&target.join("file.txt")),
                absolute_path(&target.join("link"))
            ]
        );
        assert_eq!(result.blocked, [absolute_path(&target.join("directory"))]);
    }

    #[test]
    fn transfer_preflight_detects_collision_inside_unloaded_destination_directory() {
        let root = tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::create_dir(&source).unwrap();
        fs::create_dir_all(target.join("lazy")).unwrap();
        fs::write(source.join("a.txt"), b"source").unwrap();
        fs::write(target.join("lazy/a.txt"), b"existing").unwrap();
        let mut ids = fyler_core::id::IdAllocator::new();
        let baseline = crate::scan::scan_baseline_shallow_with(
            &target,
            &mut ids,
            &crate::scan::ScanOptions::default(),
        )
        .unwrap();
        assert!(baseline.is_unloaded(&TreePath::parse("lazy")));
        assert!(
            baseline
                .get_by_path(&TreePath::parse("lazy/a.txt"))
                .is_none()
        );
        let plan = transfer_plan(
            &source,
            &target,
            vec![transfer_op(
                TransferKind::Copy,
                "a.txt",
                "lazy/a.txt",
                EntryKind::File,
            )],
        );

        let result = preflight_transfer(&plan);

        assert_eq!(
            result.overwritable,
            [absolute_path(&target.join("lazy/a.txt"))]
        );
        assert!(result.blocked.is_empty());
    }

    #[test]
    fn transfer_blocks_missing_source() {
        let root = tempdir().unwrap();
        let plan = transfer_plan(
            root.path(),
            root.path(),
            vec![transfer_op(
                TransferKind::Copy,
                "missing.txt",
                "new.txt",
                EntryKind::File,
            )],
        );

        let result = preflight_transfer(&plan);

        assert_eq!(
            result.blocked,
            [absolute_path(&root.path().join("missing.txt"))]
        );
    }

    #[test]
    fn transfer_blocks_directory_move_and_copy_into_own_descendant() {
        for kind in [TransferKind::Move, TransferKind::Copy] {
            let root = tempdir().unwrap();
            fs::create_dir(root.path().join("directory")).unwrap();
            let target = root.path().join("directory/child/copy");
            let plan = transfer_plan(
                root.path(),
                root.path(),
                vec![transfer_op(
                    kind,
                    "directory",
                    "directory/child/copy",
                    EntryKind::Dir,
                )],
            );

            assert!(
                preflight_transfer(&plan)
                    .blocked
                    .contains(&absolute_path(&target))
            );
        }
    }

    #[test]
    fn transfer_blocks_move_that_contains_target_root() {
        let outer = tempdir().unwrap();
        let source = outer.path().join("source");
        let target_root = source.join("target-pane");
        fs::create_dir_all(&target_root).unwrap();
        let plan = transfer_plan(
            outer.path(),
            &target_root,
            vec![transfer_op(
                TransferKind::Move,
                "source",
                "moved",
                EntryKind::Dir,
            )],
        );

        let result = preflight_transfer(&plan);

        assert!(result.blocked.contains(&absolute_path(&source)));
    }

    #[test]
    fn transfer_vacated_path_is_not_reported_as_overwritable() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("a.txt"), b"a").unwrap();
        fs::write(root.path().join("b.txt"), b"b").unwrap();
        let plan = transfer_plan(
            root.path(),
            root.path(),
            vec![
                transfer_op(TransferKind::Move, "b.txt", "c.txt", EntryKind::File),
                transfer_op(TransferKind::Move, "a.txt", "b.txt", EntryKind::File),
            ],
        );

        assert!(preflight_transfer(&plan).overwritable.is_empty());
    }

    #[test]
    fn transfer_blocks_interference_between_flat_operations() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("first")).unwrap();
        fs::write(root.path().join("first/file.txt"), b"first").unwrap();
        fs::write(root.path().join("second.txt"), b"second").unwrap();
        let plan = transfer_plan(
            root.path(),
            root.path(),
            vec![
                transfer_op(
                    TransferKind::Move,
                    "second.txt",
                    "first/new.txt",
                    EntryKind::File,
                ),
                transfer_op(TransferKind::Copy, "first", "copied", EntryKind::Dir),
            ],
        );

        let result = preflight_transfer(&plan);

        assert!(
            result
                .blocked
                .contains(&absolute_path(&root.path().join("first/new.txt")))
        );
        assert!(
            result
                .blocked
                .contains(&absolute_path(&root.path().join("copied")))
        );
    }

    fn import_op(source: &Path, target: &Path) -> ImportOp {
        ImportOp {
            source: source.to_path_buf(),
            target: target.to_path_buf(),
        }
    }

    #[test]
    fn import_classifies_file_and_symlink_as_overwritable_and_directory_as_blocked() {
        let source_root = tempdir().unwrap();
        let dest_root = tempdir().unwrap();
        fs::write(source_root.path().join("a.txt"), b"a").unwrap();
        fs::write(source_root.path().join("b.txt"), b"b").unwrap();
        fs::write(source_root.path().join("c.txt"), b"c").unwrap();
        fs::write(dest_root.path().join("file.txt"), b"existing").unwrap();
        fs::create_dir(dest_root.path().join("directory")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("file.txt", dest_root.path().join("link")).unwrap();
        #[cfg(not(unix))]
        fs::write(dest_root.path().join("link"), b"link stand-in").unwrap();

        let plan = ImportPlan {
            destination: dest_root.path().to_path_buf(),
            effect: DropEffect::Copy,
            ops: vec![
                import_op(
                    &source_root.path().join("a.txt"),
                    &dest_root.path().join("file.txt"),
                ),
                import_op(
                    &source_root.path().join("b.txt"),
                    &dest_root.path().join("link"),
                ),
                import_op(
                    &source_root.path().join("c.txt"),
                    &dest_root.path().join("directory"),
                ),
            ],
        };

        let result = preflight_import(&plan);

        assert_eq!(
            result.overwritable,
            [
                absolute_path(&dest_root.path().join("file.txt")),
                absolute_path(&dest_root.path().join("link")),
            ]
        );
        assert_eq!(
            result.blocked,
            [absolute_path(&dest_root.path().join("directory"))]
        );
    }

    #[test]
    fn import_blocks_missing_source() {
        let source_root = tempdir().unwrap();
        let dest_root = tempdir().unwrap();
        let plan = ImportPlan {
            destination: dest_root.path().to_path_buf(),
            effect: DropEffect::Copy,
            ops: vec![import_op(
                &source_root.path().join("missing.txt"),
                &dest_root.path().join("missing.txt"),
            )],
        };

        let result = preflight_import(&plan);

        assert_eq!(
            result.blocked,
            [absolute_path(&source_root.path().join("missing.txt"))]
        );
    }

    #[test]
    fn import_blocks_directory_move_and_copy_into_own_descendant() {
        for effect in [DropEffect::Move, DropEffect::Copy] {
            let root = tempdir().unwrap();
            fs::create_dir(root.path().join("directory")).unwrap();
            let destination = root.path().join("directory/child");
            fs::create_dir_all(&destination).unwrap();
            let target = destination.join("directory");
            let plan = ImportPlan {
                destination: destination.clone(),
                effect,
                ops: vec![import_op(&root.path().join("directory"), &target)],
            };

            assert!(
                preflight_import(&plan)
                    .blocked
                    .contains(&absolute_path(&target))
            );
        }
    }

    #[test]
    fn import_vacated_path_is_not_reported_as_overwritable() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("a.txt"), b"a").unwrap();
        fs::write(root.path().join("b.txt"), b"b").unwrap();
        let plan = ImportPlan {
            destination: root.path().to_path_buf(),
            effect: DropEffect::Move,
            ops: vec![
                import_op(&root.path().join("b.txt"), &root.path().join("c.txt")),
                import_op(&root.path().join("a.txt"), &root.path().join("b.txt")),
            ],
        };

        assert!(preflight_import(&plan).overwritable.is_empty());
    }

    #[test]
    fn import_blocks_interference_between_flat_operations() {
        let root = tempdir().unwrap();
        let source_root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("first")).unwrap();
        fs::write(source_root.path().join("second.txt"), b"second").unwrap();
        fs::write(source_root.path().join("first-source"), b"source").unwrap();
        let plan = ImportPlan {
            destination: root.path().to_path_buf(),
            effect: DropEffect::Move,
            ops: vec![
                import_op(
                    &source_root.path().join("second.txt"),
                    &root.path().join("first/new.txt"),
                ),
                import_op(
                    &source_root.path().join("first-source"),
                    &root.path().join("first"),
                ),
            ],
        };

        let result = preflight_import(&plan);

        assert!(
            result
                .blocked
                .contains(&absolute_path(&root.path().join("first/new.txt")))
        );
        assert!(
            result
                .blocked
                .contains(&absolute_path(&root.path().join("first")))
        );
    }
}
