//! 確認ダイアログとCommitReport表示。
//!
//! **絶対ルール1**: ここでの承認なしに実FSへ触れない。

use std::path::PathBuf;

use eframe::egui;
use fyler_core::pane::PaneId;
use fyler_core::path::TreePath;
use fyler_core::plan::{FsOperation, OperationPlan};
use fyler_core::report::{CommitReport, OpOutcome, OpResult};
use fyler_core::transfer::{TransferKind, TransferOp, TransferPlan};
use fyler_core::validate::ValidateError;

/// ユーザーの選択。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmChoice {
    Approve,
    Cancel,
}

/// 保存確認ダイアログでの操作一覧の詳細度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConfirmDetail {
    /// 全操作を1件ずつ表示する。
    #[default]
    Full,
    /// 操作が多い場合に種別ごとの件数へ圧縮する。
    Summary,
}

/// ツリーへ描画するファイルアイコンのスタイル。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IconStyle {
    /// eguiの既定フォントだけで表示できるASCII文字を使う。
    #[default]
    Ascii,
    /// ユーザーが指定したNerd Fontのグリフを使う。
    Nerd,
}

/// OperationPlanをモーダルで表示し、選択があれば返す。
///
/// 実装契約:
/// - 操作を1件1行で人間可読に表示する(例: `RENAME a.txt → b.txt`、
///   `DELETE src/old.rs (ごみ箱へ)`、`COPY a.txt → b.txt`)
/// - 上書き対象があれば、ごみ箱へ退避するパスを操作一覧の下へ警告色で表示する
/// - 警告があれば操作一覧の下へ警告色で表示する
/// - 表示中はGUIの入力ゲートでエンジンへの入力転送を止める
/// - キーボード(`y` / `n` / `Esc`)でも選択できる。ダイアログ表示中はGUI側で
///   入力転送がゲートされている前提とする
/// - 選択結果はapp層の保存フローへ返す
pub fn draw_plan(
    ui: &mut egui::Ui,
    plan: &OperationPlan,
    overwrites: &[TreePath],
    warnings: &[String],
    detail: ConfirmDetail,
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
            ui.heading("Confirm changes");
            ui.add_space(8.0);

            for label in plan_labels(plan, detail) {
                ui.monospace(label);
            }
            if !overwrites.is_empty() {
                ui.add_space(8.0);
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    "These existing files will be moved to the recycle bin before overwrite:",
                );
                for path in overwrites {
                    ui.colored_label(ui.visuals().warn_fg_color, path.to_string());
                }
            }
            if !warnings.is_empty() {
                ui.add_space(8.0);
                for warning in warnings {
                    ui.colored_label(ui.visuals().warn_fg_color, warning);
                }
            }

            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let approve_label = if overwrites.is_empty() {
                    "Approve (y)"
                } else {
                    "Overwrite and apply (y)"
                };
                let approve_clicked = ui.button(approve_label).clicked();
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

/// pane間transferを保存planと同じ確認モーダルで表示する。
pub fn draw_transfer_plan(
    ui: &mut egui::Ui,
    plan: &TransferPlan,
    target: PaneId,
    overwrites: &[PathBuf],
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
            ui.heading("Confirm changes");
            ui.add_space(8.0);
            for operation in &plan.ops {
                ui.monospace(transfer_operation_label(operation, Some(target)));
            }
            if !overwrites.is_empty() {
                ui.add_space(8.0);
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    "These existing files will be moved to the recycle bin before overwrite:",
                );
                for path in overwrites {
                    ui.colored_label(ui.visuals().warn_fg_color, path.display().to_string());
                }
            }
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let approve_label = if overwrites.is_empty() {
                    "Approve (y)"
                } else {
                    "Overwrite and apply (y)"
                };
                let approve_clicked = ui.button(approve_label).clicked();
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

