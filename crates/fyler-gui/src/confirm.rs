//! 確認ダイアログ(dry-run表示)とCommitReport表示。
//!
//! **絶対ルール1**: ここでの承認なしに実FSへ触れない。M2のゴールは
//! 「`i` でrenameを書いて `:w` するとダイアログに `RENAME a → b` が出る」
//! (実行はしない)。

use eframe::egui;
use fyler_core::plan::{FsOperation, OperationPlan};
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
/// - 表示中はGUIの入力ゲートでエンジンへの入力転送を止める
/// - 選択結果はapp層の保存フローへ返す(M2のApproveはdry-run終了として扱う)
pub fn draw_plan(ui: &mut egui::Ui, plan: &OperationPlan) -> Option<ConfirmChoice> {
    egui::Modal::new(egui::Id::new("save-plan-confirmation"))
        .show(ui.ctx(), |ui| {
            ui.heading("変更内容の確認");
            ui.add_space(8.0);

            for operation in &plan.ops {
                ui.monospace(operation_label(operation));
            }

            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.button("Approve").clicked() {
                    Some(ConfirmChoice::Approve)
                } else if ui.button("Cancel").clicked() {
                    Some(ConfirmChoice::Cancel)
                } else {
                    None
                }
            })
            .inner
        })
        .inner
}

/// validateエラーの表示(保存は中断済み)。行番号は0始まりなので表示時に+1する。
pub fn draw_validation_errors(ui: &mut egui::Ui, errors: &[ValidateError]) {
    for error in errors {
        ui.colored_label(ui.visuals().error_fg_color, validation_error_label(error));
    }
}

/// CommitReportの表示。部分失敗時は操作単位の成功/失敗と、
/// 非原子的操作の進捗(`OpOutcome::Failed.progress`)を明示する。
pub fn draw_report(ui: &mut egui::Ui, report: &CommitReport) {
    todo!("M3: CommitReport表示")
}

fn operation_label(operation: &FsOperation) -> String {
    match operation {
        FsOperation::Create { path, .. } => format!("CREATE {path}"),
        FsOperation::Move { from, to, .. } if from.parent() == to.parent() => {
            format!("RENAME {from} → {to}")
        }
        FsOperation::Move { from, to, .. } => format!("MOVE {from} → {to}"),
        FsOperation::Copy { from, to, .. } => format!("COPY {from} → {to}"),
        FsOperation::Delete { path, .. } => format!("DELETE {path} (ごみ箱へ)"),
    }
}

fn validation_error_label(error: &ValidateError) -> String {
    let line = match error {
        ValidateError::BrokenIdPrefix { line }
        | ValidateError::InvalidIndent { line }
        | ValidateError::ReservedChar { line, .. }
        | ValidateError::ReservedName { line, .. }
        | ValidateError::InvalidTrailing { line, .. } => Some(*line),
        ValidateError::DuplicateName { .. } | ValidateError::MoveIntoSelf { .. } => None,
    };

    let label = error.to_string();
    match line {
        Some(line) => label.replacen(
            &format!("行{line}"),
            &format!("行{}", line.saturating_add(1)),
            1,
        ),
        None => label,
    }
}

#[cfg(test)]
mod tests {
    use fyler_core::{EntryId, TreePath};

    use super::*;

    #[test]
    fn labels_same_parent_move_as_rename() {
        let operation = FsOperation::Move {
            id: EntryId(1),
            from: TreePath::parse("a.txt"),
            to: TreePath::parse("b.txt"),
        };
        assert_eq!(operation_label(&operation), "RENAME a.txt → b.txt");
    }

    #[test]
    fn displays_validation_line_as_one_based() {
        let error = ValidateError::BrokenIdPrefix { line: 0 };
        assert_eq!(
            validation_error_label(&error),
            "行1: IDプレフィックスが壊れています。undoで戻すか行を削除してください"
        );
    }
}
