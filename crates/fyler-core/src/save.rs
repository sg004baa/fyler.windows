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
//! ```
//!
//! この状態機械の遷移確定は**M0の項目**(docs/M0_RESULTS.md #5)。
//! 遷移ロジック([`transition`])は純粋関数とし、副作用は [`SaveEffect`] として
//! 返してapp層が実行する(テスト可能にするため)。

use crate::plan::OperationPlan;
use crate::report::CommitReport;
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
    /// 実FSを再スキャンし、バッファを実FSの状態から再構築中。
    Reconciling {
        report: CommitReport,
    },
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
    /// CommitReportを表示する(部分失敗時は操作単位の成功/失敗を提示)。
    ShowCommitReport(CommitReport),
    /// 実FSを再スキャンし、バッファを実FSの状態から再構築、baselineを更新、
    /// `modified=false` に設定する。
    /// 部分失敗時: 成功した操作のみbaselineへ反映し、失敗分に対応する行は
    /// dirtyな差分として残す。
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
/// - 上記以外の(状態, イベント)組は不正遷移として無視する(状態維持・副作用なし)。
///   ただしdebugビルドではログ等で気づけるようにしてよい
pub fn transition(state: SaveState, event: SaveEvent) -> (SaveState, Vec<SaveEffect>) {
    todo!("M0: 上記の実装契約どおりに遷移を実装し、下の#[ignore]テストを通す")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "M0: save::transition 未実装"]
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
    #[ignore = "M0: save::transition 未実装"]
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
    #[ignore = "M0: save::transition 未実装"]
    fn commit_while_not_idle_is_ignored() {
        let state = SaveState::Planning { changedtick: 1 };
        let (state, effects) = transition(state, SaveEvent::CommitRequested { changedtick: 2 });
        assert!(matches!(state, SaveState::Planning { changedtick: 1 }));
        assert!(effects.is_empty());
    }
}