/// undo確認ダイアログを表示する。行はapp層で整形済み。
pub fn draw_undo_plan(ui: &mut egui::Ui, lines: &[String]) -> Option<ConfirmChoice> {
    let key_choice = ui.ctx().input(|input| {
        plan_choice_from_keys(
            input.key_pressed(egui::Key::Y),
            input.key_pressed(egui::Key::N),
            input.key_pressed(egui::Key::Escape),
        )
    });

    egui::Modal::new(egui::Id::new("undo-plan-confirmation"))
        .show(ui.ctx(), |ui| {
            ui.heading("Confirm undo");
            ui.add_space(8.0);
            for line in lines {
                if line.trim_start().starts_with("[対象外]") {
                    ui.colored_label(ui.visuals().warn_fg_color, line);
                } else {
                    ui.monospace(line);
                }
            }
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let approve_clicked = ui.button("Undo (y)").clicked();
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

fn plan_labels(plan: &OperationPlan, detail: ConfirmDetail) -> Vec<String> {
    if detail == ConfirmDetail::Full || plan.ops.len() <= 5 {
        return plan.ops.iter().map(operation_label).collect();
    }

    let mut create = 0;
    let mut rename = 0;
    let mut move_count = 0;
    let mut copy = 0;
    let mut delete = 0;
    for operation in &plan.ops {
        match operation {
            FsOperation::Create { .. } => create += 1,
            FsOperation::Move { from, to, .. } if from.parent() == to.parent() => rename += 1,
            FsOperation::Move { .. } => move_count += 1,
            FsOperation::Copy { .. } => copy += 1,
            FsOperation::Delete { .. } => delete += 1,
        }
    }

    let summaries = [
        (create, "CREATE", ""),
        (rename, "RENAME", ""),
        (move_count, "MOVE", ""),
        (copy, "COPY", ""),
        (delete, "DELETE", "(to recycle bin)"),
    ]
    .into_iter()
    .filter(|(count, _, _)| *count > 0)
    .map(|(count, kind, suffix)| format!("{kind} {count}{suffix}"))
    .collect::<Vec<_>>();
    vec![summaries.join(" / ")]
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
            ui.heading("Apply result");
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
            ui.button("Close (Enter / Esc)").clicked() || dismiss_from_keyboard
        })
        .inner
}

/// undo結果ダイアログを表示する。行はapp層で整形済み。
pub fn draw_undo_report(ui: &mut egui::Ui, lines: &[String], any_failed: bool) -> bool {
    let dismiss_from_keyboard = ui
        .ctx()
        .input(|input| input.key_pressed(egui::Key::Enter) || input.key_pressed(egui::Key::Escape));

    egui::Modal::new(egui::Id::new("undo-commit-report"))
        .show(ui.ctx(), |ui| {
            ui.heading("Undo result");
            ui.add_space(8.0);
            if any_failed {
                ui.colored_label(ui.visuals().warn_fg_color, "Some undo steps did not run.");
                ui.add_space(4.0);
            }
            for line in lines {
                if line.starts_with("NG") {
                    ui.colored_label(ui.visuals().error_fg_color, line);
                } else {
                    ui.monospace(line);
                }
            }
            ui.add_space(12.0);
            ui.button("Close (Enter / Esc)").clicked() || dismiss_from_keyboard
        })
        .inner
}

/// pane間transferのCommitReportを既存reportモーダルで表示する。
pub fn draw_transfer_report(ui: &mut egui::Ui, report: &CommitReport<TransferOp>) -> bool {
    let dismiss_from_keyboard = ui
        .ctx()
        .input(|input| input.key_pressed(egui::Key::Enter) || input.key_pressed(egui::Key::Escape));
    egui::Modal::new(egui::Id::new("save-commit-report"))
        .show(ui.ctx(), |ui| {
            ui.heading("Apply result");
            ui.add_space(8.0);
            for result in &report.results {
                let label =
                    outcome_label(transfer_operation_label(&result.op, None), &result.outcome);
                match result.outcome {
                    OpOutcome::Failed { .. } => {
                        ui.colored_label(ui.visuals().error_fg_color, label);
                    }
                    OpOutcome::Success | OpOutcome::Skipped { .. } => {
                        ui.monospace(label);
                    }
                }
            }
            ui.add_space(12.0);
            ui.button("Close (Enter / Esc)").clicked() || dismiss_from_keyboard
        })
        .inner
}

/// 起動時のundo journal復旧候補を表示する。
///
/// `Approve`は候補の破棄、`Cancel`は保持して閉じることを表す。
pub fn draw_undo_recovery(ui: &mut egui::Ui, descriptions: &[String]) -> Option<ConfirmChoice> {
    let key_choice = ui.ctx().input(|input| {
        plan_choice_from_keys(
            input.key_pressed(egui::Key::Y),
            input.key_pressed(egui::Key::N),
            input.key_pressed(egui::Key::Escape),
        )
    });

    egui::Modal::new(egui::Id::new("undo-recovery"))
        .show(ui.ctx(), |ui| {
            ui.heading("Undo recovery");
            ui.add_space(8.0);
            ui.colored_label(
                ui.visuals().warn_fg_color,
                "Unfinished undo journal entries were found.",
            );
            ui.add_space(4.0);
            for description in descriptions {
                ui.monospace(description);
            }
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let discard_clicked = ui.button("Discard (y)").clicked();
                let keep_clicked = ui.button("Keep and close (n / Esc)").clicked();
                if discard_clicked || key_choice == Some(ConfirmChoice::Approve) {
                    Some(ConfirmChoice::Approve)
                } else if keep_clicked || key_choice == Some(ConfirmChoice::Cancel) {
                    Some(ConfirmChoice::Cancel)
                } else {
                    None
                }
            })
            .inner
        })
        .inner
}

