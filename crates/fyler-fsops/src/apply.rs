//! apply: 承認済みOperationPlanの実行。**実FS書き込みの入口はここだけ**(絶対ルール1)。

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use fyler_core::path::TreePath;
use fyler_core::plan::{FsOperation, OperationPlan};
use fyler_core::report::{CommitReport, OpOutcome, OpResult};
use fyler_core::tree::EntryKind;

use crate::classify::MoveClass;

#[derive(Debug)]
struct OpFailure {
    error: String,
    progress: Option<String>,
}

impl OpFailure {
    fn with_progress(error: anyhow::Error, progress: impl Into<String>) -> Self {
        Self {
            error: error.to_string(),
            progress: Some(progress.into()),
        }
    }
}

impl From<anyhow::Error> for OpFailure {
    fn from(error: anyhow::Error) -> Self {
        Self {
            error: error.to_string(),
            progress: None,
        }
    }
}

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
/// - Moveはボリュームを3分類し、別ボリュームではcopy + sourceの直接削除を行う
/// - Copyはsourceを残し、ディレクトリはsymlinkへ潜らず再帰コピーする
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
            Err(failure) => {
                if let FsOperation::Create {
                    path,
                    kind: EntryKind::Dir,
                } = operation
                {
                    failed_directories.push(path.clone());
                }
                OpOutcome::Failed {
                    error: failure.error,
                    progress: failure.progress,
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

fn execute_operation(root: &Path, operation: &FsOperation) -> Result<(), OpFailure> {
    match operation {
        FsOperation::Create { path, kind } => {
            let target = path.to_fs_path(root);
            match kind {
                EntryKind::Dir => fs::create_dir(&target)
                    .with_context(|| format!("ディレクトリを作成できません: {}", target.display()))
                    .map_err(OpFailure::from),
                EntryKind::File => fs::File::create(&target)
                    .map(|_| ())
                    .with_context(|| format!("ファイルを作成できません: {}", target.display()))
                    .map_err(OpFailure::from),
                EntryKind::Symlink => Err(OpFailure::from(anyhow!("SYMLINKのCREATEは未実装"))),
            }
        }
        FsOperation::Move { from, to, .. } => {
            let source = from.to_fs_path(root);
            let target = to.to_fs_path(root);
            let metadata = fs::symlink_metadata(&source)
                .with_context(|| format!("移動元のmetadataを取得できません: {}", source.display()))
                .map_err(OpFailure::from)?;
            let kind = crate::scan::kind_from_metadata(&metadata);

            if is_case_only_rename(from, to) {
                crate::case::case_only_rename(&source, &target).map_err(OpFailure::from)
            } else {
                match crate::classify::classify_move(&source, &target, kind)
                    .map_err(OpFailure::from)?
                {
                    MoveClass::SameVolumeRename => fs::rename(&source, &target)
                        .with_context(|| {
                            format!(
                                "renameできません: {} → {}",
                                source.display(),
                                target.display()
                            )
                        })
                        .map_err(OpFailure::from),
                    MoveClass::CrossVolumeFileMove => {
                        move_file_across_volumes(&source, &target, kind)
                    }
                    MoveClass::CrossVolumeDirectoryMove => {
                        move_directory_across_volumes(&source, &target)
                    }
                }
            }
        }
        FsOperation::Copy { from, to, .. } => {
            let source = from.to_fs_path(root);
            let target = to.to_fs_path(root);
            let metadata = fs::symlink_metadata(&source)
                .with_context(|| {
                    format!("コピー元のmetadataを取得できません: {}", source.display())
                })
                .map_err(OpFailure::from)?;
            match crate::scan::kind_from_metadata(&metadata) {
                EntryKind::Dir => copy_tree(&source, &target).map(|_| ()),
                kind @ (EntryKind::File | EntryKind::Symlink) => {
                    copy_single_entry(&source, &target, kind).map_err(|error| {
                        OpFailure::with_progress(
                            error,
                            "コピー完了: 0/1 エントリ; コピー先に不完全なファイルが残っている可能性があります",
                        )
                    })
                }
            }
        }
        FsOperation::Delete { path, .. } => {
            crate::recycle::delete_to_recycle_bin(&path.to_fs_path(root)).map_err(OpFailure::from)
        }
    }
}

fn move_file_across_volumes(
    source: &Path,
    target: &Path,
    kind: EntryKind,
) -> Result<(), OpFailure> {
    if let Err(error) = copy_single_entry(source, target, kind) {
        let cleanup = match cleanup_incomplete_target(target) {
            Ok(true) => "不完全なコピー先を削除済み".to_owned(),
            Ok(false) => "コピー先は作成されていません".to_owned(),
            Err(cleanup_error) => format!("不完全なコピー先の削除にも失敗: {cleanup_error:#}"),
        };
        return Err(OpFailure::with_progress(
            error,
            format!("コピー完了: 0/1 エントリ; {cleanup}"),
        ));
    }

    remove_non_directory_entry(source, kind).map_err(|error| {
        OpFailure::with_progress(
            error.context(format!(
                "コピー後に移動元を削除できません: {}",
                source.display()
            )),
            "コピー完了: 1/1 エントリ; 移動元の削除に失敗",
        )
    })
}

fn move_directory_across_volumes(source: &Path, target: &Path) -> Result<(), OpFailure> {
    let progress = copy_tree(source, target)?;
    fs::remove_dir_all(source)
        .with_context(|| {
            format!(
                "コピー後に移動元ディレクトリを削除できません: {}",
                source.display()
            )
        })
        .map_err(|error| {
            OpFailure::with_progress(
                error,
                format!(
                    "コピー完了: {0}/{0} エントリ; 移動元ディレクトリの削除に失敗",
                    progress.total
                ),
            )
        })
}

fn copy_single_entry(source: &Path, target: &Path, kind: EntryKind) -> anyhow::Result<()> {
    match kind {
        EntryKind::File => fs::copy(source, target).map(|_| ()).with_context(|| {
            format!(
                "ファイルをコピーできません: {} → {}",
                source.display(),
                target.display()
            )
        }),
        EntryKind::Symlink => copy_symlink(source, target),
        EntryKind::Dir => bail!(
            "ディレクトリは単一エントリとしてコピーできません: {}",
            source.display()
        ),
    }
}

#[derive(Debug)]
struct CopyTask {
    source: PathBuf,
    target: PathBuf,
    kind: EntryKind,
}

#[derive(Debug, Clone, Copy)]
struct CopyProgress {
    total: usize,
}

fn copy_tree(source: &Path, target: &Path) -> Result<CopyProgress, OpFailure> {
    let tasks = collect_copy_tasks(source, target).map_err(|error| {
        OpFailure::with_progress(
            error,
            "コピー完了: 0 エントリ; コピー対象の総数を確定できませんでした",
        )
    })?;
    let total = tasks.len();

    fs::create_dir(target)
        .with_context(|| format!("コピー先ディレクトリを作成できません: {}", target.display()))
        .map_err(|error| OpFailure::with_progress(error, copy_progress(0, total)))?;

    for (copied, task) in tasks.into_iter().enumerate() {
        let result = match task.kind {
            EntryKind::Dir => fs::create_dir(&task.target)
                .with_context(|| {
                    format!(
                        "コピー先ディレクトリを作成できません: {}",
                        task.target.display()
                    )
                })
                .map(|_| ()),
            EntryKind::File | EntryKind::Symlink => {
                copy_single_entry(&task.source, &task.target, task.kind)
            }
        };
        if let Err(error) = result {
            return Err(OpFailure::with_progress(
                error,
                copy_progress(copied, total),
            ));
        }
    }

    Ok(CopyProgress { total })
}

fn collect_copy_tasks(source: &Path, target: &Path) -> anyhow::Result<Vec<CopyTask>> {
    let mut tasks = Vec::new();
    let mut directories = vec![(source.to_path_buf(), target.to_path_buf())];

    while let Some((source_dir, target_dir)) = directories.pop() {
        let entries = fs::read_dir(&source_dir).with_context(|| {
            format!(
                "コピー元ディレクトリを読み取れません: {}",
                source_dir.display()
            )
        })?;
        for entry in entries {
            let entry = entry.with_context(|| {
                format!(
                    "コピー元ディレクトリのエントリを読み取れません: {}",
                    source_dir.display()
                )
            })?;
            let child_source = entry.path();
            let child_target = target_dir.join(entry.file_name());
            let metadata = fs::symlink_metadata(&child_source).with_context(|| {
                format!(
                    "コピー元のmetadataを取得できません: {}",
                    child_source.display()
                )
            })?;
            let kind = crate::scan::kind_from_metadata(&metadata);

            tasks.push(CopyTask {
                source: child_source.clone(),
                target: child_target.clone(),
                kind,
            });
            if kind == EntryKind::Dir {
                directories.push((child_source, child_target));
            }
        }
    }

    Ok(tasks)
}

fn copy_progress(copied: usize, total: usize) -> String {
    format!("コピー完了: {copied}/{total} エントリ")
}

fn copy_symlink(source: &Path, target: &Path) -> anyhow::Result<()> {
    let link_target = fs::read_link(source)
        .with_context(|| format!("symlinkを読み取れません: {}", source.display()))?;
    create_symlink_like(source, &link_target, target).with_context(|| {
        format!(
            "symlinkをコピーできません: {} → {}",
            source.display(),
            target.display()
        )
    })
}

#[cfg(unix)]
fn create_symlink_like(_source: &Path, link_target: &Path, target: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(link_target, target)
}

#[cfg(windows)]
fn create_symlink_like(source: &Path, link_target: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::{FileTypeExt, symlink_dir, symlink_file};

    let file_type = fs::symlink_metadata(source)?.file_type();
    if file_type.is_symlink_dir() {
        symlink_dir(link_target, target)
    } else {
        symlink_file(link_target, target)
    }
}

fn cleanup_incomplete_target(target: &Path) -> anyhow::Result<bool> {
    let metadata = match fs::symlink_metadata(target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("不完全なコピー先を確認できません: {}", target.display())
            });
        }
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        bail!(
            "既存ディレクトリはクリーンアップ対象にしません: {}",
            target.display()
        );
    }

    remove_non_directory_entry(target, crate::scan::kind_from_metadata(&metadata))
        .with_context(|| format!("不完全なコピー先を削除できません: {}", target.display()))?;
    Ok(true)
}

