//! forward apply 1回分のundo receipt recorderとundo実行系。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use fyler_core::tree::EntryKind;
use fyler_core::undo::{
    BackupRef, FileIdentity, Fingerprint, ManifestEntry, UndoStep, UndoStepStatus, UndoTransaction,
};
use fyler_core::{
    report::{ApplyProgress, CommitReport, OpOutcome, OpResult},
    undo::UndoStepStatus::{Ready, Rejected},
};

use crate::classify::MoveClass;

/// forward apply 1回分のreceiptを実行順に蓄積する。
pub struct UndoRecorder {
    id: String,
    root: PathBuf,
    backup_dir: PathBuf,
    steps: Vec<UndoStep>,
    used_backup: bool,
}

impl UndoRecorder {
    /// backup_dir はapp層が確保済みのtransaction dir(存在保証は呼び出し側)。
    pub fn new(id: String, root: PathBuf, backup_dir: PathBuf) -> Self {
        Self {
            id,
            root: absolute_path(&root),
            backup_dir,
            steps: Vec::new(),
            used_backup: false,
        }
    }

    /// 蓄積したstepsから [`UndoTransaction`] を組み立てる。backup未使用ならbackup_dir=None。
    pub fn into_transaction(self) -> UndoTransaction {
        UndoTransaction {
            id: self.id,
            root: self.root,
            steps: self.steps,
            backup_dir: self.used_backup.then_some(self.backup_dir),
        }
    }

    pub(crate) fn backup_for_next_step(&self, source: &Path) -> anyhow::Result<BackupRef> {
        crate::backup::backup_entry(source, &self.backup_dir, self.steps.len())
    }

    pub(crate) fn discard_backup(&self, backup: &BackupRef) {
        crate::backup::discard_backup_payload(&self.backup_dir, backup);
    }

    pub(crate) fn record_created(&mut self, path: &Path, kind: EntryKind) {
        let path = absolute_path(path);
        let (identity, post) = capture_post(&path, kind);
        self.steps.push(UndoStep::RemoveCreated {
            path,
            identity,
            post,
        });
    }

    pub(crate) fn record_copied(&mut self, path: &Path, kind: EntryKind) {
        let path = absolute_path(path);
        let (identity, post) = capture_post(&path, kind);
        let manifest = (kind == EntryKind::Dir)
            .then(|| crate::identity::capture_manifest(&path).ok())
            .flatten();
        self.steps.push(UndoStep::RemoveCopied {
            path,
            identity,
            post,
            manifest,
        });
    }

    pub(crate) fn record_moved(
        &mut self,
        from: &Path,
        to: &Path,
        kind: EntryKind,
        case_only: bool,
    ) {
        let from = absolute_path(from);
        let to = absolute_path(to);
        let (identity, post) = capture_post(&to, kind);
        self.steps.push(UndoStep::MoveBack {
            from,
            to,
            identity,
            post,
            case_only,
        });
    }

    pub(crate) fn record_deleted(&mut self, path: &Path, backup: BackupRef) {
        self.used_backup = true;
        self.steps.push(UndoStep::RestoreDeleted {
            path: absolute_path(path),
            backup,
        });
    }

    pub(crate) fn record_overwritten(&mut self, path: &Path, backup: BackupRef) {
        self.used_backup = true;
        self.steps.push(UndoStep::RestoreOverwritten {
            path: absolute_path(path),
            backup,
        });
    }
}

fn capture_post(path: &Path, fallback_kind: EntryKind) -> (Option<FileIdentity>, Fingerprint) {
    let identity = crate::identity::capture_identity(path).ok();
    let post = crate::identity::capture_fingerprint(path).unwrap_or(Fingerprint {
        kind: fallback_kind,
        size: None,
        mtime: None,
        link_target: None,
    });
    (identity, post)
}