/// apply進捗ダイアログを描画する。
///
/// キャンセルボタンまたは`n`/`Esc`が押されたら`true`を返す。キャンセル要求後は
/// ボタンを無効化し、実行中の操作が完了した後に停止することを表示する。
pub fn draw_apply_progress(
    ui: &mut egui::Ui,
    completed: usize,
    total: usize,
    current: Option<&str>,
    cancel_requested: bool,
) -> bool {
    let cancel_from_keyboard = !cancel_requested
        && ui
            .ctx()
            .input(|input| input.key_pressed(egui::Key::N) || input.key_pressed(egui::Key::Escape));
    let fraction = completed as f32 / total.max(1) as f32;

    egui::Modal::new(egui::Id::new("save-apply-progress"))
        .show(ui.ctx(), |ui| {
            ui.heading("Applying changes");
            ui.add_space(8.0);
            ui.add(egui::ProgressBar::new(fraction.clamp(0.0, 1.0)));
            ui.label(format!("{completed} / {total}"));
            if let Some(current) = current {
                ui.add_space(4.0);
                ui.monospace(current);
            }

            ui.add_space(12.0);
            if cancel_requested {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    "Cancel requested; stopping after the current operation finishes",
                );
            }
            let cancel_clicked = ui
                .add_enabled(!cancel_requested, egui::Button::new("Cancel (n / Esc)"))
                .clicked();
            cancel_clicked || cancel_from_keyboard
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
    outcome_label(operation, &result.outcome)
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

/// transfer操作を確認・進捗・結果ダイアログで共通利用する表示ラベルへ変換する。
pub(crate) fn transfer_operation_label(operation: &TransferOp, target: Option<PaneId>) -> String {
    let kind = match operation.kind {
        TransferKind::Move => "MOVE",
        TransferKind::Copy => "COPY",
    };
    let pane = target
        .map(|pane| format!("[pane {pane}] "))
        .unwrap_or_default();
    format!("{kind} {} → {pane}{}", operation.from, operation.to)
}

/// 操作を確認・進捗・結果ダイアログで共通利用する表示ラベルへ変換する。
pub(crate) fn operation_label(operation: &FsOperation) -> String {
    match operation {
        FsOperation::Create { path, .. } => format!("CREATE {path}"),
        FsOperation::Move { from, to, .. } if from.parent() == to.parent() => {
            format!("RENAME {from} → {to}")
        }
        FsOperation::Move { from, to, .. } => format!("MOVE {from} → {to}"),
        FsOperation::Copy { from, to, .. } => format!("COPY {from} → {to}"),
        FsOperation::Delete { path, .. } => format!("DELETE {path} (to recycle bin)"),
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
        | ValidateError::MoveCycle { .. }
        | ValidateError::TargetOccupiedByDirectory { .. } => None,
    };

    let label = error.to_string();
    match line {
        Some(line) => label.replacen(
            &format!("line {line}"),
            &format!("line {}", line.saturating_add(1)),
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
    fn transfer_label_names_the_target_pane() {
        let operation = TransferOp {
            kind: TransferKind::Move,
            from: TreePath::parse("dir/a.txt"),
            to: TreePath::parse("inbox/a.txt"),
            entry_kind: fyler_core::tree::EntryKind::File,
        };
        assert_eq!(
            transfer_operation_label(&operation, Some(PaneId::new(2))),
            "MOVE dir/a.txt → [pane 2] inbox/a.txt"
        );
    }

    #[test]
    fn displays_validation_line_as_one_based() {
        let error = ValidateError::BrokenIdPrefix { line: 0 };
        assert_eq!(
            validation_error_label(&error),
            "line 1: broken ID prefix; undo or delete this line"
        );
    }

    #[test]
    fn displays_target_directory_conflict_without_line_rewrite() {
        let error = ValidateError::TargetOccupiedByDirectory {
            path: TreePath::parse(".hidden"),
        };
        assert_eq!(
            validation_error_label(&error),
            "target is occupied by an existing directory and cannot be overwritten: .hidden"
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
            "NG  COPY a.txt → b.txt (reason: copy failed / progress: 2/3 files)"
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

    #[test]
    fn summary_detail_counts_operation_kinds() {
        let plan = OperationPlan {
            ops: vec![
                FsOperation::Move {
                    id: EntryId(1),
                    from: TreePath::parse("a.txt"),
                    to: TreePath::parse("b.txt"),
                },
                FsOperation::Move {
                    id: EntryId(2),
                    from: TreePath::parse("c.txt"),
                    to: TreePath::parse("d.txt"),
                },
                FsOperation::Delete {
                    id: EntryId(3),
                    path: TreePath::parse("old.txt"),
                },
                FsOperation::Copy {
                    src: EntryId(4),
                    from: TreePath::parse("src/a.txt"),
                    to: TreePath::parse("dst/a.txt"),
                },
                FsOperation::Copy {
                    src: EntryId(5),
                    from: TreePath::parse("src/b.txt"),
                    to: TreePath::parse("dst/b.txt"),
                },
                FsOperation::Copy {
                    src: EntryId(6),
                    from: TreePath::parse("src/c.txt"),
                    to: TreePath::parse("dst/c.txt"),
                },
            ],
        };

        assert_eq!(
            plan_labels(&plan, ConfirmDetail::Summary),
            ["RENAME 2 / COPY 3 / DELETE 1(to recycle bin)"]
        );
    }

    #[test]
    fn summary_detail_keeps_short_plans_expanded() {
        let plan = OperationPlan {
            ops: vec![FsOperation::Delete {
                id: EntryId(1),
                path: TreePath::parse("old.txt"),
            }],
        };
        assert_eq!(
            plan_labels(&plan, ConfirmDetail::Summary),
            ["DELETE old.txt (to recycle bin)"]
        );
    }
}
