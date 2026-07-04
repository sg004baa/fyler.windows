//! 確認ダイアログとCommitReport表示。
//!
//! **絶対ルール1**: ここでの承認なしに実FSへ触れない。

use eframe::egui;
use fyler_core::plan::{FsOperation, OperationPlan};
use fyler_core::report::{CommitReport, OpOutcome, OpResult};
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
/// - 警告があれば操作一覧の下へ警告色で表示する
/// - 表示中はGUIの入力ゲートでエンジンへの入力転送を止める
/// - キーボード(`y` / `n` / `Esc`)でも選択できる。ダイアログ表示中はGUI側で
///   入力転送がゲートされている前提とする
/// - 選択結果はapp層の保存フローへ返す
pub fn draw_plan(
    ui: &mut egui::Ui,
    plan: &OperationPlan,
    warnings: &[String],
) -> Option<ConfirmChoice> {
    let key_choice = ui.ctx().input(|input| {
        plan_choice_from_keys(
            input.key_pressed(egui::Key::Y),
            input.key_pressed(egui::Key::N),
            input.key_pressed(egui::Key::Escape),
        )
    });

    egui::Modal::new(egui::Id::new("save-plan-confirmation"))
        .show(ui.ctx(), |ui| {
            ui.heading("変更内容の確認");
            ui.add_space(8.0);

            for operation in &plan.ops {
                ui.monospace(operation_label(operation));
            }
            if !warnings.is_empty() {
                ui.add_space(8.0);
                for warning in warnings {
                    ui.colored_label(ui.visuals().warn_fg_color, warning);
                }
            }

            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let approve_clicked = ui.button("Approve (y)").clicked();
                let cancel_clicked = ui.button("Cancel (n / Esc)").clicked();
                if approve_clicked || key_choice == Some(ConfirmChoice::Approve) {
                    Some(ConfirmChoice::Approve)
                } else if cancel_clicked || key_choice == Some(ConfirmChoice::Cancel) {
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
/// 閉じるボタンが押されたら `true` を返す。
pub fn draw_report(ui: &mut egui::Ui, report: &CommitReport) -> bool {
    let dismiss_from_keyboard = ui
        .ctx()
        .input(|input| input.key_pressed(egui::Key::Enter) || input.key_pressed(egui::Key::Escape));

    egui::Modal::new(egui::Id::new("save-commit-report"))
        .show(ui.ctx(), |ui| {
            ui.heading("実行結果");
            ui.add_space(8.0);

            for result in &report.results {
                let label = report_label(result);
                match &result.outcome {
                    OpOutcome::Success => {
                        ui.monospace(label);
                    }
                    OpOutcome::Failed { .. } => {
                        ui.colored_label(ui.visuals().error_fg_color, label);
                    }
                    OpOutcome::Skipped { .. } => {
                        ui.monospace(label);
                    }
                }
            }

            ui.add_space(12.0);
            ui.button("閉じる (Enter / Esc)").clicked() || dismiss_from_keyboard
        })
        .inner
}

/// plan確認キーの押下状態を、エンジン非依存の確認結果へ変換する。
fn plan_choice_from_keys(y: bool, n: bool, esc: bool) -> Option<ConfirmChoice> {
    if y {
        Some(ConfirmChoice::Approve)
    } else if n || esc {
        Some(ConfirmChoice::Cancel)
    } else {
        None
    }
}

fn report_label(result: &OpResult) -> String {
    let operation = operation_label(&result.op);
    match &result.outcome {
        OpOutcome::Success => format!("OK  {operation}"),
        OpOutcome::Failed { error, progress } => {
            let progress = progress
                .as_deref()
                .map(|progress| format!(" / 進捗: {progress}"))
                .unwrap_or_default();
            format!("NG  {operation} (理由: {error}{progress})")
        }
        OpOutcome::Skipped { reason } => {
            format!("--  SKIP {operation} (理由: {reason})")
        }
    }
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
        | ValidateError::EmptyName { line }
        | ValidateError::ReservedChar { line, .. }
        | ValidateError::ReservedName { line, .. }
        | ValidateError::InvalidTrailing { line, .. } => Some(*line),
        ValidateError::DuplicateName { .. }
        | ValidateError::MoveIntoSelf { .. }
        | ValidateError::MoveCycle { .. } => None,
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

    #[test]
    fn report_labels_include_failure_progress() {
        let result = OpResult {
            op: FsOperation::Copy {
                src: EntryId(1),
                from: TreePath::parse("a.txt"),
                to: TreePath::parse("b.txt"),
            },
            outcome: OpOutcome::Failed {
                error: "copy failed".to_owned(),
                progress: Some("2/3 files".to_owned()),
            },
        };
        assert_eq!(
            report_label(&result),
            "NG  COPY a.txt → b.txt (理由: copy failed / 進捗: 2/3 files)"
        );
    }

    #[test]
    fn plan_keyboard_shortcuts_map_to_choices() {
        assert_eq!(
            plan_choice_from_keys(true, false, false),
            Some(ConfirmChoice::Approve)
        );
        assert_eq!(
            plan_choice_from_keys(false, true, false),
            Some(ConfirmChoice::Cancel)
        );
        assert_eq!(
            plan_choice_from_keys(false, false, true),
            Some(ConfirmChoice::Cancel)
        );
        assert_eq!(plan_choice_from_keys(false, false, false), None);
    }
}
