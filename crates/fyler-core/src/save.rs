//! 保存処理の状態機械(DESIGN.md「保存処理の状態機械」)。
//!
//! `:w`(BufWriteCmd相当)を「rpcnotifyを投げて終わり」にせず、明示的な
//! 状態機械として扱う。確認ダイアログ中の編集・再保存・部分失敗の扱いを
//! ここで一元的に定義する。
//!
//! ```text
//! Idle
//!   → Planning(changedtickスナップショット取得, modifiable=false)
//!   → AwaitingConfirmation(plan表示)
//!   → [承認] Applying → Reconciling → Idle
//!   → [キャンセル/失敗] modifiable=true, dirtyのまま → Idle
//!   → AwaitingUndoConfirmation(undo transaction表示, modifiable=false)
//!   → [承認] ApplyingUndo → Reconciling → Idle
//!   → [キャンセル/全件失敗] modifiable=true → Idle
//! ```
//!
//! この状態機械の遷移確定は**M0の項目**(docs/M0_RESULTS.md #5)。
//! 遷移ロジック([`transition`])は純粋関数とし、副作用は [`SaveEffect`] として
//! 返してapp層が実行する(テスト可能にするため)。

use crate::plan::OperationPlan;
use crate::report::CommitReport;
use crate::undo::{UndoStep, UndoTransaction};
use crate::validate::ValidateError;

#[derive(Debug, Clone)]
pub enum SaveState {
    Idle,
    /// バッファスナップショット取得済み、parse/validate/diff実行中。
    /// この間バッファは `modifiable=false`。
    Planning {
        changedtick: u64,
    },
    /// planを確認ダイアログに表示中。バッファは引き続き `modifiable=false`
    /// (確認中の編集と再`:w`を構造的に防ぐ)。
    AwaitingConfirmation {
        changedtick: u64,
        plan: OperationPlan,
    },
    /// 承認済みplanを実行中。
    Applying {
        changedtick: u64,
        plan: OperationPlan,
    },
    /// undo transactionを確認ダイアログに表示中。バッファはclean前提で
    /// `modifiable=false`。
    AwaitingUndoConfirmation {
        transaction: UndoTransaction,
    },
    /// 承認済みundo transactionを実行中。
    ApplyingUndo {
        transaction: UndoTransaction,
    },
    /// 実FSを再スキャンし、バッファを実FSの状態から再構築中。
    Reconciling,
}

/// 状態機械への入力イベント。
#[derive(Debug, Clone)]
pub enum SaveEvent {
    /// `:w` 相当([`crate::editor::EditorEvent::CommitRequested`] から)。
    /// Idle以外の状態で受けた場合は無視する(modifiable=falseにより通常は発生しない)。
    CommitRequested { changedtick: u64 },
    /// parse/validate/diffが完了し、planができた。
    PlanReady { plan: OperationPlan },
    /// validateエラーで保存を中断する。
    ValidationFailed { errors: Vec<ValidateError> },
    /// 確認ダイアログで承認された。
    Approved,
    /// 確認ダイアログでキャンセルされた。
    Cancelled,
    /// apply完了(部分失敗を含む)。
    ApplyFinished { report: CommitReport },
    /// `:FylerUndo`から直近transactionのundoが要求された。
    UndoRequested { transaction: UndoTransaction },
    /// undo apply完了(部分失敗を含む)。
    UndoApplyFinished { report: CommitReport<UndoStep> },
    /// reconcile完了。
    ReconcileFinished,
}

