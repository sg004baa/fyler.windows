//! apply: 承認済みOperationPlanの実行。**実FS書き込みの入口はここだけ**(絶対ルール1)。

use std::fs;
use std::path::Path;

use anyhow::{Context, bail};
use fyler_core::path::TreePath;
use fyler_core::plan::{FsOperation, OperationPlan};
use fyler_core::report::{CommitReport, OpOutcome, OpResult};
use fyler_core::tree::EntryKind;

/// planを実行し、操作単位の結果を返す。
///
/// 呼び出し契約: 保存状態機械(`fyler_core::save`)の `Applying` 状態
/// (= 確認ダイアログで承認済み)からのみ呼ぶこと。M2まではdry-runのみで、
/// この関数は呼ばれない。
///
/// 実装契約:
/// - `plan.ops` を**並べ替えずに**上から順に実行する(順序はdiff層が保証済み)
/// - `CommitReport.results` はopsと同順・同数で返す
/// - エラーは操作単位で報告する。**全体ロールバックはしない**。部分成功を明示する
///   (ロック中ファイル等。DESIGN.md「その他の対応事項」)
/// - 先行操作の失敗で実行不能になった操作は `OpOutcome::Skipped`
///   (例: 親ディレクトリのCreate失敗 → 子のCreate)
/// - M3のMoveは同一ボリュームrenameとして扱う。3分類とクロスボリューム対応はM4
/// - Deleteは必ず [`crate::recycle`] 経由(ごみ箱)。直接削除しない
/// - case-onlyリネームは [`crate::case`] のtemp名経由2段renameを使う
/// - パスは `TreePath::to_fs_path(root)` → 必要時のみ [`crate::long_path`] で変換
pub fn apply_plan(root: &Path, plan: &OperationPlan) -> CommitReport {
    let mut results = Vec::with_capacity(plan.ops.len());
    let mut failed_directories = Vec::new();

    for operation in &plan.ops {
        if let Some(failed_parent) = failed_parent(operation, &failed_directories) {
            results.push(OpResult {
                op: operation.clone(),
                outcome: OpOutcome::Skipped {
                    reason: format!("先行する親ディレクトリの作成に失敗しました: {failed_parent}"),
                },
            });
            continue;
        }

        let outcome = match execute_operation(root, operation) {
            Ok(()) => OpOutcome::Success,
            Err(error) => {
                if let FsOperation::Create {
                    path,
                    kind: EntryKind::Dir,
                } = operation
                {
                    failed_directories.push(path.clone());
                }
                OpOutcome::Failed {
                    error: error.to_string(),
                    progress: None,
                }
            }
        };
        results.push(OpResult {
            op: operation.clone(),
            outcome,
        });
    }

    CommitReport { results }
}

fn execute_operation(root: &Path, operation: &FsOperation) -> anyhow::Result<()> {
    match operation {
        FsOperation::Create { path, kind } => {
            let target = path.to_fs_path(root);
            match kind {
                EntryKind::Dir => fs::create_dir(&target)
                    .with_context(|| format!("ディレクトリを作成できません: {}", target.display())),
                EntryKind::File => fs::File::create(&target)
                    .map(|_| ())
                    .with_context(|| format!("ファイルを作成できません: {}", target.display())),
                EntryKind::Symlink => bail!("SYMLINKのCREATEはM3で未実装"),
            }
        }
        FsOperation::Move { from, to, .. } => {
            let source = from.to_fs_path(root);
            let target = to.to_fs_path(root);
            if is_case_only_rename(from, to) {
                crate::case::case_only_rename(&source, &target)
            } else {
                fs::rename(&source, &target).with_context(|| {
                    format!(
                        "renameできません: {} → {}",
                        source.display(),
                        target.display()
                    )
                })
            }
        }
        FsOperation::Copy { .. } => bail!("COPYはM4で実装"),
        FsOperation::Delete { path, .. } => {
            crate::recycle::delete_to_recycle_bin(&path.to_fs_path(root))
        }
    }
}

fn is_case_only_rename(from: &TreePath, to: &TreePath) -> bool {
    let (Some(from_name), Some(to_name)) = (from.name(), to.name()) else {
        return false;
    };
    from.parent() == to.parent()
        && from_name.as_bytes() != to_name.as_bytes()
        && from_name.eq_ignore_ascii_case(to_name)
}

