//! 確認ダイアログ(dry-run表示)とCommitReport表示。
//!
//! **絶対ルール1**: ここでの承認なしに実FSへ触れない。M2のゴールは
//! 「`i` でrenameを書いて `:w` するとダイアログに `RENAME a → b` が出る」
//! (実行はしない)。

use eframe::egui;
use fyler_core::plan::OperationPlan;
use fyler_core::report::CommitReport;
use fyler_core::validate::ValidateError;

/// ユーザーの選択。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmChoice {
    Approve,
    Cancel,
}

/// OperationPlanをモーダルで表示し、選択があれば返す。
///
/// 実装契約:
/// - 操作を1件1行で人間可読に表示する(例: `RENAME a.txt → b.txt`、
///   `DELETE src/old.rs (ごみ箱へ)`、`COPY a.txt → b.txt`)
/// - 表示中はバッファがmodifiable=falseである(保存状態機械が保証。GUIは前提にしてよい)
/// - 選択結果は保存状態機械の `Approved` / `Cancelled` イベントになる
pub fn draw_plan(ui: &mut egui::Ui, plan: &OperationPlan) -> Option<ConfirmChoice> {
    todo!("M2: planの確認ダイアログ(M2のゴール)")
}

/// validateエラーの表示(保存は中断済み)。行番号は0始まりなので表示時に+1する。
pub fn draw_validation_errors(ui: &mut egui::Ui, errors: &[ValidateError]) {
    todo!("M2: validateエラー表示")
}

/// CommitReportの表示。部分失敗時は操作単位の成功/失敗と、
/// 非原子的操作の進捗(`OpOutcome::Failed.progress`)を明示する。
pub fn draw_report(ui: &mut egui::Ui, report: &CommitReport) {
    todo!("M3: CommitReport表示")
}
