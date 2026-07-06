//! plan確定時の実FS衝突preflight走査(読み取り専用)。
//!
//! baselineに現れない実体(隠しファイル等)への上書きを、apply前に検出して
//! ユーザーへ提示する。applyの`ensure_target_vacant`はTOCTOU最終防衛線として残る。

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use fyler_core::path::TreePath;
use fyler_core::plan::{FsOperation, OperationPlan};

/// preflight走査の結果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreflightConflicts {
    /// 移動先に既存のファイル/シンボリックリンクがある操作の移動先パス。
    /// 承認されればapply時にごみ箱へ退避して上書きする。plan順。
    pub overwritable: Vec<TreePath>,
    /// 移動先に既存のディレクトリがある操作の移動先パス。上書き不可。plan順。
    pub blocked: Vec<TreePath>,
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

fn case_folded_key(path: &TreePath) -> String {
    path.to_string().to_lowercase()
}

#[cfg(test)]
mod tests {
    use fyler_core::id::EntryId;
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
}