fn failed_parent<'a>(
    operation: &FsOperation,
    failed_directories: &'a [TreePath],
) -> Option<&'a TreePath> {
    let target = match operation {
        FsOperation::Create { path, .. } | FsOperation::Delete { path, .. } => path,
        FsOperation::Move { to, .. } | FsOperation::Copy { to, .. } => to,
    };
    failed_directories
        .iter()
        .find(|parent| parent.is_strict_ancestor_of(target))
}

#[cfg(test)]
mod tests {
    use fyler_core::id::EntryId;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn creates_directories_and_files_in_plan_order() {
        let root = tempdir().unwrap();
        let plan = OperationPlan {
            ops: vec![
                FsOperation::Create {
                    path: TreePath::parse("parent"),
                    kind: EntryKind::Dir,
                },
                FsOperation::Create {
                    path: TreePath::parse("parent/child.txt"),
                    kind: EntryKind::File,
                },
            ],
        };

        let report = apply_plan(root.path(), &plan);

        assert!(report.all_succeeded());
        assert!(root.path().join("parent/child.txt").is_file());
    }

    #[test]
    fn renames_entries_and_continues_with_later_operations() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("a.txt"), b"content").unwrap();
        let plan = OperationPlan {
            ops: vec![
                FsOperation::Move {
                    id: EntryId(1),
                    from: TreePath::parse("a.txt"),
                    to: TreePath::parse("b.txt"),
                },
                FsOperation::Create {
                    path: TreePath::parse("c.txt"),
                    kind: EntryKind::File,
                },
            ],
        };

        let report = apply_plan(root.path(), &plan);

        assert!(report.all_succeeded());
        assert!(!root.path().join("a.txt").exists());
        assert_eq!(fs::read(root.path().join("b.txt")).unwrap(), b"content");
        assert!(root.path().join("c.txt").is_file());
        assert_eq!(report.results.len(), plan.ops.len());
    }

    #[test]
    fn supports_case_only_rename() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("Name.txt"), b"content").unwrap();
        let plan = OperationPlan {
            ops: vec![FsOperation::Move {
                id: EntryId(1),
                from: TreePath::parse("Name.txt"),
                to: TreePath::parse("name.txt"),
            }],
        };

        let report = apply_plan(root.path(), &plan);

        assert!(report.all_succeeded());
        assert!(!root.path().join("Name.txt").exists());
        assert_eq!(fs::read(root.path().join("name.txt")).unwrap(), b"content");
    }

    #[test]
    fn skips_descendants_after_parent_directory_create_failure() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("parent"), b"conflict").unwrap();
        let plan = OperationPlan {
            ops: vec![
                FsOperation::Create {
                    path: TreePath::parse("parent"),
                    kind: EntryKind::Dir,
                },
                FsOperation::Create {
                    path: TreePath::parse("parent/child.txt"),
                    kind: EntryKind::File,
                },
                FsOperation::Create {
                    path: TreePath::parse("sibling.txt"),
                    kind: EntryKind::File,
                },
            ],
        };

        let report = apply_plan(root.path(), &plan);

        assert!(matches!(
            report.results[0].outcome,
            OpOutcome::Failed { .. }
        ));
        assert!(matches!(
            report.results[1].outcome,
            OpOutcome::Skipped { .. }
        ));
        assert!(matches!(report.results[2].outcome, OpOutcome::Success));
        assert!(!root.path().join("parent/child.txt").exists());
        assert!(root.path().join("sibling.txt").is_file());
    }

    #[test]
    fn copy_is_reported_as_unimplemented_failure() {
        let root = tempdir().unwrap();
        let plan = OperationPlan {
            ops: vec![FsOperation::Copy {
                src: EntryId(1),
                from: TreePath::parse("a.txt"),
                to: TreePath::parse("b.txt"),
            }],
        };

        let report = apply_plan(root.path(), &plan);

        assert_eq!(
            report.results[0].outcome,
            OpOutcome::Failed {
                error: "COPYはM4で実装".to_owned(),
                progress: None,
            }
        );
    }
}
