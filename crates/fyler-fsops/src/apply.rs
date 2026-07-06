//! apply: 承認済みOperationPlanの実行。**実FS書き込みの入口はここだけ**(絶対ルール1)。

use std::collections::HashSet;
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

/// 上書き対象を伴わないplanを実行し、操作単位の結果を返す。
///
/// 呼び出し契約: 保存状態機械(`fyler_core::save`)の `Applying` 状態
/// (= 確認ダイアログで承認済み)からのみ呼ぶこと。M2まではdry-runのみで、
/// この関数は呼ばれない。
///
/// preflightで検出した既存ファイルを承認付きで上書きする場合は
/// [`apply_plan_with_overwrites`]を使う。この関数は空の上書き対象を渡すため、
/// 従来どおり既存の移動先を上書きせず失敗する。
pub fn apply_plan(root: &Path, plan: &OperationPlan) -> CommitReport {
    apply_plan_with_overwrites(root, plan, &HashSet::new())
}

/// 承認済みの上書き対象(preflightでユーザーへ提示済みの移動先)を伴ってplanを実行する。
///
/// `overwrites` に含まれる移動先は、操作の直前に実体を確認し、ファイル/シンボリック
/// リンクであれば**ごみ箱へ退避**してから実行する。ディレクトリに変わっていた場合は
/// 上書きせず操作失敗として報告する(TOCTOU防衛)。
///
/// 呼び出し契約と実装契約:
/// - 保存状態機械(`fyler_core::save`)の `Applying` 状態からのみ呼ぶ
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
pub fn apply_plan_with_overwrites(
    root: &Path,
    plan: &OperationPlan,
    overwrites: &HashSet<TreePath>,
) -> CommitReport {
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

        let outcome = match execute_operation(root, operation, overwrites) {
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

fn execute_operation(
    root: &Path,
    operation: &FsOperation,
    overwrites: &HashSet<TreePath>,
) -> Result<(), OpFailure> {
    match operation {
        FsOperation::Create { path, kind } => {
            let target = crate::long_path::to_fs(&path.to_fs_path(root));
            match kind {
                // create_dir / create_new(true) は既存パスで必ず失敗する
                // (fs::File::createの黙った切り詰めを防ぐ)。
                EntryKind::Dir => {
                    if overwrites.contains(path) {
                        recycle_approved_target(&target)?;
                    }
                    fs::create_dir(&target)
                        .with_context(|| {
                            format!("ディレクトリを作成できません: {}", target.display())
                        })
                        .map_err(OpFailure::from)
                }
                EntryKind::File => {
                    if overwrites.contains(path) {
                        recycle_approved_target(&target)?;
                    }
                    fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&target)
                        .map(|_| ())
                        .with_context(|| format!("ファイルを作成できません: {}", target.display()))
                        .map_err(OpFailure::from)
                }
                EntryKind::Symlink => Err(OpFailure::from(anyhow!("SYMLINKのCREATEは未実装"))),
            }
        }
        FsOperation::Move { from, to, .. } => {
            let source = crate::long_path::to_fs(&from.to_fs_path(root));
            let target = crate::long_path::to_fs(&to.to_fs_path(root));
            let metadata = fs::symlink_metadata(&source)
                .with_context(|| format!("移動元のmetadataを取得できません: {}", source.display()))
                .map_err(OpFailure::from)?;
            let kind = crate::scan::kind_from_metadata(&metadata);

            let case_only_rename = is_case_only_rename(from, to);
            let case_sensitive_directory = case_only_rename
                && source.parent().is_some_and(|parent| {
                    crate::case::dir_is_case_sensitive(parent).unwrap_or(false)
                });

            // case-sensitiveディレクトリでは大文字小文字違いも別名であり、2段renameは
            // 既存の別エントリを黙って上書きし得る。通常rename経路の移動先preflightを
            // 必ず通す。判定失敗時は従来どおり保守的に2段renameを使う。
            if case_only_rename && !case_sensitive_directory {
                crate::case::case_only_rename(&source, &target).map_err(OpFailure::from)
            } else {
                if overwrites.contains(to) {
                    recycle_approved_target(&target)?;
                }
                // fs::renameはWindows(MOVEFILE_REPLACE_EXISTING)でもUnixでも
                // 既存の移動先ファイルを黙って上書きする。baseline取得後の外部変更
                // (TOCTOU)でユーザーデータを消さないよう、直前に存在確認する。
                ensure_target_vacant(&target)?;
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
            let source = crate::long_path::to_fs(&from.to_fs_path(root));
            let target = crate::long_path::to_fs(&to.to_fs_path(root));
            let metadata = fs::symlink_metadata(&source)
                .with_context(|| {
                    format!("コピー元のmetadataを取得できません: {}", source.display())
                })
                .map_err(OpFailure::from)?;
            if overwrites.contains(to) {
                recycle_approved_target(&target)?;
            }
            // fs::copyも既存の移動先を黙って上書きするため、同様に存在確認する。
            ensure_target_vacant(&target)?;
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

/// 承認済み移動先の現在の実体を確認し、非ディレクトリならごみ箱へ退避する。
///
/// preflight後にディレクトリへ変化していた場合は破壊せず失敗させる。
fn recycle_approved_target(target: &Path) -> Result<(), OpFailure> {
    match fs::symlink_metadata(crate::long_path::to_fs(target)) {
        Ok(metadata) if crate::scan::kind_from_metadata(&metadata) == EntryKind::Dir => {
            Err(OpFailure::from(anyhow!(
                "移動先がディレクトリに変わっているため上書きを中止しました: {}",
                target.display()
            )))
        }
        Ok(_) => crate::recycle::delete_to_recycle_bin(target).map_err(OpFailure::from),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(OpFailure::from(anyhow::Error::from(error).context(
            format!("上書き対象の存在確認に失敗しました: {}", target.display()),
        ))),
    }
}

/// Move/Copyの直前preflight: 移動先にエントリが既に存在したら失敗させる。
/// planのvalidate/orderingが正しければ通常は空いているはずで、これは
/// baseline取得後の外部変更(TOCTOU)による黙った上書きへの最終防衛線。
fn ensure_target_vacant(target: &Path) -> Result<(), OpFailure> {
    match fs::symlink_metadata(crate::long_path::to_fs(target)) {
        Ok(_) => Err(OpFailure::from(anyhow!(
            "移動先に別のエントリが既に存在します(外部で変更された可能性): {}",
            target.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(OpFailure::from(anyhow::Error::from(error).context(
            format!("移動先の存在確認に失敗しました: {}", target.display()),
        ))),
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
    fs::remove_dir_all(crate::long_path::to_fs(source))
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
        EntryKind::File => fs::copy(
            crate::long_path::to_fs(source),
            crate::long_path::to_fs(target),
        )
        .map(|_| ())
        .with_context(|| {
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

    fs::create_dir(crate::long_path::to_fs(target))
        .with_context(|| format!("コピー先ディレクトリを作成できません: {}", target.display()))
        .map_err(|error| OpFailure::with_progress(error, copy_progress(0, total)))?;

    for (copied, task) in tasks.into_iter().enumerate() {
        let result = match task.kind {
            EntryKind::Dir => fs::create_dir(crate::long_path::to_fs(&task.target))
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
        let entries = fs::read_dir(crate::long_path::to_fs(&source_dir)).with_context(|| {
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
            let metadata = fs::symlink_metadata(crate::long_path::to_fs(&child_source))
                .with_context(|| {
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
    let link_target = fs::read_link(crate::long_path::to_fs(source))
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
    std::os::unix::fs::symlink(link_target, crate::long_path::to_fs(target))
}

#[cfg(windows)]
fn create_symlink_like(source: &Path, link_target: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::{FileTypeExt, symlink_dir, symlink_file};

    let file_type = fs::symlink_metadata(crate::long_path::to_fs(source))?.file_type();
    if file_type.is_symlink_dir() {
        symlink_dir(link_target, crate::long_path::to_fs(target))
    } else {
        symlink_file(link_target, crate::long_path::to_fs(target))
    }
}

fn cleanup_incomplete_target(target: &Path) -> anyhow::Result<bool> {
    let metadata = match fs::symlink_metadata(crate::long_path::to_fs(target)) {
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
        EntryKind::File => fs::remove_file(crate::long_path::to_fs(path))
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
    fs::remove_file(crate::long_path::to_fs(path))
}

#[cfg(windows)]
fn remove_symlink(path: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::FileTypeExt;

    let file_type = fs::symlink_metadata(crate::long_path::to_fs(path))?.file_type();
    if file_type.is_symlink_dir() {
        fs::remove_dir(crate::long_path::to_fs(path))
    } else {
        fs::remove_file(crate::long_path::to_fs(path))
    }
}

pub(crate) fn is_case_only_rename(from: &TreePath, to: &TreePath) -> bool {
    let (Some(from_name), Some(to_name)) = (from.name(), to.name()) else {
        return false;
    };
    // ASCII限定比較だと `Ähnlich → ähnlich` 等の非ASCII case-only renameが
    // 通常のrename経路に落ち、case-insensitiveなNTFSでpreflightに衝突する。
    // Windowsの大文字小文字判定の近似としてUnicode小文字化で比較する。
    from.parent() == to.parent()
        && from_name != to_name
        && from_name.to_lowercase() == to_name.to_lowercase()
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

    /// ディレクトリ直下の実際の格納名を返す(ソート済み)。
    ///
    /// NTFSなどcase-insensitiveなFSでは `Path::exists()` がcase違いにも
    /// マッチするため、case-only renameの検証はこれで格納名を完全一致比較する。
    fn stored_entry_names(directory: &Path) -> Vec<String> {
        let mut names = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

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
        // NTFSはcase-insensitiveで `Name.txt` の exists() が rename後の
        // `name.txt` にもマッチするため、実際に格納された名前を列挙して検証する。
        assert_eq!(stored_entry_names(root.path()), ["name.txt"]);
        assert_eq!(fs::read(root.path().join("name.txt")).unwrap(), b"content");
    }

    #[cfg(not(windows))]
    #[test]
    fn case_sensitive_directory_uses_normal_rename_preflight_for_case_only_names() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("Name.txt"), b"source").unwrap();
        fs::write(root.path().join("name.txt"), b"existing").unwrap();
        let plan = OperationPlan {
            ops: vec![FsOperation::Move {
                id: EntryId(1),
                from: TreePath::parse("Name.txt"),
                to: TreePath::parse("name.txt"),
            }],
        };

        let report = apply_plan(root.path(), &plan);

        assert!(matches!(
            report.results[0].outcome,
            OpOutcome::Failed { .. }
        ));
        assert_eq!(fs::read(root.path().join("Name.txt")).unwrap(), b"source");
        assert_eq!(fs::read(root.path().join("name.txt")).unwrap(), b"existing");
    }

    #[test]
    fn move_fails_instead_of_overwriting_existing_target() {
        // baseline取得後に外部で移動先が作られた場合(TOCTOU)、fs::renameは
        // 黙って上書きする。preflightで失敗させ、既存内容を守る。
        let root = tempdir().unwrap();
        fs::write(root.path().join("a.txt"), b"source").unwrap();
        fs::write(root.path().join("b.txt"), b"existing").unwrap();
        let plan = OperationPlan {
            ops: vec![FsOperation::Move {
                id: EntryId(1),
                from: TreePath::parse("a.txt"),
                to: TreePath::parse("b.txt"),
            }],
        };

        let report = apply_plan(root.path(), &plan);

        assert!(matches!(
            report.results[0].outcome,
            OpOutcome::Failed { .. }
        ));
        assert_eq!(fs::read(root.path().join("a.txt")).unwrap(), b"source");
        assert_eq!(fs::read(root.path().join("b.txt")).unwrap(), b"existing");
    }

    #[test]
    fn approved_move_recycles_existing_target_before_rename() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("src.txt"), b"source").unwrap();
        fs::write(root.path().join("target.txt"), b"existing").unwrap();
        let target = TreePath::parse("target.txt");
        let plan = OperationPlan {
            ops: vec![FsOperation::Move {
                id: EntryId(1),
                from: TreePath::parse("src.txt"),
                to: target.clone(),
            }],
        };
        let overwrites = HashSet::from([target]);

        let report = apply_plan_with_overwrites(root.path(), &plan, &overwrites);

        assert!(report.all_succeeded());
        assert!(!root.path().join("src.txt").exists());
        assert_eq!(fs::read(root.path().join("target.txt")).unwrap(), b"source");
    }

    #[test]
    fn approved_move_stops_if_target_is_a_directory() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("src.txt"), b"source").unwrap();
        fs::create_dir(root.path().join("target")).unwrap();
        let target = TreePath::parse("target");
        let plan = OperationPlan {
            ops: vec![FsOperation::Move {
                id: EntryId(1),
                from: TreePath::parse("src.txt"),
                to: target.clone(),
            }],
        };
        let overwrites = HashSet::from([target]);

        let report = apply_plan_with_overwrites(root.path(), &plan, &overwrites);

        assert!(matches!(
            &report.results[0].outcome,
            OpOutcome::Failed { error, .. }
                if error.contains("ディレクトリに変わっているため上書きを中止")
        ));
        assert_eq!(fs::read(root.path().join("src.txt")).unwrap(), b"source");
        assert!(root.path().join("target").is_dir());
    }

    #[test]
    fn copy_fails_instead_of_overwriting_existing_target() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("a.txt"), b"source").unwrap();
        fs::write(root.path().join("b.txt"), b"existing").unwrap();
        let plan = OperationPlan {
            ops: vec![FsOperation::Copy {
                src: EntryId(1),
                from: TreePath::parse("a.txt"),
                to: TreePath::parse("b.txt"),
            }],
        };

        let report = apply_plan(root.path(), &plan);

        assert!(matches!(
            report.results[0].outcome,
            OpOutcome::Failed { .. }
        ));
        assert_eq!(fs::read(root.path().join("b.txt")).unwrap(), b"existing");
    }

    #[test]
    fn create_file_fails_without_truncating_existing_target() {
        // fs::File::createは既存ファイルを黙って0バイトに切り詰める。
        // create_newで失敗させ、既存内容を守る。
        let root = tempdir().unwrap();
        fs::write(root.path().join("a.txt"), b"existing").unwrap();
        let plan = OperationPlan {
            ops: vec![FsOperation::Create {
                path: TreePath::parse("a.txt"),
                kind: EntryKind::File,
            }],
        };

        let report = apply_plan(root.path(), &plan);

        assert!(matches!(
            report.results[0].outcome,
            OpOutcome::Failed { .. }
        ));
        assert_eq!(fs::read(root.path().join("a.txt")).unwrap(), b"existing");
    }

    #[test]
    fn supports_non_ascii_case_only_rename() {
        // `Ähnlich → ähnlich` はASCII限定比較だとcase-only判定から漏れ、
        // case-insensitiveなNTFSで通常rename経路のpreflightに衝突する。
        // Unicode小文字化での判定を固定する(実FS挙動はcase-sensitiveな
        // Linuxでは経路によらず成功するため、判定関数も直接検証する)。
        assert!(is_case_only_rename(
            &TreePath::parse("Ähnlich.txt"),
            &TreePath::parse("ähnlich.txt"),
        ));

        let root = tempdir().unwrap();
        fs::write(root.path().join("Ähnlich.txt"), b"content").unwrap();
        let plan = OperationPlan {
            ops: vec![FsOperation::Move {
                id: EntryId(1),
                from: TreePath::parse("Ähnlich.txt"),
                to: TreePath::parse("ähnlich.txt"),
            }],
        };

        let report = apply_plan(root.path(), &plan);

        assert!(report.all_succeeded());
        // supports_case_only_rename と同じ理由で、格納名の完全一致で検証する。
        assert_eq!(stored_entry_names(root.path()), ["ähnlich.txt"]);
        assert_eq!(
            fs::read(root.path().join("ähnlich.txt")).unwrap(),
            b"content"
        );
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
