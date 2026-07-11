//! undo表示用の文字列整形。
//!
//! `fyler-gui`へ`UndoStep`の知識を持ち込まないため、app層で確認・進捗・結果の
//! 表示行へ変換する。

use fyler_core::report::{CommitReport, OpOutcome};
use fyler_core::undo::{UndoStep, UndoStepStatus};

use crate::undo_journal::{JournalEntry, JournalState};

/// undo stepを確認・進捗・結果で共通利用する表示ラベルへ変換する。
pub(super) fn undo_step_label(step: &UndoStep) -> String {
    match step {
        UndoStep::RemoveCreated { path, .. } => format!("UNDO CREATE {}", path.display()),
        UndoStep::RemoveCopied { path, .. } => format!("UNDO COPY {}", path.display()),
        UndoStep::MoveBack { from, to, .. } => {
            format!("UNDO MOVE {} → {}", to.display(), from.display())
        }
        UndoStep::RestoreDeleted { path, .. } => format!("UNDO DELETE {}", path.display()),
        UndoStep::RestoreOverwritten { path, .. } => {
            format!("UNDO OVERWRITE {}", path.display())
        }
    }
}

/// undo確認ダイアログに渡す表示行を構築する。
pub(super) fn undo_plan_lines(
    transaction: &fyler_core::undo::UndoTransaction,
    statuses: &[UndoStepStatus],
) -> Vec<String> {
    let mut lines = Vec::new();
    for (step, status) in transaction.steps.iter().zip(statuses) {
        lines.push(undo_step_label(step));
        if let UndoStepStatus::Rejected { reason } = status {
            lines.push(format!("  [対象外] {reason}"));
        }
    }
    lines
}

/// undo結果ダイアログに渡す表示行と失敗有無を構築する。
pub(super) fn undo_report_lines(report: &CommitReport<UndoStep>) -> (Vec<String>, bool) {
    let lines = report
        .results
        .iter()
        .map(|result| outcome_label(undo_step_label(&result.op), &result.outcome))
        .collect::<Vec<_>>();
    (lines, report.any_failed())
}

/// 起動時復旧ダイアログに渡す表示行を構築する。
pub(super) fn recovery_descriptions(entries: &[JournalEntry]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| {
            format!(
                "{}: {} ({})",
                entry.id,
                journal_state_label(entry.state),
                entry.dir.display()
            )
        })
        .collect()
}

/// 起動時復旧ダイアログを表示すべきか判定する。
pub(super) fn should_show_undo_recovery(entries: &[JournalEntry]) -> bool {
    entries
        .iter()
        .any(|entry| matches!(entry.state, JournalState::Preparing | JournalState::Undoing))
}

fn outcome_label(operation: String, outcome: &OpOutcome) -> String {
    match outcome {
        OpOutcome::Success => format!("OK  {operation}"),
        OpOutcome::Failed { error, progress } => {
            let progress = progress
                .as_deref()
                .map(|progress| format!(" / progress: {progress}"))
                .unwrap_or_default();
            format!("NG  {operation} (reason: {error}{progress})")
        }
        OpOutcome::Skipped { reason } => {
            format!("--  SKIP {operation} (reason: {reason})")
        }
    }
}

fn journal_state_label(state: JournalState) -> &'static str {
    match state {
        JournalState::Preparing => "Preparing",
        JournalState::Committed => "Committed",
        JournalState::Undoing => "Undoing",
        JournalState::Undone => "Undone",
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use fyler_core::report::{CommitReport, OpResult};
    use fyler_core::tree::EntryKind;
    use fyler_core::undo::{BackupRef, Fingerprint, UndoStep, UndoStepStatus, UndoTransaction};

    use super::*;

    fn fingerprint(kind: EntryKind) -> Fingerprint {
        Fingerprint {
            kind,
            size: None,
            mtime: None,
            link_target: None,
        }
    }

    fn backup() -> BackupRef {
        BackupRef {
            payload_rel: "payload/0/file.txt".to_owned(),
            kind: EntryKind::File,
        }
    }

    #[test]
    fn formats_each_undo_step_variant() {
        let root = PathBuf::from("/root");
        let steps = [
            UndoStep::RemoveCreated {
                path: root.join("created.txt"),
                identity: None,
                post: fingerprint(EntryKind::File),
            },
            UndoStep::RemoveCopied {
                path: root.join("copied.txt"),
                identity: None,
                post: fingerprint(EntryKind::File),
                manifest: None,
            },
            UndoStep::MoveBack {
                from: root.join("a.txt"),
                to: root.join("b.txt"),
                identity: None,
                post: fingerprint(EntryKind::File),
                case_only: false,
            },
            UndoStep::RestoreDeleted {
                path: root.join("deleted.txt"),
                backup: backup(),
            },
            UndoStep::RestoreOverwritten {
                path: root.join("target.txt"),
                backup: backup(),
            },
        ];

        let labels = steps.iter().map(undo_step_label).collect::<Vec<_>>();

        // PathBuf::join はWindowsで `\` 区切りになるため、期待値も同じ
        // display() から組み立てる(表示契約は「pathをそのままdisplay」)。
        let p = |name: &str| root.join(name).display().to_string();
        assert_eq!(
            labels,
            [
                format!("UNDO CREATE {}", p("created.txt")),
                format!("UNDO COPY {}", p("copied.txt")),
                format!("UNDO MOVE {} → {}", p("b.txt"), p("a.txt")),
                format!("UNDO DELETE {}", p("deleted.txt")),
                format!("UNDO OVERWRITE {}", p("target.txt")),
            ]
        );
    }

    #[test]
    fn undo_plan_lines_include_rejected_reasons() {
        let transaction = UndoTransaction {
            id: "tx".to_owned(),
            root: PathBuf::from("/root"),
            steps: vec![UndoStep::RemoveCreated {
                path: PathBuf::from("/root/created.txt"),
                identity: None,
                post: fingerprint(EntryKind::File),
            }],
            backup_dir: None,
        };

        let lines = undo_plan_lines(
            &transaction,
            &[UndoStepStatus::Rejected {
                reason: "変更されています".to_owned(),
            }],
        );

        assert_eq!(
            lines,
            [
                "UNDO CREATE /root/created.txt",
                "  [対象外] 変更されています"
            ]
        );
    }

    #[test]
    fn undo_report_lines_mark_failures_and_progress() {
        let step = UndoStep::RestoreDeleted {
            path: PathBuf::from("/root/deleted.txt"),
            backup: backup(),
        };
        let report = CommitReport {
            results: vec![OpResult {
                op: step,
                outcome: OpOutcome::Failed {
                    error: "復元できません".to_owned(),
                    progress: Some("1/2 files".to_owned()),
                },
            }],
        };

        let (lines, any_failed) = undo_report_lines(&report);

        assert!(any_failed);
        assert_eq!(
            lines,
            ["NG  UNDO DELETE /root/deleted.txt (reason: 復元できません / progress: 1/2 files)"]
        );
    }

    #[test]
    fn recovery_dialog_is_shown_only_for_preparing_or_undoing() {
        assert!(!should_show_undo_recovery(&[]));
        assert!(!should_show_undo_recovery(&[JournalEntry {
            id: "done".to_owned(),
            state: JournalState::Committed,
            dir: PathBuf::from("/undo/done"),
        }]));
        assert!(should_show_undo_recovery(&[JournalEntry {
            id: "preparing".to_owned(),
            state: JournalState::Preparing,
            dir: PathBuf::from("/undo/preparing"),
        }]));
    }
}