fn remove_non_directory_entry(path: &Path, kind: EntryKind) -> anyhow::Result<()> {
    match kind {
        EntryKind::File => fs::remove_file(path)
            .with_context(|| format!("ファイルを削除できません: {}", path.display())),
        EntryKind::Symlink => remove_symlink(path)
            .with_context(|| format!("symlinkを削除できません: {}", path.display())),
        EntryKind::Dir => bail!(
            "ディレクトリを単一エントリ削除できません: {}",
            path.display()
        ),
    }
}

#[cfg(not(windows))]
fn remove_symlink(path: &Path) -> std::io::Result<()> {
    fs::remove_file(path)
}

#[cfg(windows)]
fn remove_symlink(path: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::FileTypeExt;

    let file_type = fs::symlink_metadata(path)?.file_type();
    if file_type.is_symlink_dir() {
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
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
    fn copies_file_and_preserves_source() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("a.txt"), b"content").unwrap();
        let plan = OperationPlan {
            ops: vec![FsOperation::Copy {
                src: EntryId(1),
                from: TreePath::parse("a.txt"),
                to: TreePath::parse("b.txt"),
            }],
        };

        let report = apply_plan(root.path(), &plan);

        assert!(report.all_succeeded());
        assert_eq!(fs::read(root.path().join("a.txt")).unwrap(), b"content");
        assert_eq!(fs::read(root.path().join("b.txt")).unwrap(), b"content");
    }

    #[test]
    fn copies_directory_recursively_and_preserves_source() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("dir")).unwrap();
        fs::create_dir(root.path().join("dir/nested")).unwrap();
        fs::write(root.path().join("dir/first.txt"), b"first").unwrap();
        fs::write(root.path().join("dir/nested/second.txt"), b"second").unwrap();
        let plan = OperationPlan {
            ops: vec![FsOperation::Copy {
                src: EntryId(1),
                from: TreePath::parse("dir"),
                to: TreePath::parse("dir2"),
            }],
        };

        let report = apply_plan(root.path(), &plan);

        assert!(report.all_succeeded());
        assert_eq!(
            fs::read(root.path().join("dir/first.txt")).unwrap(),
            b"first"
        );
        assert_eq!(
            fs::read(root.path().join("dir/nested/second.txt")).unwrap(),
            b"second"
        );
        assert_eq!(
            fs::read(root.path().join("dir2/first.txt")).unwrap(),
            b"first"
        );
        assert_eq!(
            fs::read(root.path().join("dir2/nested/second.txt")).unwrap(),
            b"second"
        );
    }

    #[cfg(unix)]
    #[test]
    fn copies_symlink_as_one_entry_without_descending() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("dir")).unwrap();
        fs::create_dir(root.path().join("outside")).unwrap();
        fs::write(root.path().join("outside/file.txt"), b"outside").unwrap();
        std::os::unix::fs::symlink("../outside", root.path().join("dir/link")).unwrap();
        let plan = OperationPlan {
            ops: vec![FsOperation::Copy {
                src: EntryId(1),
                from: TreePath::parse("dir"),
                to: TreePath::parse("dir2"),
            }],
        };

        let report = apply_plan(root.path(), &plan);

        assert!(report.all_succeeded());
        assert!(
            fs::symlink_metadata(root.path().join("dir2/link"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_link(root.path().join("dir2/link")).unwrap(),
            PathBuf::from("../outside")
        );
    }

    #[test]
    fn file_copy_failure_includes_progress() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("a.txt"), b"content").unwrap();
        let plan = OperationPlan {
            ops: vec![FsOperation::Copy {
                src: EntryId(1),
                from: TreePath::parse("a.txt"),
                to: TreePath::parse("missing/b.txt"),
            }],
        };

        let report = apply_plan(root.path(), &plan);

        assert!(matches!(
            &report.results[0].outcome,
            OpOutcome::Failed {
                progress: Some(progress),
                ..
            } if progress.contains("0/1")
        ));
    }

    #[test]
    fn cross_volume_file_move_helper_copies_then_removes_source() {
        let root = tempdir().unwrap();
        let source = root.path().join("a.txt");
        let target = root.path().join("b.txt");
        fs::write(&source, b"content").unwrap();

        move_file_across_volumes(&source, &target, EntryKind::File).unwrap();

        assert!(!source.exists());
        assert_eq!(fs::read(target).unwrap(), b"content");
    }

    #[test]
    fn cross_volume_directory_move_helper_copies_then_removes_source() {
        let root = tempdir().unwrap();
        let source = root.path().join("dir");
        let target = root.path().join("dir2");
        fs::create_dir(&source).unwrap();
        fs::create_dir(source.join("nested")).unwrap();
        fs::write(source.join("nested/file.txt"), b"content").unwrap();

        move_directory_across_volumes(&source, &target).unwrap();

        assert!(!source.exists());
        assert_eq!(
            fs::read(target.join("nested/file.txt")).unwrap(),
            b"content"
        );
    }
}
