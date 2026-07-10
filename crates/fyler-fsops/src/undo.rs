//! forward apply 1回分のundo receipt recorder。

use std::path::{Path, PathBuf};

use fyler_core::tree::EntryKind;
use fyler_core::undo::{BackupRef, FileIdentity, Fingerprint, UndoStep, UndoTransaction};

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

#[cfg(test)]
mod tests {
    use std::fs;

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
}