/// 状態遷移に伴ってapp層が実行すべき副作用。
#[derive(Debug, Clone)]
pub enum SaveEffect {
    /// バッファの `modifiable` を設定する(エンジン経由)。
    SetModifiable(bool),
    /// changedtick付きでバッファ全体をスナップショットし、
    /// parse → validate → diff を実行する(結果はPlanReady/ValidationFailedで戻す)。
    RunPipeline,
    /// 確認ダイアログを表示する(AwaitingConfirmationのplanを見せる)。
    ShowConfirmDialog,
    /// validateエラーを表示する(保存は中断済み)。
    ShowValidationErrors(Vec<ValidateError>),
    /// **承認済み**planをfsops::applyで実行する(絶対ルール1: ここ以外で実FSに触れない)。
    ExecutePlan,
    /// undo確認ダイアログを表示する(AwaitingUndoConfirmationのtransactionを見せる)。
    ShowUndoConfirmDialog,
    /// **承認済み**undo transactionをfsops::applyで実行する。
    ExecuteUndo,
    /// CommitReportを表示する(部分失敗時は操作単位の成功/失敗を提示)。
    ShowCommitReport(CommitReport),
    /// undoのCommitReportを表示する(部分失敗時はstep単位の成功/失敗を提示)。
    ShowUndoReport(CommitReport<UndoStep>),
    /// 実FSを再スキャンし、バッファを実FSの状態から再構築、baselineを更新、
    /// `modified=false` に設定する。
    /// 部分失敗時も実FSを正典としてbaselineとバッファを作り直す。
    ReconcileFromFs,
    /// キャンセル / 全件失敗: baselineは更新しない。バッファはdirtyのまま。
    KeepBufferDirty,
}

