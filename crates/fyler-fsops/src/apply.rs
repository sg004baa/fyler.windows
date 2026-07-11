//! apply: 承認済みOperationPlanの実行。**実FS書き込みの入口はここだけ**(絶対ルール1)。
//!
//! 承認済み上書きでは、旧targetを同一親ディレクトリの `.fyler-staged-*` へ一時的に
//! renameしてから本体操作を行う。成功時はstaging内の旧targetをごみ箱へ送り、失敗時は
//! 元位置へ戻す。staging dirは処理中だけ親dirに現れ、ごみ箱上の表示名は元basenameを
//! 保つ一方、「元の場所」はstaging dirとして記録される。

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, anyhow, bail};
use fyler_core::path::TreePath;
use fyler_core::plan::{FsOperation, OperationPlan};
use fyler_core::report::{ApplyProgress, CommitReport, OpOutcome, OpResult};
use fyler_core::transfer::{TransferKind, TransferOp, TransferPlan};
use fyler_core::tree::EntryKind;

use crate::classify::MoveClass;
use crate::undo::UndoRecorder;

#[derive(Debug)]
pub(crate) struct OpFailure {
    pub(crate) error: String,
    progress: Option<String>,
}

impl OpFailure {
    pub(crate) fn with_progress(error: anyhow::Error, progress: impl Into<String>) -> Self {
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

#[cfg(test)]
type FaultHook = Box<dyn FnMut(&str, &Path) -> Option<anyhow::Error>>;

#[cfg(test)]
thread_local! {
    static FAULT_INJECTION: std::cell::RefCell<Option<FaultHook>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
fn fault_point(stage: &str, path: &Path) -> anyhow::Result<()> {
    FAULT_INJECTION.with(|hook| {
        let mut hook = hook.borrow_mut();
        if let Some(error) = hook.as_mut().and_then(|hook| hook(stage, path)) {
            Err(error)
        } else {
            Ok(())
        }
    })
}

#[cfg(not(test))]
fn fault_point(_stage: &str, _path: &Path) -> anyhow::Result<()> {
    Ok(())
}

#[derive(Debug)]
struct StagedTarget {
    staged_path: PathBuf,
    staging_dir: PathBuf,
    original: PathBuf,
}

impl StagedTarget {
    fn commit_to_trash(self) -> anyhow::Result<()> {
        fault_point("commit_to_trash", &self.staged_path)?;
        crate::recycle::delete_to_recycle_bin(&self.staged_path)?;
        let _ = fs::remove_dir(crate::long_path::to_fs(&self.staging_dir));
        Ok(())
    }

    fn restore(self) -> anyhow::Result<()> {
        fault_point("restore", &self.staged_path)?;
        fs::rename(
            crate::long_path::to_fs(&self.staged_path),
            crate::long_path::to_fs(&self.original),
        )
        .with_context(|| {
            format!(
                "Failed to restore staged old entry to its original location: {} → {}",
                self.staged_path.display(),
                self.original.display()
            )
        })?;
        let _ = fs::remove_dir(crate::long_path::to_fs(&self.staging_dir));
        Ok(())
    }
}

fn stage_target_aside(target: &Path) -> Result<Option<StagedTarget>, OpFailure> {
    match fs::symlink_metadata(crate::long_path::to_fs(target)) {
        Ok(metadata) if crate::scan::kind_from_metadata(&metadata) == EntryKind::Dir => {
            return Err(OpFailure::from(anyhow!(
                "Overwrite cancelled because the destination became a directory: {}",
                target.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(OpFailure::from(anyhow::Error::from(error).context(
                format!("Failed to check overwrite target: {}", target.display()),
            )));
        }
    }

    let parent = target.parent().ok_or_else(|| {
        OpFailure::from(anyhow!(
            "Failed to get parent directory of overwrite target: {}",
            target.display()
        ))
    })?;
    let basename = target.file_name().ok_or_else(|| {
        OpFailure::from(anyhow!(
            "Failed to get file name of overwrite target: {}",
            target.display()
        ))
    })?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();

    let mut last_error = None;
    for suffix in 0..8_u8 {
        let staging_dir = parent.join(format!(
            ".fyler-staged-{}-{nanos}-{suffix}",
            std::process::id()
        ));
        match fs::create_dir(crate::long_path::to_fs(&staging_dir)) {
            Ok(()) => {
                let staged_path = staging_dir.join(basename);
                if let Err(error) = fs::rename(
                    crate::long_path::to_fs(target),
                    crate::long_path::to_fs(&staged_path),
                ) {
                    let _ = fs::remove_dir(crate::long_path::to_fs(&staging_dir));
                    return Err(OpFailure::from(anyhow::Error::from(error).context(
                        format!(
                            "Failed to stage overwrite target: {} → {}",
                            target.display(),
                            staged_path.display()
                        ),
                    )));
                }
                return Ok(Some(StagedTarget {
                    staged_path,
                    staging_dir,
                    original: target.to_path_buf(),
                }));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                last_error = Some(error);
            }
            Err(error) => {
                return Err(OpFailure::from(anyhow::Error::from(error).context(
                    format!(
                        "Failed to create staging directory for overwrite target: {}",
                        staging_dir.display()
                    ),
                )));
            }
        }
    }

    Err(OpFailure::from(anyhow::Error::from(
        last_error
            .unwrap_or_else(|| std::io::Error::other("Failed to allocate staging directory name")),
    )))
}

fn execute_with_staged_overwrite(
    target: &Path,
    approved: bool,
    mut recorder: Option<&mut UndoRecorder>,
    execute: impl FnOnce() -> Result<(), OpFailure>,
) -> Result<(), OpFailure> {
    if !approved {
        return execute();
    }

    let backup = recorder
        .as_deref()
        .map(|recorder| recorder.backup_for_next_step(target))
        .transpose()
        .map_err(OpFailure::from)?;
    let staged = match stage_target_aside(target) {
        Ok(staged) => staged,
        Err(error) => {
            if let (Some(recorder), Some(backup)) = (recorder.as_deref(), backup.as_ref()) {
                recorder.discard_backup(backup);
            }
            return Err(error);
        }
    };

    let Some(staged) = staged else {
        if let (Some(recorder), Some(backup)) = (recorder.as_deref(), backup.as_ref()) {
            recorder.discard_backup(backup);
        }
        return execute();
    };
    let staged_path = staged.staged_path.clone();
    let original = staged.original.clone();

    match execute() {
        Ok(()) => match staged.commit_to_trash() {
            Ok(()) => {
                if let (Some(recorder), Some(backup)) = (recorder.as_deref_mut(), backup) {
                    recorder.record_overwritten(target, backup);
                }
                Ok(())
            }
            Err(error) => {
                if let (Some(recorder), Some(backup)) = (recorder.as_deref(), backup.as_ref()) {
                    recorder.discard_backup(backup);
                }
                Err(OpFailure::with_progress(
                    error,
                    format!(
                        "The operation completed, but the overwritten old entry could not be moved to the recycle bin. It remains at {}",
                        staged_path.display()
                    ),
                ))
            }
        },
        Err(operation_error) => {
            let cleanup = cleanup_staged_operation_target(target);
            let restore = if cleanup.is_ok() {
                staged.restore()
            } else {
                Err(anyhow!(
                    "Cannot restore because the incomplete target could not be removed"
                ))
            };
            match (cleanup, restore) {
                (Ok(()), Ok(())) => {
                    if let (Some(recorder), Some(backup)) = (recorder.as_deref(), backup.as_ref()) {
                        recorder.discard_backup(backup);
                    }
                    Err(operation_error)
                }
                (cleanup, restore) => {
                    let backup_note = backup.as_ref().map_or(String::new(), |backup| {
                        format!(" Backup payload {} is also retained.", backup.payload_rel)
                    });
                    Err(OpFailure::with_progress(
                        anyhow!(operation_error.error),
                        format!(
                            "Compensation after the operation failed also failed (cleanup: {}; restore: {}). The old entry remains staged at {}. Restore it manually to {}.{}",
                            cleanup
                                .err()
                                .map_or_else(|| "success".to_owned(), |error| error.to_string()),
                            restore
                                .err()
                                .map_or_else(|| "success".to_owned(), |error| error.to_string()),
                            staged_path.display(),
                            original.display(),
                            backup_note
                        ),
                    ))
                }
            }
        }
    }
}

fn cleanup_staged_operation_target(target: &Path) -> anyhow::Result<()> {
    let metadata = match fs::symlink_metadata(crate::long_path::to_fs(target)) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let kind = crate::scan::kind_from_metadata(&metadata);
    if kind == EntryKind::Dir {
        fs::remove_dir_all(crate::long_path::to_fs(target)).with_context(|| {
            format!(
                "Failed to remove incomplete directory: {}",
                target.display()
            )
        })
    } else {
        remove_non_directory_entry(target, kind)
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
/// リンクであれば同一親dir内へstaging退避する。本体操作の成功後だけごみ箱へ送り、
/// 失敗時は元位置へ復元する。ディレクトリに変わっていた場合は上書きせず操作失敗として
/// 報告する(TOCTOU防衛)。
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
    let cancel = AtomicBool::new(false);
    apply_plan_cancellable(root, plan, overwrites, &cancel, &mut |_| {}, None)
}

/// planを実行する。`cancel`が立った時点で残りの操作を実行せず
/// [`OpOutcome::Skipped`]として報告する。実行中の1操作は完走し、
/// キャンセルは次の操作との間でのみ反映する。
///
/// `on_progress`は各操作の実行開始前と全操作完了後に呼ぶ。最終通知の
/// [`ApplyProgress::completed`]は、Skippedを除いて実際に実行を試みた操作数である。
///
/// 呼び出し契約は [`apply_plan`] と同じで、保存状態機械
/// (`fyler_core::save`)の`Applying`状態からのみ呼ぶこと。
pub fn apply_plan_cancellable(
    root: &Path,
    plan: &OperationPlan,
    overwrites: &HashSet<TreePath>,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(ApplyProgress),
    mut recorder: Option<&mut UndoRecorder>,
) -> CommitReport {
    let mut results = Vec::with_capacity(plan.ops.len());
    let mut failed_directories = Vec::new();
    let mut attempted = 0;
    let total = plan.ops.len();

    for (index, operation) in plan.ops.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            results.extend(plan.ops[index..].iter().cloned().map(|op| OpResult {
                op,
                outcome: OpOutcome::Skipped {
                    reason: "Cancelled by user".to_owned(),
                },
            }));
            break;
        }

        on_progress(ApplyProgress {
            completed: index,
            total,
            current: Some(operation.clone()),
        });

        if let Some(failed_parent) = failed_parent(operation, &failed_directories) {
            results.push(OpResult {
                op: operation.clone(),
                outcome: OpOutcome::Skipped {
                    reason: format!(
                        "A preceding parent directory creation failed: {failed_parent}"
                    ),
                },
            });
            continue;
        }

        attempted += 1;
        let outcome = match execute_operation(root, operation, overwrites, recorder.as_deref_mut())
        {
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

    on_progress(ApplyProgress {
        completed: attempted,
        total,
        current: None,
    });

    CommitReport { results }
}

/// 承認済みのpane間transfer planを実行する。
///
/// **呼び出し契約**: app層のtransfer確認ダイアログでplanと`overwrites`を提示し、
/// ユーザーが承認した後に限ってapply workerから呼ぶこと。M10-3のapp/GUI配線が
/// この契約を担う。この関数をpreflightやプレビュー目的で呼んではならない。
///
/// 実装契約:
/// - `plan.ops`を並べ替えず、`CommitReport.results`を同順・同数で返す
/// - キャンセルは操作間だけで反映し、残りを[`OpOutcome::Skipped`]にする
/// - Moveは[`crate::classify::classify_move`]で同一/クロスvolumeを分類する
/// - 承認済み上書きも実行直前に再確認し、非ディレクトリだけをごみ箱へ退避する
/// - FS APIの直前には必ず[`crate::long_path::to_fs`]を通す
pub fn apply_transfer_plan_cancellable(
    plan: &TransferPlan,
    overwrites: &HashSet<PathBuf>,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(ApplyProgress<TransferOp>),
) -> CommitReport<TransferOp> {
    let from_root = absolute_path(&plan.from_root);
    let to_root = absolute_path(&plan.to_root);
    let mut results = Vec::with_capacity(plan.ops.len());
    let mut attempted = 0;
    let total = plan.ops.len();

    for (index, operation) in plan.ops.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            results.extend(plan.ops[index..].iter().cloned().map(|op| OpResult {
                op,
                outcome: OpOutcome::Skipped {
                    reason: "Cancelled by user".to_owned(),
                },
            }));
            break;
        }

        on_progress(ApplyProgress {
            completed: index,
            total,
            current: Some(operation.clone()),
        });
        attempted += 1;

        let outcome = match execute_transfer_operation(&from_root, &to_root, operation, overwrites)
        {
            Ok(()) => OpOutcome::Success,
            Err(failure) => OpOutcome::Failed {
                error: failure.error,
                progress: failure.progress,
            },
        };
        results.push(OpResult {
            op: operation.clone(),
            outcome,
        });
    }

    on_progress(ApplyProgress {
        completed: attempted,
        total,
        current: None,
    });

    CommitReport { results }
}

fn execute_transfer_operation(
    from_root: &Path,
    to_root: &Path,
    operation: &TransferOp,
    overwrites: &HashSet<PathBuf>,
) -> Result<(), OpFailure> {
    let source = operation.from.to_fs_path(from_root);
    let target = operation.to.to_fs_path(to_root);
    let metadata = fs::symlink_metadata(crate::long_path::to_fs(&source))
        .with_context(|| {
            format!(
                "Failed to get transfer source metadata: {}",
                source.display()
            )
        })
        .map_err(OpFailure::from)?;
    let actual_kind = crate::scan::kind_from_metadata(&metadata);
    if actual_kind != operation.entry_kind {
        return Err(OpFailure::from(anyhow!(
            "Transfer source type changed after the plan was finalized: {}",
            source.display()
        )));
    }

    let approved_overwrite = overwrites
        .iter()
        .any(|approved| transfer_paths_equal(approved, &target));
    let case_only_rename = operation.kind == TransferKind::Move
        && is_case_only_absolute_rename(&source, &target)
        && !source
            .parent()
            .is_some_and(|parent| crate::case::dir_is_case_sensitive(parent).unwrap_or(false));

    if case_only_rename {
        return crate::case::case_only_rename(&source, &target).map_err(OpFailure::from);
    }
    execute_with_staged_overwrite(&target, approved_overwrite, None, || {
        ensure_target_vacant(&target)?;
        match operation.kind {
            TransferKind::Copy => {
                fault_point("transfer_copy", &target).map_err(OpFailure::from)?;
                match actual_kind {
                    EntryKind::Dir => copy_tree(&source, &target).map(|_| ()),
                    kind @ (EntryKind::File | EntryKind::Symlink) => {
                        copy_single_entry(&source, &target, kind).map_err(|error| {
                            OpFailure::with_progress(
                                error,
                                "Copy complete: 0/1 entries; an incomplete file may remain at the destination",
                            )
                        })
                    }
                }
            }
            TransferKind::Move => {
                fault_point("transfer_move", &target).map_err(OpFailure::from)?;
                match crate::classify::classify_move(&source, &target, actual_kind)
                    .map_err(OpFailure::from)?
                {
                    MoveClass::SameVolumeRename => fs::rename(
                        crate::long_path::to_fs(&source),
                        crate::long_path::to_fs(&target),
                    )
                    .with_context(|| {
                        format!(
                            "Failed to rename between panes: {} → {}",
                            source.display(),
                            target.display()
                        )
                    })
                    .map_err(OpFailure::from),
                    MoveClass::CrossVolumeFileMove => {
                        move_file_across_volumes(&source, &target, actual_kind)
                    }
                    MoveClass::CrossVolumeDirectoryMove => {
                        move_directory_across_volumes(&source, &target)
                    }
                }
            }
        }
    })
}

fn absolute_path(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

fn transfer_paths_equal(left: &Path, right: &Path) -> bool {
    let left = normalized_path_text(left);
    let right_text = normalized_path_text(right);
    if right
        .parent()
        .is_some_and(|parent| crate::case::dir_is_case_sensitive(parent).unwrap_or(false))
    {
        left == right_text
    } else {
        left.to_lowercase() == right_text.to_lowercase()
    }
}

fn normalized_path_text(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("\0")
}

fn is_case_only_absolute_rename(from: &Path, to: &Path) -> bool {
    from.parent()
        .zip(to.parent())
        .is_some_and(|(left, right)| transfer_paths_equal(left, right))
        && from.file_name() != to.file_name()
        && from
            .file_name()
            .zip(to.file_name())
            .is_some_and(|(left, right)| {
                left.to_string_lossy().to_lowercase() == right.to_string_lossy().to_lowercase()
            })
}

fn execute_operation(
    root: &Path,
    operation: &FsOperation,
    overwrites: &HashSet<TreePath>,
    mut recorder: Option<&mut UndoRecorder>,
) -> Result<(), OpFailure> {
    match operation {
        FsOperation::Create { path, kind } => {
            let target = path.to_fs_path(root);
            let fs_target = crate::long_path::to_fs(&target);
            match kind {
                // create_dir / create_new(true) は既存パスで必ず失敗する
                // (fs::File::createの黙った切り詰めを防ぐ)。
                EntryKind::Dir => {
                    execute_with_staged_overwrite(
                        &target,
                        overwrites.contains(path),
                        recorder.as_deref_mut(),
                        || {
                            fault_point("create_dir", &target).map_err(OpFailure::from)?;
                            fs::create_dir(&fs_target)
                                .with_context(|| {
                                    format!("Failed to create directory: {}", target.display())
                                })
                                .map_err(OpFailure::from)
                        },
                    )?;
                    if let Some(recorder) = recorder {
                        recorder.record_created(&target, *kind);
                    }
                    Ok(())
                }
                EntryKind::File => {
                    execute_with_staged_overwrite(
                        &target,
                        overwrites.contains(path),
                        recorder.as_deref_mut(),
                        || {
                            fault_point("create_file", &target).map_err(OpFailure::from)?;
                            fs::OpenOptions::new()
                                .write(true)
                                .create_new(true)
                                .open(&fs_target)
                                .map(|_| ())
                                .with_context(|| {
                                    format!("Failed to create file: {}", target.display())
                                })
                                .map_err(OpFailure::from)
                        },
                    )?;
                    if let Some(recorder) = recorder {
                        recorder.record_created(&target, *kind);
                    }
                    Ok(())
                }
                EntryKind::Symlink => Err(OpFailure::from(anyhow!(
                    "CREATE for SYMLINK is not implemented"
                ))),
            }
        }
        FsOperation::Move { from, to, .. } => {
            let source = from.to_fs_path(root);
            let target = to.to_fs_path(root);
            let fs_source = crate::long_path::to_fs(&source);
            let fs_target = crate::long_path::to_fs(&target);
            let metadata = fs::symlink_metadata(&fs_source)
                .with_context(|| format!("Failed to get source metadata: {}", source.display()))
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
                crate::case::case_only_rename(&source, &target).map_err(OpFailure::from)?;
            } else {
                execute_with_staged_overwrite(
                    &target,
                    overwrites.contains(to),
                    recorder.as_deref_mut(),
                    || {
                        // fs::renameはWindows(MOVEFILE_REPLACE_EXISTING)でもUnixでも
                        // 既存の移動先ファイルを黙って上書きする。baseline取得後の外部変更
                        // (TOCTOU)でユーザーデータを消さないよう、直前に存在確認する。
                        ensure_target_vacant(&target)?;
                        match crate::classify::classify_move(&source, &target, kind)
                            .map_err(OpFailure::from)?
                        {
                            MoveClass::SameVolumeRename => {
                                fault_point("rename", &target).map_err(OpFailure::from)?;
                                fs::rename(&fs_source, &fs_target)
                                    .with_context(|| {
                                        format!(
                                            "Failed to rename: {} → {}",
                                            source.display(),
                                            target.display()
                                        )
                                    })
                                    .map_err(OpFailure::from)
                            }
                            MoveClass::CrossVolumeFileMove => {
                                fault_point("cross_volume_file_move", &target)
                                    .map_err(OpFailure::from)?;
                                move_file_across_volumes(&source, &target, kind)
                            }
                            MoveClass::CrossVolumeDirectoryMove => {
                                fault_point("cross_volume_dir_move", &target)
                                    .map_err(OpFailure::from)?;
                                move_directory_across_volumes(&source, &target)
                            }
                        }
                    },
                )?;
            }
            if let Some(recorder) = recorder {
                recorder.record_moved(&source, &target, kind, case_only_rename);
            }
            Ok(())
        }
        FsOperation::Copy { from, to, .. } => {
            let source = from.to_fs_path(root);
            let target = to.to_fs_path(root);
            let metadata = fs::symlink_metadata(crate::long_path::to_fs(&source))
                .with_context(|| {
                    format!("Failed to get copy source metadata: {}", source.display())
                })
                .map_err(OpFailure::from)?;
            let kind = crate::scan::kind_from_metadata(&metadata);
            execute_with_staged_overwrite(
                &target,
                overwrites.contains(to),
                recorder.as_deref_mut(),
                || {
                    // fs::copyも既存の移動先を黙って上書きするため、同様に存在確認する。
                    ensure_target_vacant(&target)?;
                    match kind {
                        EntryKind::Dir => {
                            fault_point("copy_tree", &target).map_err(OpFailure::from)?;
                            copy_tree(&source, &target).map(|_| ())
                        }
                        kind @ (EntryKind::File | EntryKind::Symlink) => {
                            fault_point("copy_single", &target).map_err(OpFailure::from)?;
                            copy_single_entry(&source, &target, kind).map_err(|error| {
                                OpFailure::with_progress(
                                    error,
                                    "Copy complete: 0/1 entries; an incomplete file may remain at the destination",
                                )
                            })
                        }
                    }
                },
            )?;
            if let Some(recorder) = recorder {
                recorder.record_copied(&target, kind);
            }
            Ok(())
        }
        FsOperation::Delete { path, .. } => {
            recycle_deleted_target(&path.to_fs_path(root), recorder)
        }
    }
}

fn recycle_deleted_target(
    target: &Path,
    recorder: Option<&mut UndoRecorder>,
) -> Result<(), OpFailure> {
    recycle_with_optional_backup(target, target, recorder)
}

fn recycle_with_optional_backup(
    logical_target: &Path,
    recycle_target: &Path,
    recorder: Option<&mut UndoRecorder>,
) -> Result<(), OpFailure> {
    let Some(recorder) = recorder else {
        return crate::recycle::delete_to_recycle_bin(recycle_target).map_err(OpFailure::from);
    };

    let backup = recorder
        .backup_for_next_step(logical_target)
        .map_err(OpFailure::from)?;
    match crate::recycle::delete_to_recycle_bin(recycle_target) {
        Ok(()) => {
            recorder.record_deleted(logical_target, backup);
            Ok(())
        }
        Err(error) => {
            recorder.discard_backup(&backup);
            Err(OpFailure::from(error))
        }
    }
}

/// Move/Copyの直前preflight: 移動先にエントリが既に存在したら失敗させる。
/// planのvalidate/orderingが正しければ通常は空いているはずで、これは
/// baseline取得後の外部変更(TOCTOU)による黙った上書きへの最終防衛線。
pub(crate) fn ensure_target_vacant(target: &Path) -> Result<(), OpFailure> {
    match fs::symlink_metadata(crate::long_path::to_fs(target)) {
        Ok(_) => Err(OpFailure::from(anyhow!(
            "Another entry already exists at the destination, possibly due to an external change: {}",
            target.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(OpFailure::from(
            anyhow::Error::from(error)
                .context(format!("Failed to check destination: {}", target.display())),
        )),
    }
}

pub(crate) fn move_file_across_volumes(
    source: &Path,
    target: &Path,
    kind: EntryKind,
) -> Result<(), OpFailure> {
    if let Err(error) = copy_single_entry(source, target, kind) {
        let cleanup = match cleanup_incomplete_target(target) {
            Ok(true) => "Incomplete copy destination removed".to_owned(),
            Ok(false) => "Copy destination was not created".to_owned(),
            Err(cleanup_error) => {
                format!("Failed to remove incomplete copy destination: {cleanup_error:#}")
            }
        };
        return Err(OpFailure::with_progress(
            error,
            format!("Copy complete: 0/1 entries; {cleanup}"),
        ));
    }

    remove_non_directory_entry(source, kind).map_err(|error| {
        OpFailure::with_progress(
            error.context(format!(
                "Failed to remove source after copy: {}",
                source.display()
            )),
            "Copy complete: 1/1 entries; failed to remove source",
        )
    })
}

pub(crate) fn move_directory_across_volumes(source: &Path, target: &Path) -> Result<(), OpFailure> {
    let progress = copy_tree(source, target)?;
    fs::remove_dir_all(crate::long_path::to_fs(source))
        .with_context(|| {
            format!(
                "Failed to remove source directory after copy: {}",
                source.display()
            )
        })
        .map_err(|error| {
            OpFailure::with_progress(
                error,
                format!(
                    "Copy complete: {0}/{0} entries; failed to remove source directory",
                    progress.total
                ),
            )
        })
}

pub(crate) fn copy_single_entry(
    source: &Path,
    target: &Path,
    kind: EntryKind,
) -> anyhow::Result<()> {
    match kind {
        EntryKind::File => fs::copy(
            crate::long_path::to_fs(source),
            crate::long_path::to_fs(target),
        )
        .map(|_| ())
        .with_context(|| {
            format!(
                "Failed to copy file: {} → {}",
                source.display(),
                target.display()
            )
        }),
        EntryKind::Symlink => copy_symlink(source, target),
        EntryKind::Dir => bail!(
            "A directory cannot be copied as a single entry: {}",
            source.display()
        ),
    }
}

#[derive(Debug)]
pub(crate) struct CopyTask {
    source: PathBuf,
    target: PathBuf,
    kind: EntryKind,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CopyProgress {
    total: usize,
}

pub(crate) fn copy_tree(source: &Path, target: &Path) -> Result<CopyProgress, OpFailure> {
    let tasks = collect_copy_tasks(source, target).map_err(|error| {
        OpFailure::with_progress(
            error,
            "Copy complete: 0 entries; could not determine total number of entries",
        )
    })?;
    let total = tasks.len();

    fs::create_dir(crate::long_path::to_fs(target))
        .with_context(|| {
            format!(
                "Failed to create destination directory: {}",
                target.display()
            )
        })
        .map_err(|error| OpFailure::with_progress(error, copy_progress(0, total)))?;

    for (copied, task) in tasks.into_iter().enumerate() {
        let result = match task.kind {
            EntryKind::Dir => fs::create_dir(crate::long_path::to_fs(&task.target))
                .with_context(|| {
                    format!(
                        "Failed to create destination directory: {}",
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

pub(crate) fn collect_copy_tasks(source: &Path, target: &Path) -> anyhow::Result<Vec<CopyTask>> {
    let mut tasks = Vec::new();
    let mut directories = vec![(source.to_path_buf(), target.to_path_buf())];

    while let Some((source_dir, target_dir)) = directories.pop() {
        let entries = fs::read_dir(crate::long_path::to_fs(&source_dir)).with_context(|| {
            format!("Failed to read source directory: {}", source_dir.display())
        })?;
        for entry in entries {
            let entry = entry.with_context(|| {
                format!(
                    "Failed to read entry in source directory: {}",
                    source_dir.display()
                )
            })?;
            let child_source = entry.path();
            let child_target = target_dir.join(entry.file_name());
            let metadata = fs::symlink_metadata(crate::long_path::to_fs(&child_source))
                .with_context(|| {
                    format!(
                        "Failed to get copy source metadata: {}",
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
    format!("Copy complete: {copied}/{total} entries")
}

fn copy_symlink(source: &Path, target: &Path) -> anyhow::Result<()> {
    let link_target = fs::read_link(crate::long_path::to_fs(source))
        .with_context(|| format!("Failed to read symlink: {}", source.display()))?;
    create_symlink_like(source, &link_target, target).with_context(|| {
        format!(
            "Failed to copy symlink: {} → {}",
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
                format!(
                    "Failed to inspect incomplete copy destination: {}",
                    target.display()
                )
            });
        }
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        bail!(
            "Existing directory will not be cleaned up: {}",
            target.display()
        );
    }

    remove_non_directory_entry(target, crate::scan::kind_from_metadata(&metadata)).with_context(
        || {
            format!(
                "Failed to remove incomplete copy destination: {}",
                target.display()
            )
        },
    )?;
    Ok(true)
}

pub(crate) fn remove_non_directory_entry(path: &Path, kind: EntryKind) -> anyhow::Result<()> {
    match kind {
        EntryKind::File => fs::remove_file(crate::long_path::to_fs(path))
            .with_context(|| format!("Failed to remove file: {}", path.display())),
        EntryKind::Symlink => remove_symlink(path)
            .with_context(|| format!("Failed to remove symlink: {}", path.display())),
        EntryKind::Dir => bail!(
            "A directory cannot be removed as a single entry: {}",
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
    use fyler_core::undo::{BackupRef, UndoStep};
    use tempfile::tempdir;

    use super::*;

    struct FaultGuard;

    impl FaultGuard {
        fn once(stage: &'static str) -> Self {
            let mut fired = false;
            FAULT_INJECTION.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move |actual, _| {
                    if actual == stage && !fired {
                        fired = true;
                        Some(anyhow!("test fault at {stage}"))
                    } else {
                        None
                    }
                }));
            });
            Self
        }

        fn stages(stages: &'static [&'static str]) -> Self {
            FAULT_INJECTION.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move |actual, _| {
                    stages
                        .contains(&actual)
                        .then(|| anyhow!("test fault at {actual}"))
                }));
            });
            Self
        }
    }

    impl Drop for FaultGuard {
        fn drop(&mut self) {
            FAULT_INJECTION.with(|hook| *hook.borrow_mut() = None);
        }
    }

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

    fn backup_payload_path(backup_dir: &Path, backup: &BackupRef) -> PathBuf {
        backup
            .payload_rel
            .split('/')
            .fold(backup_dir.to_path_buf(), |path, component| {
                path.join(component)
            })
    }

    #[test]
    fn pre_cancelled_plan_skips_every_operation_without_touching_filesystem() {
        let root = tempdir().unwrap();
        let plan = OperationPlan {
            ops: vec![
                FsOperation::Create {
                    path: TreePath::parse("a.txt"),
                    kind: EntryKind::File,
                },
                FsOperation::Create {
                    path: TreePath::parse("b.txt"),
                    kind: EntryKind::File,
                },
            ],
        };
        let cancel = AtomicBool::new(true);
        let mut progress = Vec::new();

        let report = apply_plan_cancellable(
            root.path(),
            &plan,
            &HashSet::new(),
            &cancel,
            &mut |event| progress.push(event),
            None,
        );

        assert!(report.results.iter().all(|result| matches!(
            &result.outcome,
            OpOutcome::Skipped { reason } if reason.contains("Cancelled by user")
        )));
        assert!(!root.path().join("a.txt").exists());
        assert!(!root.path().join("b.txt").exists());
        assert_eq!(
            progress,
            [ApplyProgress {
                completed: 0,
                total: 2,
                current: None,
            }]
        );
    }

    #[test]
    fn cancellation_requested_during_progress_stops_at_next_operation_boundary() {
        let root = tempdir().unwrap();
        let plan = OperationPlan {
            ops: vec![
                FsOperation::Create {
                    path: TreePath::parse("a.txt"),
                    kind: EntryKind::File,
                },
                FsOperation::Create {
                    path: TreePath::parse("b.txt"),
                    kind: EntryKind::File,
                },
                FsOperation::Create {
                    path: TreePath::parse("c.txt"),
                    kind: EntryKind::File,
                },
            ],
        };
        let cancel = AtomicBool::new(false);
        let mut progress = Vec::new();

        let report = apply_plan_cancellable(
            root.path(),
            &plan,
            &HashSet::new(),
            &cancel,
            &mut |event| {
                if event.completed == 0 && event.current.is_some() {
                    cancel.store(true, Ordering::Relaxed);
                }
                progress.push(event);
            },
            None,
        );

        assert!(matches!(report.results[0].outcome, OpOutcome::Success));
        assert!(
            report.results[1..]
                .iter()
                .all(|result| matches!(result.outcome, OpOutcome::Skipped { .. }))
        );
        assert!(root.path().join("a.txt").is_file());
        assert!(!root.path().join("b.txt").exists());
        assert!(!root.path().join("c.txt").exists());
        assert_eq!(
            progress,
            [
                ApplyProgress {
                    completed: 0,
                    total: 3,
                    current: Some(plan.ops[0].clone()),
                },
                ApplyProgress {
                    completed: 1,
                    total: 3,
                    current: None,
                },
            ]
        );
    }

    #[test]
    fn uncancelled_cancellable_apply_matches_overwrite_entrypoint_report() {
        let first_root = tempdir().unwrap();
        let second_root = tempdir().unwrap();
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
        let overwrites = HashSet::new();

        let expected = apply_plan_with_overwrites(first_root.path(), &plan, &overwrites);
        let actual = apply_plan_cancellable(
            second_root.path(),
            &plan,
            &overwrites,
            &AtomicBool::new(false),
            &mut |_| {},
            None,
        );

        assert_eq!(actual, expected);
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
                if error.contains("destination became a directory")
        ));
        assert_eq!(fs::read(root.path().join("src.txt")).unwrap(), b"source");
        assert!(root.path().join("target").is_dir());
    }

    #[test]
    fn approved_create_file_failure_restores_original_target() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("target.txt"), b"original").unwrap();
        let target = TreePath::parse("target.txt");
        let plan = OperationPlan {
            ops: vec![FsOperation::Create {
                path: target.clone(),
                kind: EntryKind::File,
            }],
        };
        let _fault = FaultGuard::once("create_file");

        let report = apply_plan_with_overwrites(root.path(), &plan, &HashSet::from([target]));

        assert!(matches!(
            report.results[0].outcome,
            OpOutcome::Failed { .. }
        ));
        assert_eq!(
            fs::read(root.path().join("target.txt")).unwrap(),
            b"original"
        );
    }

    #[test]
    fn approved_create_dir_failure_restores_original_file_target() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("target"), b"original").unwrap();
        let target = TreePath::parse("target");
        let plan = OperationPlan {
            ops: vec![FsOperation::Create {
                path: target.clone(),
                kind: EntryKind::Dir,
            }],
        };
        let _fault = FaultGuard::once("create_dir");

        let report = apply_plan_with_overwrites(root.path(), &plan, &HashSet::from([target]));

        assert!(matches!(
            report.results[0].outcome,
            OpOutcome::Failed { .. }
        ));
        assert_eq!(fs::read(root.path().join("target")).unwrap(), b"original");
    }

    #[test]
    fn approved_move_failure_restores_source_target_and_discards_undo_backup() {
        let root = tempdir().unwrap();
        let backup = tempdir().unwrap();
        fs::write(root.path().join("source.txt"), b"source").unwrap();
        fs::write(root.path().join("target.txt"), b"original").unwrap();
        let target = TreePath::parse("target.txt");
        let plan = OperationPlan {
            ops: vec![FsOperation::Move {
                id: EntryId(1),
                from: TreePath::parse("source.txt"),
                to: target.clone(),
            }],
        };
        let mut recorder = UndoRecorder::new(
            "tx".to_owned(),
            root.path().to_path_buf(),
            backup.path().to_path_buf(),
        );
        let _fault = FaultGuard::once("rename");

        let report = apply_plan_cancellable(
            root.path(),
            &plan,
            &HashSet::from([target]),
            &AtomicBool::new(false),
            &mut |_| {},
            Some(&mut recorder),
        );
        let transaction = recorder.into_transaction();

        assert!(matches!(
            report.results[0].outcome,
            OpOutcome::Failed { .. }
        ));
        assert_eq!(fs::read(root.path().join("source.txt")).unwrap(), b"source");
        assert_eq!(
            fs::read(root.path().join("target.txt")).unwrap(),
            b"original"
        );
        assert!(transaction.steps.is_empty());
        assert_eq!(transaction.backup_dir, None);
        assert!(!backup.path().join("payload/0/target.txt").exists());
    }

    #[test]
    fn approved_copy_failure_restores_original_without_incomplete_target() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("source.txt"), b"source").unwrap();
        fs::write(root.path().join("target.txt"), b"original").unwrap();
        let target = TreePath::parse("target.txt");
        let plan = OperationPlan {
            ops: vec![FsOperation::Copy {
                src: EntryId(1),
                from: TreePath::parse("source.txt"),
                to: target.clone(),
            }],
        };
        let _fault = FaultGuard::once("copy_single");

        let report = apply_plan_with_overwrites(root.path(), &plan, &HashSet::from([target]));

        assert!(matches!(
            report.results[0].outcome,
            OpOutcome::Failed { .. }
        ));
        assert_eq!(fs::read(root.path().join("source.txt")).unwrap(), b"source");
        assert_eq!(
            fs::read(root.path().join("target.txt")).unwrap(),
            b"original"
        );
    }

    #[test]
    fn restore_failure_reports_staged_path_for_manual_recovery() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("source.txt"), b"source").unwrap();
        fs::write(root.path().join("target.txt"), b"original").unwrap();
        let target = TreePath::parse("target.txt");
        let plan = OperationPlan {
            ops: vec![FsOperation::Move {
                id: EntryId(1),
                from: TreePath::parse("source.txt"),
                to: target.clone(),
            }],
        };
        let _fault = FaultGuard::stages(&["rename", "restore"]);

        let report = apply_plan_with_overwrites(root.path(), &plan, &HashSet::from([target]));

        assert!(matches!(
            &report.results[0].outcome,
            OpOutcome::Failed { progress: Some(progress), .. }
                if progress.contains(".fyler-staged-")
                    && progress.contains("manually")
                    && progress.contains("target.txt")
        ));
        assert_eq!(fs::read(root.path().join("source.txt")).unwrap(), b"source");
        assert!(!root.path().join("target.txt").exists());
        assert!(fs::read_dir(root.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".fyler-staged-")
        }));
    }