fn absolute_path(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// undo確認ダイアログ表示用に、transactionの各stepが現在undo可能かをread-onlyで検査する。
///
/// 実FSへは一切書き込まない。`steps` と同順・同数で [`UndoStepStatus`] を返す。
/// 判定基準は M12 の stale検知契約で、実行時にも同じ検証を各step直前に再実施する。
pub fn preflight_undo(transaction: &UndoTransaction) -> Vec<UndoStepStatus> {
    transaction
        .steps
        .iter()
        .map(|step| match validate_undo_step(transaction, step) {
            Ok(()) => Ready,
            Err(reason) => Rejected { reason },
        })
        .collect()
}

/// 承認済みtransactionを逆順に実行する。
///
/// 呼び出し契約: 保存状態機械の `ApplyingUndo`
/// (= undo確認ダイアログ承認後)からのみ呼ぶこと。`steps` は末尾から処理し、
/// [`CommitReport`] の `results` は実行順(= `steps` の逆順)で返す。
///
/// 各stepは実行直前に stale検証を再実施し、拒否時は該当実体へ触れず
/// [`OpOutcome::Failed`] として報告する。キャンセルはstep間でのみ反映し、残りは
/// [`OpOutcome::Skipped`] とする。ごみ箱送りは [`crate::recycle`]、復元は
/// [`crate::backup::restore_entry`] を経由し、FS API直前のパス変換は
/// [`crate::long_path::to_fs`] に閉じ込める。
pub fn apply_undo_cancellable(
    transaction: &UndoTransaction,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(ApplyProgress<UndoStep>),
) -> CommitReport<UndoStep> {
    let execution_steps = transaction.steps.iter().rev().cloned().collect::<Vec<_>>();
    let total = execution_steps.len();
    let mut results = Vec::with_capacity(total);
    let mut attempted = 0;

    for (index, step) in execution_steps.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            results.extend(execution_steps[index..].iter().cloned().map(|op| OpResult {
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
            current: Some(step.clone()),
        });
        attempted += 1;

        let outcome = match execute_undo_step(transaction, step) {
            Ok(()) => OpOutcome::Success,
            Err(error) => OpOutcome::Failed {
                error,
                progress: None,
            },
        };
        results.push(OpResult {
            op: step.clone(),
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

fn execute_undo_step(transaction: &UndoTransaction, step: &UndoStep) -> Result<(), String> {
    validate_undo_step(transaction, step)?;
    match step {
        UndoStep::RemoveCreated { path, .. } | UndoStep::RemoveCopied { path, .. } => {
            crate::recycle::delete_to_recycle_bin(&crate::long_path::to_fs(path)).map_err(|error| {
                format!("Failed to move undo target to the recycle bin: {error:#}")
            })
        }
        UndoStep::MoveBack {
            from,
            to,
            post,
            case_only,
            ..
        } => execute_move_back(from, to, post.kind, *case_only),
        UndoStep::RestoreDeleted { path, backup }
        | UndoStep::RestoreOverwritten { path, backup } => {
            let backup_dir = transaction
                .backup_dir
                .as_deref()
                .ok_or_else(|| "Backup directory was not recorded".to_owned())?;
            crate::backup::restore_entry(backup_dir, backup, path).map_err(|error| {
                format!("Failed to restore from backup payload: {error:#}; the backup payload remains available")
            })
        }
    }
}

fn execute_move_back(
    from: &Path,
    to: &Path,
    kind: EntryKind,
    case_only: bool,
) -> Result<(), String> {
    let case_sensitive_directory = case_only
        && from
            .parent()
            .is_some_and(|parent| crate::case::dir_is_case_sensitive(parent).unwrap_or(false));
    if case_only && !case_sensitive_directory {
        return crate::case::case_only_rename(to, from)
            .map_err(|error| format!("Failed to reverse case-only rename: {error:#}"));
    }

    match crate::classify::classify_move(to, from, kind)
        .map_err(|error| format!("Failed to classify volume for undo move: {error:#}"))?
    {
        MoveClass::SameVolumeRename => {
            fs::rename(crate::long_path::to_fs(to), crate::long_path::to_fs(from)).map_err(
                |error| {
                    format!(
                        "Failed to reverse rename: {} → {}: {error}",
                        to.display(),
                        from.display()
                    )
                },
            )
        }
        MoveClass::CrossVolumeFileMove => {
            crate::apply::move_file_across_volumes(to, from, kind).map_err(|failure| failure.error)
        }
        MoveClass::CrossVolumeDirectoryMove => {
            crate::apply::move_directory_across_volumes(to, from).map_err(|failure| failure.error)
        }
    }
}

/// MoveBackのundoで移動先(from)の空き確認が必要かを判定する純ロジック。
///
/// case-only rename を case-insensitive ディレクトリで戻す場合だけ、from と to が
/// 同一実体を指すため空き確認をスキップする(常に「占有」と誤検出するため)。
/// case-sensitiveディレクトリでは大文字小文字違いは別エントリなので確認が必要。
fn move_back_requires_vacancy(
    case_only: bool,
    dir_is_case_sensitive: impl FnOnce() -> bool,
) -> bool {
    !case_only || dir_is_case_sensitive()
}

fn validate_undo_step(transaction: &UndoTransaction, step: &UndoStep) -> Result<(), String> {
    match step {
        UndoStep::RemoveCreated {
            path,
            identity,
            post,
        } => validate_remove_created(path, identity.as_ref(), post),
        UndoStep::RemoveCopied {
            path,
            identity,
            post,
            manifest,
        } => validate_remove_copied(path, identity.as_ref(), post, manifest.as_deref()),
        UndoStep::MoveBack {
            from,
            to,
            identity,
            post,
            case_only,
        } => validate_move_back(from, to, identity.as_ref(), post, *case_only),
        UndoStep::RestoreDeleted { path, backup }
        | UndoStep::RestoreOverwritten { path, backup } => {
            validate_restore(transaction, path, backup)
        }
    }
}

fn validate_remove_created(
    path: &Path,
    identity: Option<&FileIdentity>,
    post: &Fingerprint,
) -> Result<(), String> {
    match post.kind {
        EntryKind::File => {
            let current = capture_current(path, identity, "Created file was not found")?;
            ensure_identity_matches(
                identity,
                current.identity.as_ref(),
                "Replaced by a different object after creation",
            )?;
            ensure_file_fingerprint_matches(
                post,
                &current.fingerprint,
                "Content changed after creation",
            )
        }
        EntryKind::Dir => {
            let current = capture_current(path, identity, "Created directory was not found")?;
            ensure_identity_matches(
                identity,
                current.identity.as_ref(),
                "Replaced by a different object after creation",
            )?;
            ensure_kind_matches(
                post.kind,
                current.fingerprint.kind,
                "Type changed after creation",
            )?;
            ensure_directory_empty(path)
        }
        EntryKind::Symlink => {
            let current = capture_current(path, identity, "Created symlink was not found")?;
            ensure_identity_matches(
                identity,
                current.identity.as_ref(),
                "Replaced by a different object after creation",
            )?;
            ensure_symlink_fingerprint_matches(
                post,
                &current.fingerprint,
                "Link target changed after creation",
            )
        }
    }
}

fn validate_remove_copied(
    path: &Path,
    identity: Option<&FileIdentity>,
    post: &Fingerprint,
    manifest: Option<&[ManifestEntry]>,
) -> Result<(), String> {
    match post.kind {
        EntryKind::File => {
            let current = capture_current(path, identity, "Copied file was not found")?;
            ensure_identity_matches(
                identity,
                current.identity.as_ref(),
                "Replaced by a different object after copy",
            )?;
            ensure_file_fingerprint_matches(
                post,
                &current.fingerprint,
                "Content changed after copy",
            )
        }
        EntryKind::Dir => {
            let expected_manifest = manifest
                .ok_or_else(|| "Manifest for copied directory was not recorded".to_owned())?;
            let current = capture_current(path, identity, "Copied directory was not found")?;
            ensure_identity_matches(
                identity,
                current.identity.as_ref(),
                "Replaced by a different object after copy",
            )?;
            ensure_kind_matches(
                post.kind,
                current.fingerprint.kind,
                "Type changed after copy",
            )?;
            let current_manifest = crate::identity::capture_manifest(path).map_err(|error| {
                format!("Failed to capture manifest for copied directory: {error:#}")
            })?;
            if current_manifest == expected_manifest {
                Ok(())
            } else {
                Err("Directory contents changed after copy".to_owned())
            }
        }
        EntryKind::Symlink => {
            let current = capture_current(path, identity, "Copied symlink was not found")?;
            ensure_identity_matches(
                identity,
                current.identity.as_ref(),
                "Replaced by a different object after copy",
            )?;
            ensure_symlink_fingerprint_matches(
                post,
                &current.fingerprint,
                "Link target changed after copy",
            )
        }
    }
}

fn validate_move_back(
    from: &Path,
    to: &Path,
    identity: Option<&FileIdentity>,
    post: &Fingerprint,
    case_only: bool,
) -> Result<(), String> {
    // case-insensitiveディレクトリでのcase-only renameは from と to が同一実体を
    // 指すため、fromの存在確認は必ず「占有」になる(forward側 apply.rs の
    // case_only分岐と同じ理由)。この場合だけ空き確認をスキップする。
    if move_back_requires_vacancy(case_only, || {
        from.parent()
            .is_some_and(|parent| crate::case::dir_is_case_sensitive(parent).unwrap_or(false))
    }) {
        ensure_path_vacant(from, "Another entry exists at the original location")?;
    }
    let current = capture_current(to, identity, "Target to restore was not found")?;
    ensure_identity_matches(
        identity,
        current.identity.as_ref(),
        "Replaced by a different object after move",
    )?;
    match post.kind {
        EntryKind::File => ensure_file_fingerprint_matches(
            post,
            &current.fingerprint,
            "Content changed after move",
        ),
        EntryKind::Dir => ensure_kind_matches(
            post.kind,
            current.fingerprint.kind,
            "Type changed after move",
        ),
        EntryKind::Symlink => ensure_symlink_fingerprint_matches(
            post,
            &current.fingerprint,
            "Link target changed after move",
        ),
    }
}

fn validate_restore(
    transaction: &UndoTransaction,
    path: &Path,
    backup: &BackupRef,
) -> Result<(), String> {
    ensure_path_vacant(path, "Another entry exists at the restore destination")?;
    let backup_dir = transaction
        .backup_dir
        .as_deref()
        .ok_or_else(|| "Backup directory was not recorded".to_owned())?;
    let payload = backup_payload_path(backup_dir, backup)?;
    let metadata = fs::symlink_metadata(crate::long_path::to_fs(&payload)).map_err(|error| {
        match error.kind() {
            std::io::ErrorKind::NotFound => "Backup payload was not found".to_owned(),
            _ => format!("Failed to inspect backup payload: {error}"),
        }
    })?;
    let actual_kind = crate::scan::kind_from_metadata(&metadata);
    if actual_kind == backup.kind {
        Ok(())
    } else {
        Err("Backup payload type does not match the record".to_owned())
    }
}

#[derive(Debug)]
struct CurrentEntry {
    identity: Option<FileIdentity>,
    fingerprint: Fingerprint,
}

fn capture_current(
    path: &Path,
    expected_identity: Option<&FileIdentity>,
    not_found_reason: &str,
) -> Result<CurrentEntry, String> {
    let fingerprint = crate::identity::capture_fingerprint(path).map_err(|error| {
        if metadata_missing(path) {
            not_found_reason.to_owned()
        } else {
            format!("Failed to capture current fingerprint: {error:#}")
        }
    })?;
    let identity = if expected_identity.is_some() {
        Some(
            crate::identity::capture_identity(path)
                .map_err(|error| format!("Failed to capture current object identity: {error:#}"))?,
        )
    } else {
        None
    };
    Ok(CurrentEntry {
        identity,
        fingerprint,
    })
}

fn ensure_identity_matches(
    expected: Option<&FileIdentity>,
    current: Option<&FileIdentity>,
    reason: &str,
) -> Result<(), String> {
    match expected {
        Some(expected) if Some(expected) == current => Ok(()),
        Some(_) => Err(reason.to_owned()),
        None => Ok(()),
    }
}

fn ensure_file_fingerprint_matches(
    expected: &Fingerprint,
    current: &Fingerprint,
    reason: &str,
) -> Result<(), String> {
    ensure_kind_matches(expected.kind, current.kind, "Type has changed")?;
    if current.size == expected.size && current.mtime == expected.mtime {
        Ok(())
    } else {
        Err(reason.to_owned())
    }
}

fn ensure_symlink_fingerprint_matches(
    expected: &Fingerprint,
    current: &Fingerprint,
    reason: &str,
) -> Result<(), String> {
    ensure_kind_matches(expected.kind, current.kind, "Type has changed")?;
    if current.link_target == expected.link_target {
        Ok(())
    } else {
        Err(reason.to_owned())
    }
}

fn ensure_kind_matches(
    expected: EntryKind,
    current: EntryKind,
    reason: &str,
) -> Result<(), String> {
    if current == expected {
        Ok(())
    } else {
        Err(reason.to_owned())
    }
}

fn ensure_directory_empty(path: &Path) -> Result<(), String> {
    let mut entries = fs::read_dir(crate::long_path::to_fs(path))
        .map_err(|error| format!("Failed to enumerate created directory: {error}"))?;
    match entries.next() {
        None => Ok(()),
        Some(Ok(_)) => Err("Directory contents changed after creation".to_owned()),
        Some(Err(error)) => Err(format!(
            "Failed to inspect entry in created directory: {error}"
        )),
    }
}

fn ensure_path_vacant(path: &Path, occupied_reason: &str) -> Result<(), String> {
    match fs::symlink_metadata(crate::long_path::to_fs(path)) {
        Ok(_) => Err(occupied_reason.to_owned()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Failed to check whether path exists: {error}")),
    }
}

fn metadata_missing(path: &Path) -> bool {
    matches!(
        fs::symlink_metadata(crate::long_path::to_fs(path)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    )
}

fn backup_payload_path(backup_dir: &Path, backup: &BackupRef) -> Result<PathBuf, String> {
    let mut path = backup_dir.to_path_buf();
    for component in backup.payload_rel.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(format!(
                "Invalid backup payload relative path: {}",
                backup.payload_rel
            ));
        }
        path.push(component);
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};

    use fyler_core::id::EntryId;
    use fyler_core::path::TreePath;
    use fyler_core::plan::{FsOperation, OperationPlan};
    use fyler_core::report::{ApplyProgress, OpOutcome};
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn transaction_omits_backup_dir_until_backup_step_is_recorded() {
        let root = tempdir().unwrap();
        let backup = tempdir().unwrap();
        let mut recorder = UndoRecorder::new(
            "tx".to_owned(),
            root.path().to_path_buf(),
            backup.path().to_path_buf(),
        );
        let created = root.path().join("created.txt");
        fs::write(&created, b"").unwrap();

        recorder.record_created(&created, EntryKind::File);
        let transaction = recorder.into_transaction();

        assert_eq!(transaction.id, "tx");
        assert_eq!(transaction.root, absolute_path(root.path()));
        assert_eq!(transaction.backup_dir, None);
        assert_eq!(transaction.steps.len(), 1);
    }

    #[test]
    fn backup_recording_marks_transaction_as_payload_backed() {
        let root = tempdir().unwrap();
        let backup = tempdir().unwrap();
        let mut recorder = UndoRecorder::new(
            "tx".to_owned(),
            root.path().to_path_buf(),
            backup.path().to_path_buf(),
        );
        let deleted = root.path().join("deleted.txt");
        fs::write(&deleted, b"delete").unwrap();
        let reference = recorder.backup_for_next_step(&deleted).unwrap();

        recorder.record_deleted(&deleted, reference);
        let transaction = recorder.into_transaction();

        assert_eq!(transaction.backup_dir, Some(backup.path().to_path_buf()));
        assert!(matches!(
            &transaction.steps[0],
            UndoStep::RestoreDeleted { path, backup }
                if path == &absolute_path(&deleted) && backup.payload_rel == "payload/0/deleted.txt"
        ));
    }

    fn apply_with_recorder(
        root: &Path,
        backup: &Path,
        ops: Vec<FsOperation>,
        overwrites: HashSet<TreePath>,
    ) -> UndoTransaction {
        let plan = OperationPlan { ops };
        let mut recorder =
            UndoRecorder::new("tx".to_owned(), root.to_path_buf(), backup.to_path_buf());
        let report = crate::apply::apply_plan_cancellable(
            root,
            &plan,
            &overwrites,
            &AtomicBool::new(false),
            &mut |_| {},
            Some(&mut recorder),
        );
        assert!(report.all_succeeded(), "{report:#?}");
        recorder.into_transaction()
    }

    fn apply_undo(transaction: &UndoTransaction) -> CommitReport<UndoStep> {
        apply_undo_cancellable(transaction, &AtomicBool::new(false), &mut |_| {})
    }

    fn assert_success(report: &CommitReport<UndoStep>) {
        assert!(report.all_succeeded(), "{report:#?}");
    }

    fn assert_failed_contains(outcome: &OpOutcome, needle: &str) {
        assert!(
            matches!(
                outcome,
                OpOutcome::Failed { error, progress: None } if error.contains(needle)
            ),
            "{outcome:#?}"
        );
    }

    fn relative_entry_snapshot(root: &Path) -> Vec<(String, EntryKind, Vec<u8>)> {
        let mut entries = Vec::new();
        collect_snapshot(root, root, &mut entries);
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        entries
    }

    fn collect_snapshot(
        root: &Path,
        directory: &Path,
        entries: &mut Vec<(String, EntryKind, Vec<u8>)>,
    ) {
        let mut children = fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        children.sort();
        for path in children {
            let metadata = fs::symlink_metadata(&path).unwrap();
            let kind = crate::scan::kind_from_metadata(&metadata);
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .components()
                .map(|component| component.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            let payload = match kind {
                EntryKind::File => fs::read(&path).unwrap(),
                EntryKind::Symlink => fs::read_link(&path)
                    .unwrap()
                    .to_string_lossy()
                    .as_bytes()
                    .to_vec(),
                EntryKind::Dir => Vec::new(),
            };
            entries.push((rel, kind, payload));
            if kind == EntryKind::Dir {
                collect_snapshot(root, &path, entries);
            }
        }
    }

    #[test]
    fn undo_create_file_and_directory_returns_to_initial_state() {
        let root = tempdir().unwrap();
        let backup = tempdir().unwrap();
        let transaction = apply_with_recorder(
            root.path(),
            backup.path(),
            vec![
                FsOperation::Create {
                    path: TreePath::parse("dir"),
                    kind: EntryKind::Dir,
                },
                FsOperation::Create {
                    path: TreePath::parse("dir/child.txt"),
                    kind: EntryKind::File,
                },
            ],
            HashSet::new(),
        );

        assert!(root.path().join("dir/child.txt").is_file());
        let report = apply_undo(&transaction);

        assert_success(&report);
        assert!(!root.path().join("dir").exists());
    }

    #[test]
    fn undo_copy_file_and_directory_returns_to_initial_state() {
        let root = tempdir().unwrap();
        let backup = tempdir().unwrap();
        fs::write(root.path().join("source.txt"), b"source").unwrap();
        fs::create_dir(root.path().join("dir")).unwrap();
        fs::create_dir(root.path().join("dir/nested")).unwrap();
        fs::write(root.path().join("dir/nested/file.txt"), b"nested").unwrap();
        let transaction = apply_with_recorder(
            root.path(),
            backup.path(),
            vec![
                FsOperation::Copy {
                    src: EntryId(1),
                    from: TreePath::parse("source.txt"),
                    to: TreePath::parse("copied.txt"),
                },
                FsOperation::Copy {
                    src: EntryId(2),
                    from: TreePath::parse("dir"),
                    to: TreePath::parse("dir-copy"),
                },
            ],
            HashSet::new(),
        );

        let report = apply_undo(&transaction);

        assert_success(&report);
        assert_eq!(fs::read(root.path().join("source.txt")).unwrap(), b"source");
        assert_eq!(
            fs::read(root.path().join("dir/nested/file.txt")).unwrap(),
            b"nested"
        );
        assert!(!root.path().join("copied.txt").exists());
        assert!(!root.path().join("dir-copy").exists());
    }

    #[test]
    fn undo_move_and_rename_returns_entries_to_original_paths() {
        let root = tempdir().unwrap();
        let backup = tempdir().unwrap();
        fs::create_dir(root.path().join("folder")).unwrap();
        fs::write(root.path().join("move.txt"), b"move").unwrap();
        fs::write(root.path().join("rename.txt"), b"rename").unwrap();
        let transaction = apply_with_recorder(
            root.path(),
            backup.path(),
            vec![
                FsOperation::Move {
                    id: EntryId(1),
                    from: TreePath::parse("move.txt"),
                    to: TreePath::parse("folder/move.txt"),
                },
                FsOperation::Move {
                    id: EntryId(2),
                    from: TreePath::parse("rename.txt"),
                    to: TreePath::parse("renamed.txt"),
                },
            ],
            HashSet::new(),
        );

        let report = apply_undo(&transaction);

        assert_success(&report);
        assert_eq!(fs::read(root.path().join("move.txt")).unwrap(), b"move");
        assert_eq!(fs::read(root.path().join("rename.txt")).unwrap(), b"rename");
        assert!(!root.path().join("folder/move.txt").exists());
        assert!(!root.path().join("renamed.txt").exists());
    }

    #[test]
    fn undo_deleted_file_restores_from_backup() {
        let root = tempdir().unwrap();
        let backup = tempdir().unwrap();
        fs::write(root.path().join("deleted.txt"), b"deleted").unwrap();
        let transaction = apply_with_recorder(
            root.path(),
            backup.path(),
            vec![FsOperation::Delete {
                id: EntryId(1),
                path: TreePath::parse("deleted.txt"),
            }],
            HashSet::new(),
        );

        assert!(!root.path().join("deleted.txt").exists());
        let report = apply_undo(&transaction);

        assert_success(&report);
        assert_eq!(
            fs::read(root.path().join("deleted.txt")).unwrap(),
            b"deleted"
        );
    }

    #[test]
    fn undo_executes_multiple_operations_in_reverse_order() {
        let root = tempdir().unwrap();
        let backup = tempdir().unwrap();
        fs::write(root.path().join("b.txt"), b"b").unwrap();
        fs::write(root.path().join("d.txt"), b"d").unwrap();
        let transaction = apply_with_recorder(
            root.path(),
            backup.path(),
            vec![
                FsOperation::Create {
                    path: TreePath::parse("a.txt"),
                    kind: EntryKind::File,
                },
                FsOperation::Move {
                    id: EntryId(1),
                    from: TreePath::parse("b.txt"),
                    to: TreePath::parse("c.txt"),
                },
                FsOperation::Delete {
                    id: EntryId(2),
                    path: TreePath::parse("d.txt"),
                },
            ],
            HashSet::new(),
        );

        let report = apply_undo(&transaction);

        let executed = report
            .results
            .iter()
            .map(|result| result.op.clone())
            .collect::<Vec<_>>();
        let expected = transaction.steps.iter().rev().cloned().collect::<Vec<_>>();
        assert_eq!(executed, expected);
        assert_success(&report);
        assert!(!root.path().join("a.txt").exists());
        assert_eq!(fs::read(root.path().join("b.txt")).unwrap(), b"b");
        assert!(!root.path().join("c.txt").exists());
        assert_eq!(fs::read(root.path().join("d.txt")).unwrap(), b"d");
    }

    #[cfg(not(windows))]
    #[test]
    fn undo_case_only_rename_uses_case_sensitive_normal_rename_path_on_unix() {
        let root = tempdir().unwrap();
        let from = absolute_path(&root.path().join("a.txt"));
        let to = absolute_path(&root.path().join("A.txt"));
        fs::write(&from, b"case").unwrap();
        fs::rename(&from, &to).unwrap();
        let transaction = UndoTransaction {
            id: "tx".to_owned(),
            root: absolute_path(root.path()),
            steps: vec![UndoStep::MoveBack {
                from: from.clone(),
                to: to.clone(),
                identity: crate::identity::capture_identity(&to).ok(),
                post: crate::identity::capture_fingerprint(&to).unwrap(),
                case_only: true,
            }],
            backup_dir: None,
        };

        let report = apply_undo(&transaction);

        assert_success(&report);
        assert_eq!(fs::read(&from).unwrap(), b"case");
        assert!(!to.exists());
    }

    #[test]
    fn move_back_vacancy_check_is_skipped_only_for_case_insensitive_case_only_rename() {
        // 通常のmove undoは常に空き確認が要る。
        assert!(move_back_requires_vacancy(false, || false));
        assert!(move_back_requires_vacancy(false, || true));
        // case-only + case-insensitive dir: from と to が同一実体 → 確認すると
        // 必ず「占有」に誤検出するためスキップ(Windows既定パス)。
        assert!(!move_back_requires_vacancy(true, || false));
        // case-only + case-sensitive dir: 大文字小文字違いは別エントリ → 確認必須。
        assert!(move_back_requires_vacancy(true, || true));
    }

    #[test]
    fn undo_move_back_uses_classified_same_volume_rename_path() {
        let root = tempdir().unwrap();
        let backup = tempdir().unwrap();
        fs::write(root.path().join("from.txt"), b"move").unwrap();
        let class = crate::classify::classify_move(
            &root.path().join("to.txt"),
            &root.path().join("from.txt"),
            EntryKind::File,
        )
        .unwrap();
        assert_eq!(class, MoveClass::SameVolumeRename);
        let transaction = apply_with_recorder(
            root.path(),
            backup.path(),
            vec![FsOperation::Move {
                id: EntryId(1),
                from: TreePath::parse("from.txt"),
                to: TreePath::parse("to.txt"),
            }],
            HashSet::new(),
        );

        let report = apply_undo(&transaction);

        assert_success(&report);
        assert_eq!(fs::read(root.path().join("from.txt")).unwrap(), b"move");
        assert!(!root.path().join("to.txt").exists());
    }

    #[test]
    fn stale_remove_created_rejects_replaced_file_without_touching_it() {
        let root = tempdir().unwrap();
        let backup = tempdir().unwrap();
        let transaction = apply_with_recorder(
            root.path(),
            backup.path(),
            vec![FsOperation::Create {
                path: TreePath::parse("created.txt"),
                kind: EntryKind::File,
            }],
            HashSet::new(),
        );
        fs::remove_file(root.path().join("created.txt")).unwrap();
        fs::write(root.path().join("replacement.tmp"), b"replacement").unwrap();
        fs::rename(
            root.path().join("replacement.tmp"),
            root.path().join("created.txt"),
        )
        .unwrap();

        let report = apply_undo(&transaction);

        // 削除→再作成はFSのinode再利用でidentityが偶然一致し得るため、
        // どの検査(identity or fingerprint)で拒否されたかは環境依存。
        // 契約は「置き換わった実体を絶対にrecycleしない」こと。
        assert!(
            matches!(&report.results[0].outcome, OpOutcome::Failed { .. }),
            "Undo should reject a replaced file: {:?}",
            report.results[0].outcome
        );
        assert_eq!(
            fs::read(root.path().join("created.txt")).unwrap(),
            b"replacement"
        );
    }

    #[test]
    fn stale_remove_created_rejects_wrong_identity_deterministically() {
        // inode再利用に依存しないidentity経路の決定的検証:
        // fingerprintは現物と完全一致させ、identityだけを偽物にする。
        let root = tempdir().unwrap();
        let path = absolute_path(&root.path().join("created.txt"));
        fs::write(&path, b"created").unwrap();
        let real_identity = crate::identity::capture_identity(&path).unwrap();
        let post = crate::identity::capture_fingerprint(&path).unwrap();
        let transaction = UndoTransaction {
            id: "tx".to_owned(),
            root: absolute_path(root.path()),
            steps: vec![UndoStep::RemoveCreated {
                path: path.clone(),
                identity: Some(FileIdentity {
                    volume: real_identity.volume,
                    file: real_identity.file.wrapping_add(1),
                }),
                post,
            }],
            backup_dir: None,
        };

        let report = apply_undo(&transaction);

        assert_failed_contains(&report.results[0].outcome, "different object");
        assert_eq!(fs::read(&path).unwrap(), b"created");
    }

    #[test]
    fn stale_remove_created_rejects_modified_file_without_touching_it() {
        let root = tempdir().unwrap();
        let backup = tempdir().unwrap();
        let transaction = apply_with_recorder(
            root.path(),
            backup.path(),
            vec![FsOperation::Create {
                path: TreePath::parse("created.txt"),
                kind: EntryKind::File,
            }],
            HashSet::new(),
        );
        fs::write(root.path().join("created.txt"), b"changed").unwrap();

        let report = apply_undo(&transaction);

        assert_failed_contains(&report.results[0].outcome, "Content changed");
        assert_eq!(
            fs::read(root.path().join("created.txt")).unwrap(),
            b"changed"
        );
    }

    #[test]
    fn stale_remove_copied_directory_rejects_manifest_change_without_touching_it() {
        let root = tempdir().unwrap();
        let backup = tempdir().unwrap();
        fs::create_dir(root.path().join("dir")).unwrap();
        fs::write(root.path().join("dir/file.txt"), b"file").unwrap();
        let transaction = apply_with_recorder(
            root.path(),
            backup.path(),
            vec![FsOperation::Copy {
                src: EntryId(1),
                from: TreePath::parse("dir"),
                to: TreePath::parse("dir-copy"),
            }],
            HashSet::new(),
        );
        fs::write(root.path().join("dir-copy/new.txt"), b"new").unwrap();

        let report = apply_undo(&transaction);

        assert_failed_contains(&report.results[0].outcome, "Directory contents changed");
        assert_eq!(
            fs::read(root.path().join("dir-copy/new.txt")).unwrap(),
            b"new"
        );
    }

    #[test]
    fn stale_move_back_rejects_occupied_original_path_without_touching_entries() {
        let root = tempdir().unwrap();
        let backup = tempdir().unwrap();
        fs::write(root.path().join("from.txt"), b"source").unwrap();
        let transaction = apply_with_recorder(
            root.path(),
            backup.path(),
            vec![FsOperation::Move {
                id: EntryId(1),
                from: TreePath::parse("from.txt"),
                to: TreePath::parse("to.txt"),
            }],
            HashSet::new(),
        );
        fs::write(root.path().join("from.txt"), b"occupied").unwrap();

        let report = apply_undo(&transaction);

        assert_failed_contains(&report.results[0].outcome, "original location");
        assert_eq!(fs::read(root.path().join("from.txt")).unwrap(), b"occupied");
        assert_eq!(fs::read(root.path().join("to.txt")).unwrap(), b"source");
    }

    #[test]
    fn stale_move_back_rejects_missing_current_path() {
        let root = tempdir().unwrap();
        let backup = tempdir().unwrap();
        fs::write(root.path().join("from.txt"), b"source").unwrap();
        let transaction = apply_with_recorder(
            root.path(),
            backup.path(),
            vec![FsOperation::Move {
                id: EntryId(1),
                from: TreePath::parse("from.txt"),
                to: TreePath::parse("to.txt"),
            }],
            HashSet::new(),
        );
        fs::remove_file(root.path().join("to.txt")).unwrap();

        let report = apply_undo(&transaction);

        assert_failed_contains(
            &report.results[0].outcome,
            "Target to restore was not found",
        );
        assert!(!root.path().join("from.txt").exists());
        assert!(!root.path().join("to.txt").exists());
    }

    #[test]
    fn stale_restore_deleted_rejects_occupied_path_without_overwriting() {
        let root = tempdir().unwrap();
        let backup = tempdir().unwrap();
        fs::write(root.path().join("deleted.txt"), b"deleted").unwrap();
        let transaction = apply_with_recorder(
            root.path(),
            backup.path(),
            vec![FsOperation::Delete {
                id: EntryId(1),
                path: TreePath::parse("deleted.txt"),
            }],
            HashSet::new(),
        );
        fs::write(root.path().join("deleted.txt"), b"occupied").unwrap();

        let report = apply_undo(&transaction);

        assert_failed_contains(&report.results[0].outcome, "restore destination");
        assert_eq!(
            fs::read(root.path().join("deleted.txt")).unwrap(),
            b"occupied"
        );
    }

    #[test]
    fn missing_backup_payload_fails_without_panicking() {
        let root = tempdir().unwrap();
        let backup = tempdir().unwrap();
        fs::write(root.path().join("deleted.txt"), b"deleted").unwrap();
        let transaction = apply_with_recorder(
            root.path(),
            backup.path(),
            vec![FsOperation::Delete {
                id: EntryId(1),
                path: TreePath::parse("deleted.txt"),
            }],
            HashSet::new(),
        );
        fs::remove_dir_all(backup.path().join("payload")).unwrap();

        let report = apply_undo(&transaction);

        assert_failed_contains(&report.results[0].outcome, "Backup payload was not found");
        assert!(!root.path().join("deleted.txt").exists());
    }

    #[test]
    fn undo_cancellation_skips_remaining_steps_after_current_step_finishes() {
        let root = tempdir().unwrap();
        let backup = tempdir().unwrap();
        let transaction = apply_with_recorder(
            root.path(),
            backup.path(),
            vec![
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
            HashSet::new(),
        );
        let cancel = AtomicBool::new(false);
        let mut progress = Vec::<ApplyProgress<UndoStep>>::new();

        let report = apply_undo_cancellable(&transaction, &cancel, &mut |event| {
            if event.completed == 0 && event.current.is_some() {
                cancel.store(true, Ordering::Relaxed);
            }
            progress.push(event);
        });

        assert!(matches!(report.results[0].outcome, OpOutcome::Success));
        assert!(
            report.results[1..]
                .iter()
                .all(|result| matches!(result.outcome, OpOutcome::Skipped { .. }))
        );
        assert!(root.path().join("a.txt").exists());
        assert!(root.path().join("b.txt").exists());
        assert!(!root.path().join("c.txt").exists());
        assert_eq!(progress.len(), 2);
        assert_eq!(progress[0].completed, 0);
        assert_eq!(progress[1].current, None);
    }

    #[test]
    fn undo_continues_after_one_step_fails_stale() {
        let root = tempdir().unwrap();
        let backup = tempdir().unwrap();
        let transaction = apply_with_recorder(
            root.path(),
            backup.path(),
            vec![
                FsOperation::Create {
                    path: TreePath::parse("a.txt"),
                    kind: EntryKind::File,
                },
                FsOperation::Create {
                    path: TreePath::parse("b.txt"),
                    kind: EntryKind::File,
                },
            ],
            HashSet::new(),
        );
        fs::write(root.path().join("b.txt"), b"changed").unwrap();

        let report = apply_undo(&transaction);

        assert_failed_contains(&report.results[0].outcome, "Content changed");
        assert!(matches!(report.results[1].outcome, OpOutcome::Success));
        assert!(!root.path().join("a.txt").exists());
        assert_eq!(fs::read(root.path().join("b.txt")).unwrap(), b"changed");
    }

    #[test]
    fn preflight_undo_is_read_only_and_matches_execution_readiness() {
        let root = tempdir().unwrap();
        let backup = tempdir().unwrap();
        let transaction = apply_with_recorder(
            root.path(),
            backup.path(),
            vec![
                FsOperation::Create {
                    path: TreePath::parse("ready.txt"),
                    kind: EntryKind::File,
                },
                FsOperation::Create {
                    path: TreePath::parse("stale.txt"),
                    kind: EntryKind::File,
                },
            ],
            HashSet::new(),
        );
        fs::write(root.path().join("stale.txt"), b"changed").unwrap();
        let before = relative_entry_snapshot(root.path());

        let statuses = preflight_undo(&transaction);
        let after = relative_entry_snapshot(root.path());

        assert_eq!(before, after);
        assert!(matches!(statuses[0], Ready));
        assert!(matches!(statuses[1], Rejected { .. }));
        let report = apply_undo(&transaction);
        assert!(matches!(
            report.results[0].outcome,
            OpOutcome::Failed { .. }
        ));
        assert!(matches!(report.results[1].outcome, OpOutcome::Success));
        assert_eq!(fs::read(root.path().join("stale.txt")).unwrap(), b"changed");
        assert!(!root.path().join("ready.txt").exists());
    }

    #[test]
    fn restore_overwritten_chain_is_fail_safe_when_move_back_is_stale() {
        let root = tempdir().unwrap();
        let backup = tempdir().unwrap();
        fs::write(root.path().join("src.txt"), b"source").unwrap();
        fs::write(root.path().join("target.txt"), b"existing").unwrap();
        let target = TreePath::parse("target.txt");
        let transaction = apply_with_recorder(
            root.path(),
            backup.path(),
            vec![FsOperation::Move {
                id: EntryId(1),
                from: TreePath::parse("src.txt"),
                to: target.clone(),
            }],
            HashSet::from([target]),
        );
        assert!(matches!(
            transaction.steps.as_slice(),
            [
                UndoStep::RestoreOverwritten { .. },
                UndoStep::MoveBack { .. }
            ]
        ));
        fs::write(root.path().join("src.txt"), b"occupied").unwrap();

        let report = apply_undo(&transaction);

        assert_eq!(report.results.len(), 2);
        assert_failed_contains(&report.results[0].outcome, "original location");
        assert_failed_contains(&report.results[1].outcome, "restore destination");
        assert_eq!(fs::read(root.path().join("src.txt")).unwrap(), b"occupied");
        assert_eq!(fs::read(root.path().join("target.txt")).unwrap(), b"source");
    }
}