/// 状態遷移関数(純粋関数)。
///
/// 実装契約(DESIGN.md「保存処理の状態機械」のルール):
///
/// - `Idle` + `CommitRequested` → `Planning` +
///   `[SetModifiable(false), RunPipeline]`
/// - `Planning` + `PlanReady` → `AwaitingConfirmation` + `[ShowConfirmDialog]`
///   (planが空 = 変更なしの場合の扱いもここで確定する:
///   空planは確認を出さず `Idle` に戻し `SetModifiable(true)` + reconcile不要)
/// - `Planning` + `ValidationFailed` → `Idle` +
///   `[ShowValidationErrors, SetModifiable(true), KeepBufferDirty]`
/// - `AwaitingConfirmation` + `Approved` → `Applying` + `[ExecutePlan]`
/// - `AwaitingConfirmation` + `Cancelled` → `Idle` +
///   `[SetModifiable(true), KeepBufferDirty]`
/// - `Applying` + `ApplyFinished`:
///   - 全件失敗 → `Idle` + `[ShowCommitReport, SetModifiable(true), KeepBufferDirty]`
///   - それ以外(全成功・部分失敗)→ `Reconciling` + `[ShowCommitReport, ReconcileFromFs]`
/// - `Reconciling` + `ReconcileFinished` → `Idle` + `[SetModifiable(true)]`
/// - `Idle` + `UndoRequested` → `AwaitingUndoConfirmation` +
///   `[SetModifiable(false), ShowUndoConfirmDialog]`
/// - `AwaitingUndoConfirmation` + `Approved` → `ApplyingUndo` + `[ExecuteUndo]`
/// - `AwaitingUndoConfirmation` + `Cancelled` → `Idle` + `[SetModifiable(true)]`
/// - `ApplyingUndo` + `UndoApplyFinished`:
///   - 全件失敗 → `Idle` + `[ShowUndoReport, SetModifiable(true)]`
///   - それ以外 → `Reconciling` + `[ShowUndoReport, ReconcileFromFs]`
/// - 上記以外の(状態, イベント)組は不正遷移として無視する(状態維持・副作用なし)。
///   ただしdebugビルドではログ等で気づけるようにしてよい
pub fn transition(state: SaveState, event: SaveEvent) -> (SaveState, Vec<SaveEffect>) {
    match (state, event) {
        (SaveState::Idle, SaveEvent::CommitRequested { changedtick }) => (
            SaveState::Planning { changedtick },
            vec![SaveEffect::SetModifiable(false), SaveEffect::RunPipeline],
        ),
        (SaveState::Idle, SaveEvent::UndoRequested { transaction }) => (
            SaveState::AwaitingUndoConfirmation { transaction },
            vec![
                SaveEffect::SetModifiable(false),
                SaveEffect::ShowUndoConfirmDialog,
            ],
        ),
        (SaveState::Planning { .. }, SaveEvent::PlanReady { plan }) if plan.is_empty() => {
            (SaveState::Idle, vec![SaveEffect::SetModifiable(true)])
        }
        (SaveState::Planning { changedtick }, SaveEvent::PlanReady { plan }) => (
            SaveState::AwaitingConfirmation { changedtick, plan },
            vec![SaveEffect::ShowConfirmDialog],
        ),
        (SaveState::Planning { .. }, SaveEvent::ValidationFailed { errors }) => (
            SaveState::Idle,
            vec![
                SaveEffect::ShowValidationErrors(errors),
                SaveEffect::SetModifiable(true),
                SaveEffect::KeepBufferDirty,
            ],
        ),
        (SaveState::AwaitingConfirmation { changedtick, plan }, SaveEvent::Approved) => (
            SaveState::Applying { changedtick, plan },
            vec![SaveEffect::ExecutePlan],
        ),
        (SaveState::AwaitingConfirmation { .. }, SaveEvent::Cancelled) => (
            SaveState::Idle,
            vec![SaveEffect::SetModifiable(true), SaveEffect::KeepBufferDirty],
        ),
        (SaveState::AwaitingUndoConfirmation { transaction }, SaveEvent::Approved) => (
            SaveState::ApplyingUndo { transaction },
            vec![SaveEffect::ExecuteUndo],
        ),
        (SaveState::AwaitingUndoConfirmation { .. }, SaveEvent::Cancelled) => {
            (SaveState::Idle, vec![SaveEffect::SetModifiable(true)])
        }
        (SaveState::Applying { .. }, SaveEvent::ApplyFinished { report })
            if report.all_failed() =>
        {
            (
                SaveState::Idle,
                vec![
                    SaveEffect::ShowCommitReport(report),
                    SaveEffect::SetModifiable(true),
                    SaveEffect::KeepBufferDirty,
                ],
            )
        }
        (SaveState::Applying { .. }, SaveEvent::ApplyFinished { report }) => (
            SaveState::Reconciling,
            vec![
                SaveEffect::ShowCommitReport(report),
                SaveEffect::ReconcileFromFs,
            ],
        ),
        (SaveState::ApplyingUndo { .. }, SaveEvent::UndoApplyFinished { report })
            if report.all_failed() =>
        {
            (
                SaveState::Idle,
                vec![
                    SaveEffect::ShowUndoReport(report),
                    SaveEffect::SetModifiable(true),
                ],
            )
        }
        (SaveState::ApplyingUndo { .. }, SaveEvent::UndoApplyFinished { report }) => (
            SaveState::Reconciling,
            vec![
                SaveEffect::ShowUndoReport(report),
                SaveEffect::ReconcileFromFs,
            ],
        ),
        (SaveState::Reconciling, SaveEvent::ReconcileFinished) => {
            (SaveState::Idle, vec![SaveEffect::SetModifiable(true)])
        }
        (state, _) => (state, Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    use crate::report::{OpOutcome, OpResult};
    use crate::tree::EntryKind;
    use crate::undo::Fingerprint;

    #[test]
    fn commit_requested_locks_buffer_and_runs_pipeline() {
        let (state, effects) = transition(
            SaveState::Idle,
            SaveEvent::CommitRequested { changedtick: 42 },
        );
        assert!(matches!(state, SaveState::Planning { changedtick: 42 }));
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, SaveEffect::SetModifiable(false)))
        );
        assert!(effects.iter().any(|e| matches!(e, SaveEffect::RunPipeline)));
    }

    #[test]
    fn cancel_returns_to_idle_and_keeps_dirty() {
        let state = SaveState::AwaitingConfirmation {
            changedtick: 42,
            plan: OperationPlan::default(),
        };
        let (state, effects) = transition(state, SaveEvent::Cancelled);
        assert!(matches!(state, SaveState::Idle));
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, SaveEffect::SetModifiable(true)))
        );
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, SaveEffect::KeepBufferDirty))
        );
        // キャンセルではreconcileもbaseline更新もしない
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, SaveEffect::ReconcileFromFs))
        );
    }

    #[test]
    fn commit_while_not_idle_is_ignored() {
        let state = SaveState::Planning { changedtick: 1 };
        let (state, effects) = transition(state, SaveEvent::CommitRequested { changedtick: 2 });
        assert!(matches!(state, SaveState::Planning { changedtick: 1 }));
        assert!(effects.is_empty());
    }

    fn undo_transaction() -> UndoTransaction {
        UndoTransaction {
            id: "tx".to_owned(),
            root: PathBuf::from("root"),
            steps: vec![undo_step("created.txt")],
            backup_dir: None,
        }
    }

    fn undo_step(path: &str) -> UndoStep {
        UndoStep::RemoveCreated {
            path: PathBuf::from(path),
            identity: None,
            post: Fingerprint {
                kind: EntryKind::File,
                size: Some(1),
                mtime: None,
                link_target: None,
            },
        }
    }

    fn undo_report(outcome: OpOutcome) -> CommitReport<UndoStep> {
        CommitReport {
            results: vec![OpResult {
                op: undo_step("created.txt"),
                outcome,
            }],
        }
    }

    #[test]
    fn undo_requested_from_idle_locks_buffer_and_shows_confirmation() {
        let transaction = undo_transaction();

        let (state, effects) = transition(
            SaveState::Idle,
            SaveEvent::UndoRequested {
                transaction: transaction.clone(),
            },
        );

        assert!(matches!(
            state,
            SaveState::AwaitingUndoConfirmation { transaction: actual }
                if actual == transaction
        ));
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, SaveEffect::SetModifiable(false)))
        );
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, SaveEffect::ShowUndoConfirmDialog))
        );
    }

    #[test]
    fn undo_approval_starts_applying_undo() {
        let transaction = undo_transaction();
        let state = SaveState::AwaitingUndoConfirmation {
            transaction: transaction.clone(),
        };

        let (state, effects) = transition(state, SaveEvent::Approved);

        assert!(matches!(
            state,
            SaveState::ApplyingUndo { transaction: actual } if actual == transaction
        ));
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, SaveEffect::ExecuteUndo))
        );
    }

    #[test]
    fn undo_cancel_returns_to_idle_without_keeping_dirty() {
        let state = SaveState::AwaitingUndoConfirmation {
            transaction: undo_transaction(),
        };

        let (state, effects) = transition(state, SaveEvent::Cancelled);

        assert!(matches!(state, SaveState::Idle));
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, SaveEffect::SetModifiable(true)))
        );
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, SaveEffect::KeepBufferDirty))
        );
    }

    #[test]
    fn undo_all_failed_returns_to_idle_and_reports_without_reconcile() {
        let state = SaveState::ApplyingUndo {
            transaction: undo_transaction(),
        };
        let report = undo_report(OpOutcome::Failed {
            error: "stale".to_owned(),
            progress: None,
        });

        let (state, effects) = transition(
            state,
            SaveEvent::UndoApplyFinished {
                report: report.clone(),
            },
        );

        assert!(matches!(state, SaveState::Idle));
        assert!(effects.iter().any(|effect| {
            matches!(effect, SaveEffect::ShowUndoReport(actual) if actual == &report)
        }));
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, SaveEffect::SetModifiable(true)))
        );
        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, SaveEffect::ReconcileFromFs))
        );
    }

    #[test]
    fn undo_partial_success_reconciles_after_report() {
        let state = SaveState::ApplyingUndo {
            transaction: undo_transaction(),
        };
        let report = undo_report(OpOutcome::Success);

        let (state, effects) = transition(
            state,
            SaveEvent::UndoApplyFinished {
                report: report.clone(),
            },
        );

        assert!(matches!(state, SaveState::Reconciling));
        assert!(effects.iter().any(|effect| {
            matches!(effect, SaveEffect::ShowUndoReport(actual) if actual == &report)
        }));
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, SaveEffect::ReconcileFromFs))
        );
    }

    #[test]
    fn commit_while_awaiting_undo_confirmation_is_ignored() {
        let state = SaveState::AwaitingUndoConfirmation {
            transaction: undo_transaction(),
        };

        let (state, effects) = transition(state, SaveEvent::CommitRequested { changedtick: 1 });

        assert!(matches!(state, SaveState::AwaitingUndoConfirmation { .. }));
        assert!(effects.is_empty());
    }

    #[test]
    fn undo_requested_while_applying_undo_is_ignored() {
        let transaction = undo_transaction();
        let state = SaveState::ApplyingUndo {
            transaction: transaction.clone(),
        };

        let (state, effects) = transition(
            state,
            SaveEvent::UndoRequested {
                transaction: undo_transaction(),
            },
        );

        assert!(matches!(
            state,
            SaveState::ApplyingUndo { transaction: actual } if actual == transaction
        ));
        assert!(effects.is_empty());
    }
}