    #[test]
    fn trash_commit_failure_keeps_operation_result_and_reports_staged_path() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("source.txt"), b"source").unwrap();
        fs::write(root.path().join("target.txt"), b"original").unwrap();
        let target = TreePath::parse("target.txt");
        let plan = OperationPlan {
            ops: vec![FsOperation::Move {
                id: EntryId(1),
                from: TreePath::parse("source.txt"),
                to: target.clone(),
            }],
        };
        let _fault = FaultGuard::once("commit_to_trash");

        let report = apply_plan_with_overwrites(root.path(), &plan, &HashSet::from([target]));

        assert!(matches!(
            &report.results[0].outcome,
            OpOutcome::Failed { progress: Some(progress), .. }
                if progress.contains("operation completed") && progress.contains(".fyler-staged-")
        ));
        assert!(!root.path().join("source.txt").exists());
        assert_eq!(fs::read(root.path().join("target.txt")).unwrap(), b"source");
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

    #[test]
    fn recorder_records_create_copy_move_delete_steps_in_execution_order() {
        let root = tempdir().unwrap();
        let backup = tempdir().unwrap();
        fs::write(root.path().join("copy-src.txt"), b"copy").unwrap();
        fs::write(root.path().join("move-src.txt"), b"move").unwrap();
        fs::write(root.path().join("delete-me.txt"), b"delete").unwrap();
        let plan = OperationPlan {
            ops: vec![
                FsOperation::Create {
                    path: TreePath::parse("created.txt"),
                    kind: EntryKind::File,
                },
                FsOperation::Copy {
                    src: EntryId(1),
                    from: TreePath::parse("copy-src.txt"),
                    to: TreePath::parse("copied.txt"),
                },
                FsOperation::Move {
                    id: EntryId(2),
                    from: TreePath::parse("move-src.txt"),
                    to: TreePath::parse("moved.txt"),
                },
                FsOperation::Delete {
                    id: EntryId(3),
                    path: TreePath::parse("delete-me.txt"),
                },
            ],
        };
        let mut recorder = UndoRecorder::new(
            "tx".to_owned(),
            root.path().to_path_buf(),
            backup.path().to_path_buf(),
        );

        let report = apply_plan_cancellable(
            root.path(),
            &plan,
            &HashSet::new(),
            &AtomicBool::new(false),
            &mut |_| {},
            Some(&mut recorder),
        );
        let transaction = recorder.into_transaction();

        assert!(report.all_succeeded());
        assert_eq!(transaction.steps.len(), 4);
        assert_eq!(transaction.backup_dir, Some(backup.path().to_path_buf()));
        assert!(matches!(
            &transaction.steps[0],
            UndoStep::RemoveCreated { path, identity: Some(_), post }
                if path == &absolute_path(&root.path().join("created.txt"))
                    && path.is_absolute()
                    && post.kind == EntryKind::File
        ));
        assert!(matches!(
            &transaction.steps[1],
            UndoStep::RemoveCopied { path, identity: Some(_), post, manifest: None }
                if path == &absolute_path(&root.path().join("copied.txt"))
                    && path.is_absolute()
                    && post.kind == EntryKind::File
        ));
        assert!(matches!(
            &transaction.steps[2],
            UndoStep::MoveBack { from, to, identity: Some(_), post, case_only: false }
                if from == &absolute_path(&root.path().join("move-src.txt"))
                    && to == &absolute_path(&root.path().join("moved.txt"))
                    && from.is_absolute()
                    && to.is_absolute()
                    && post.kind == EntryKind::File
        ));
        let UndoStep::RestoreDeleted {
            path,
            backup: reference,
        } = &transaction.steps[3]
        else {
            panic!("delete step should record RestoreDeleted");
        };
        assert_eq!(path, &absolute_path(&root.path().join("delete-me.txt")));
        assert!(path.is_absolute());
        assert_eq!(reference.kind, EntryKind::File);
        assert_eq!(
            fs::read(backup_payload_path(backup.path(), reference)).unwrap(),
            b"delete"
        );
    }

    #[test]
    fn recorder_records_directory_copy_manifest() {
        let root = tempdir().unwrap();
        let backup = tempdir().unwrap();
        fs::create_dir(root.path().join("dir")).unwrap();
        fs::create_dir(root.path().join("dir/nested")).unwrap();
        fs::write(root.path().join("dir/nested/file.txt"), b"nested").unwrap();
        let plan = OperationPlan {
            ops: vec![FsOperation::Copy {
                src: EntryId(1),
                from: TreePath::parse("dir"),
                to: TreePath::parse("dir-copy"),
            }],
        };
        let mut recorder = UndoRecorder::new(
            "tx".to_owned(),
            root.path().to_path_buf(),
            backup.path().to_path_buf(),
        );

        let report = apply_plan_cancellable(
            root.path(),
            &plan,
            &HashSet::new(),
            &AtomicBool::new(false),
            &mut |_| {},
            Some(&mut recorder),
        );
        let transaction = recorder.into_transaction();

        assert!(report.all_succeeded());
        assert!(matches!(
            &transaction.steps[0],
            UndoStep::RemoveCopied { manifest: Some(manifest), post, .. }
                if post.kind == EntryKind::Dir
                    && manifest.iter().any(|entry| entry.rel_path == "nested/file.txt")
        ));
    }

    #[test]
    fn recorder_delete_backup_failure_leaves_source_untouched() {
        let root = tempdir().unwrap();
        let backup = tempdir().unwrap();
        fs::write(root.path().join("victim.txt"), b"victim").unwrap();
        fs::write(backup.path().join("payload"), b"not a directory").unwrap();
        let plan = OperationPlan {
            ops: vec![FsOperation::Delete {
                id: EntryId(1),
                path: TreePath::parse("victim.txt"),
            }],
        };
        let mut recorder = UndoRecorder::new(
            "tx".to_owned(),
            root.path().to_path_buf(),
            backup.path().to_path_buf(),
        );

        let report = apply_plan_cancellable(
            root.path(),
            &plan,
            &HashSet::new(),
            &AtomicBool::new(false),
            &mut |_| {},
            Some(&mut recorder),
        );
        let transaction = recorder.into_transaction();

        assert!(matches!(
            report.results[0].outcome,
            OpOutcome::Failed { .. }
        ));
        assert_eq!(fs::read(root.path().join("victim.txt")).unwrap(), b"victim");
        assert!(transaction.steps.is_empty());
        assert_eq!(transaction.backup_dir, None);
    }

    #[test]
    fn recorder_records_overwrite_backup_before_move_step() {
        let root = tempdir().unwrap();
        let backup = tempdir().unwrap();
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
        let mut recorder = UndoRecorder::new(
            "tx".to_owned(),
            root.path().to_path_buf(),
            backup.path().to_path_buf(),
        );

        let report = apply_plan_cancellable(
            root.path(),
            &plan,
            &overwrites,
            &AtomicBool::new(false),
            &mut |_| {},
            Some(&mut recorder),
        );
        let transaction = recorder.into_transaction();

        assert!(report.all_succeeded());
        assert_eq!(transaction.steps.len(), 2);
        let UndoStep::RestoreOverwritten {
            path,
            backup: reference,
        } = &transaction.steps[0]
        else {
            panic!("overwrite backup should be recorded first");
        };
        assert_eq!(path, &absolute_path(&root.path().join("target.txt")));
        assert_eq!(
            fs::read(backup_payload_path(backup.path(), reference)).unwrap(),
            b"existing"
        );
        assert!(matches!(
            &transaction.steps[1],
            UndoStep::MoveBack { from, to, .. }
                if from == &absolute_path(&root.path().join("src.txt"))
                    && to == &absolute_path(&root.path().join("target.txt"))
        ));
    }

    #[test]
    fn recorder_keeps_only_successful_steps_after_later_failure() {
        let root = tempdir().unwrap();
        let backup = tempdir().unwrap();
        let plan = OperationPlan {
            ops: vec![
                FsOperation::Create {
                    path: TreePath::parse("created.txt"),
                    kind: EntryKind::File,
                },
                FsOperation::Create {
                    path: TreePath::parse("missing-parent/child.txt"),
                    kind: EntryKind::File,
                },
            ],
        };
        let mut recorder = UndoRecorder::new(
            "tx".to_owned(),
            root.path().to_path_buf(),
            backup.path().to_path_buf(),
        );

        let report = apply_plan_cancellable(
            root.path(),
            &plan,
            &HashSet::new(),
            &AtomicBool::new(false),
            &mut |_| {},
            Some(&mut recorder),
        );
        let transaction = recorder.into_transaction();

        assert!(matches!(report.results[0].outcome, OpOutcome::Success));
        assert!(matches!(
            report.results[1].outcome,
            OpOutcome::Failed { .. }
        ));
        assert_eq!(transaction.steps.len(), 1);
        assert!(matches!(
            &transaction.steps[0],
            UndoStep::RemoveCreated { path, .. }
                if path == &absolute_path(&root.path().join("created.txt"))
        ));
    }

    #[test]
    fn recorder_keeps_only_executed_steps_after_cancellation() {
        let root = tempdir().unwrap();
        let backup = tempdir().unwrap();
        let plan = OperationPlan {
            ops: vec![
                FsOperation::Create {
                    path: TreePath::parse("a.txt"),
                    kind: EntryKind::File,
                },
                FsOperation::Create {
                    path: TreePath::parse("b.txt"),
                    kind: EntryKind::File,
                },
            ],
        };
        let cancel = AtomicBool::new(false);
        let mut recorder = UndoRecorder::new(
            "tx".to_owned(),
            root.path().to_path_buf(),
            backup.path().to_path_buf(),
        );

        let report = apply_plan_cancellable(
            root.path(),
            &plan,
            &HashSet::new(),
            &cancel,
            &mut |event| {
                if event.completed == 0 && event.current.is_some() {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
            Some(&mut recorder),
        );
        let transaction = recorder.into_transaction();

        assert!(matches!(report.results[0].outcome, OpOutcome::Success));
        assert!(matches!(
            report.results[1].outcome,
            OpOutcome::Skipped { .. }
        ));
        assert_eq!(transaction.steps.len(), 1);
        assert!(matches!(
            &transaction.steps[0],
            UndoStep::RemoveCreated { path, .. }
                if path == &absolute_path(&root.path().join("a.txt"))
        ));
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
    fn transfer_applies_same_volume_move_and_copy() {
        let root = tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(source.join("move.txt"), b"move").unwrap();
        fs::write(source.join("copy.txt"), b"copy").unwrap();
        let plan = transfer_plan(
            &source,
            &target,
            vec![
                transfer_op(TransferKind::Move, "move.txt", "moved.txt", EntryKind::File),
                transfer_op(
                    TransferKind::Copy,
                    "copy.txt",
                    "copied.txt",
                    EntryKind::File,
                ),
            ],
        );
        let cancel = AtomicBool::new(false);

        let report = apply_transfer_plan_cancellable(&plan, &HashSet::new(), &cancel, &mut |_| {});

        assert!(report.all_succeeded());
        assert_eq!(report.results.len(), plan.ops.len());
        assert!(!source.join("move.txt").exists());
        assert_eq!(fs::read(target.join("moved.txt")).unwrap(), b"move");
        assert_eq!(fs::read(source.join("copy.txt")).unwrap(), b"copy");
        assert_eq!(fs::read(target.join("copied.txt")).unwrap(), b"copy");
    }

    #[test]
    fn transfer_report_preserves_partial_failure_in_plan_order() {
        let root = tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(source.join("present.txt"), b"present").unwrap();
        let plan = transfer_plan(
            &source,
            &target,
            vec![
                transfer_op(
                    TransferKind::Copy,
                    "present.txt",
                    "copied.txt",
                    EntryKind::File,
                ),
                transfer_op(
                    TransferKind::Copy,
                    "missing.txt",
                    "missing-copy.txt",
                    EntryKind::File,
                ),
            ],
        );
        let cancel = AtomicBool::new(false);

        let report = apply_transfer_plan_cancellable(&plan, &HashSet::new(), &cancel, &mut |_| {});

        assert_eq!(report.results.len(), plan.ops.len());
        assert_eq!(report.results[0].op, plan.ops[0]);
        assert_eq!(report.results[1].op, plan.ops[1]);
        assert!(matches!(report.results[0].outcome, OpOutcome::Success));
        assert!(matches!(
            report.results[1].outcome,
            OpOutcome::Failed { .. }
        ));
        assert_eq!(fs::read(target.join("copied.txt")).unwrap(), b"present");
    }

    #[test]
    fn transfer_only_recycles_preflight_approved_file_overwrite() {
        let root = tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(source.join("new.txt"), b"new").unwrap();
        fs::write(target.join("occupied.txt"), b"old").unwrap();
        let plan = transfer_plan(
            &source,
            &target,
            vec![transfer_op(
                TransferKind::Copy,
                "new.txt",
                "occupied.txt",
                EntryKind::File,
            )],
        );
        let cancel = AtomicBool::new(false);
        let approved = HashSet::from([absolute_path(&target.join("occupied.txt"))]);

        let report = apply_transfer_plan_cancellable(&plan, &approved, &cancel, &mut |_| {});

        assert!(report.all_succeeded());
        assert_eq!(fs::read(target.join("occupied.txt")).unwrap(), b"new");
        assert_eq!(fs::read(source.join("new.txt")).unwrap(), b"new");
    }

    #[test]
    fn transfer_move_failure_restores_source_and_original_target() {
        let root = tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(source.join("new.txt"), b"new").unwrap();
        fs::write(target.join("occupied.txt"), b"old").unwrap();
        let plan = transfer_plan(
            &source,
            &target,
            vec![transfer_op(
                TransferKind::Move,
                "new.txt",
                "occupied.txt",
                EntryKind::File,
            )],
        );
        let approved = HashSet::from([absolute_path(&target.join("occupied.txt"))]);
        let _fault = FaultGuard::once("transfer_move");

        let report =
            apply_transfer_plan_cancellable(&plan, &approved, &AtomicBool::new(false), &mut |_| {});

        assert!(matches!(
            report.results[0].outcome,
            OpOutcome::Failed { .. }
        ));
        assert_eq!(fs::read(source.join("new.txt")).unwrap(), b"new");
        assert_eq!(fs::read(target.join("occupied.txt")).unwrap(), b"old");
    }

    #[test]
    fn transfer_copy_failure_restores_original_target() {
        let root = tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(source.join("new.txt"), b"new").unwrap();
        fs::write(target.join("occupied.txt"), b"old").unwrap();
        let plan = transfer_plan(
            &source,
            &target,
            vec![transfer_op(
                TransferKind::Copy,
                "new.txt",
                "occupied.txt",
                EntryKind::File,
            )],
        );
        let approved = HashSet::from([absolute_path(&target.join("occupied.txt"))]);
        let _fault = FaultGuard::once("transfer_copy");

        let report =
            apply_transfer_plan_cancellable(&plan, &approved, &AtomicBool::new(false), &mut |_| {});

        assert!(matches!(
            report.results[0].outcome,
            OpOutcome::Failed { .. }
        ));
        assert_eq!(fs::read(source.join("new.txt")).unwrap(), b"new");
        assert_eq!(fs::read(target.join("occupied.txt")).unwrap(), b"old");
    }

    #[test]
    fn transfer_toctou_guard_preserves_unapproved_target() {
        let root = tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(source.join("new.txt"), b"new").unwrap();
        fs::write(target.join("occupied.txt"), b"old").unwrap();
        let plan = transfer_plan(
            &source,
            &target,
            vec![transfer_op(
                TransferKind::Move,
                "new.txt",
                "occupied.txt",
                EntryKind::File,
            )],
        );
        let cancel = AtomicBool::new(false);

        let report = apply_transfer_plan_cancellable(&plan, &HashSet::new(), &cancel, &mut |_| {});

        assert!(matches!(
            report.results[0].outcome,
            OpOutcome::Failed { .. }
        ));
        assert_eq!(fs::read(target.join("occupied.txt")).unwrap(), b"old");
        assert_eq!(fs::read(source.join("new.txt")).unwrap(), b"new");
    }

    #[test]
    fn transfer_approved_overwrite_stops_if_target_became_directory() {
        let root = tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(source.join("new.txt"), b"new").unwrap();
        fs::create_dir(target.join("occupied")).unwrap();
        fs::write(target.join("occupied/child.txt"), b"keep").unwrap();
        let plan = transfer_plan(
            &source,
            &target,
            vec![transfer_op(
                TransferKind::Copy,
                "new.txt",
                "occupied",
                EntryKind::File,
            )],
        );
        let cancel = AtomicBool::new(false);
        let approved = HashSet::from([absolute_path(&target.join("occupied"))]);

        let report = apply_transfer_plan_cancellable(&plan, &approved, &cancel, &mut |_| {});

        assert!(matches!(
            report.results[0].outcome,
            OpOutcome::Failed { .. }
        ));
        assert_eq!(
            fs::read(target.join("occupied/child.txt")).unwrap(),
            b"keep"
        );
    }

    #[test]
    fn transfer_cancel_skips_remaining_operations() {
        let root = tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&target).unwrap();
        fs::write(source.join("first.txt"), b"first").unwrap();
        fs::write(source.join("second.txt"), b"second").unwrap();
        let plan = transfer_plan(
            &source,
            &target,
            vec![
                transfer_op(
                    TransferKind::Copy,
                    "first.txt",
                    "first.txt",
                    EntryKind::File,
                ),
                transfer_op(
                    TransferKind::Copy,
                    "second.txt",
                    "second.txt",
                    EntryKind::File,
                ),
            ],
        );
        let cancel = AtomicBool::new(false);

        let report =
            apply_transfer_plan_cancellable(&plan, &HashSet::new(), &cancel, &mut |progress| {
                if progress.current.is_some() {
                    cancel.store(true, Ordering::Relaxed);
                }
            });

        assert!(matches!(report.results[0].outcome, OpOutcome::Success));
        assert!(matches!(
            report.results[1].outcome,
            OpOutcome::Skipped { .. }
        ));
        assert!(target.join("first.txt").exists());
        assert!(!target.join("second.txt").exists());
    }
}
