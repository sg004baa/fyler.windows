//! 確認ダイアログとCommitReport表示。
//!
//! **絶対ルール1**: ここでの承認なしに実FSへ触れない。

use std::path::PathBuf;

use eframe::egui;
use fyler_core::pane::PaneId;
use fyler_core::path::TreePath;
use fyler_core::plan::{FsOperation, OperationPlan};
use fyler_core::report::{CommitReport, OpOutcome, OpResult};
use fyler_core::transfer::{
    DropEffect, ImportOp, ImportPlan, TransferKind, TransferOp, TransferPlan,
};
use fyler_core::validate::ValidateError;

use crate::theme;

/// ユーザーの選択。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmChoice {
    Approve,
    Cancel,
    OpenWithSelected(usize),
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PlanCounts {
    create: usize,
    rename: usize,
    move_count: usize,
    copy: usize,
    delete: usize,
}

impl From<&OperationPlan> for PlanCounts {
    fn from(plan: &OperationPlan) -> Self {
        let mut counts = Self::default();
        for operation in &plan.ops {
            match operation {
                FsOperation::Create { .. } => counts.create += 1,
                FsOperation::Move { from, to, .. } if from.parent() == to.parent() => {
                    counts.rename += 1;
                }
                FsOperation::Move { .. } => counts.move_count += 1,
                FsOperation::Copy { .. } => counts.copy += 1,
                FsOperation::Delete { .. } => counts.delete += 1,
            }
        }
        counts
    }
}

fn count_badge(ui: &mut egui::Ui, count: usize, noun: &str, color: egui::Color32) {
    egui::Frame::NONE
        .fill(theme::SURFACE_RAISED)
        .stroke(egui::Stroke::new(1.0, theme::BORDER))
        .corner_radius(egui::CornerRadius::same(4))
        .inner_margin(egui::Margin::symmetric(7, 3))
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(format!("{count} {noun}"))
                    .monospace()
                    .size(11.0)
                    .color(color),
            );
        });
}

fn summary_badge(ui: &mut egui::Ui, count: usize, noun: &str, color: egui::Color32) {
    if count > 0 {
        count_badge(ui, count, noun, color);
    }
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
            input.key_pressed(egui::Key::Enter),
            input.key_pressed(egui::Key::N),
            input.key_pressed(egui::Key::Escape),
        )
    });
    let counts = PlanCounts::from(plan);

    egui::Modal::new(egui::Id::new("save-plan-confirmation"))
        .show(ui.ctx(), |ui| {
            ui.set_min_width(520.0);
            ui.horizontal(|ui| {
                ui.heading("Review changes");
                count_badge(ui, plan.ops.len(), "operations", theme::TEXT_SECONDARY);
            });
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                summary_badge(ui, counts.create, "create", theme::GREEN);
                summary_badge(ui, counts.rename, "rename", theme::BLUE);
                summary_badge(ui, counts.copy, "copy", theme::BLUE);
                summary_badge(ui, counts.move_count, "move", theme::BLUE);
                summary_badge(ui, counts.delete, "delete", theme::RED);
            });
            ui.add_space(8.0);
            ui.separator();
            egui::ScrollArea::vertical()
                .max_height(320.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.add_space(4.0);
                    for (group_index, group) in plan_groups(plan, detail).iter().enumerate() {
                        if group_index > 0 {
                            ui.add_space(8.0);
                        }
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(group.kind.heading())
                                    .monospace()
                                    .size(10.0)
                                    .strong()
                                    .color(group.kind.color()),
                            );
                            if group.kind == OpKind::Delete {
                                ui.label(
                                    egui::RichText::new("·  recoverable from recycle bin")
                                        .size(10.0)
                                        .color(theme::TEXT_FAINT),
                                );
                            }
                        });
                        ui.add_space(2.0);
                        for entry in &group.entries {
                            ui.horizontal(|ui| {
                                ui.add_space(8.0);
                                ui.label(
                                    egui::RichText::new(group.kind.marker())
                                        .monospace()
                                        .size(12.0)
                                        .color(group.kind.color()),
                                );
                                ui.label(
                                    egui::RichText::new(entry)
                                        .monospace()
                                        .size(12.0)
                                        .color(theme::TEXT_SECONDARY),
                                );
                            });
                        }
                    }
                    if !overwrites.is_empty() {
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new("OVERWRITE")
                                .monospace()
                                .size(11.0)
                                .strong()
                                .color(theme::YELLOW),
                        );
                        for path in overwrites {
                            ui.label(
                                egui::RichText::new(path.to_string())
                                    .monospace()
                                    .size(12.0)
                                    .color(theme::YELLOW),
                            );
                        }
                    }
                    for warning in warnings {
                        ui.add_space(4.0);
                        ui.label(egui::RichText::new(warning).size(12.0).color(theme::YELLOW));
                    }
                });

            ui.separator();
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let approve_label = if overwrites.is_empty() {
                        format!("Apply {} changes  ↵", plan.ops.len())
                    } else {
                        format!("Overwrite + apply {}  ↵", plan.ops.len())
                    };
                    let approve_clicked = ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new(approve_label)
                                    .strong()
                                    .color(theme::CANVAS),
                            )
                            .fill(theme::ACCENT)
                            .stroke(egui::Stroke::new(1.0, theme::ACCENT)),
                        )
                        .clicked();
                    let cancel_clicked = ui
                        .add(egui::Button::new("Cancel  esc").frame(false))
                        .clicked();
                    confirm_choice_from_buttons(approve_clicked, cancel_clicked, key_choice)
                })
                .inner
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
            input.key_pressed(egui::Key::Enter),
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
                confirm_choice_from_buttons(approve_clicked, cancel_clicked, key_choice)
            })
            .inner
        })
        .inner
}

/// clipboard・inbound drop取り込みを既存の確認モーダルパターンで表示する。
pub fn draw_import_plan(
    ui: &mut egui::Ui,
    plan: &ImportPlan,
    overwrites: &[PathBuf],
) -> Option<ConfirmChoice> {
    let key_choice = ui.ctx().input(|input| {
        plan_choice_from_keys(
            input.key_pressed(egui::Key::Y),
            input.key_pressed(egui::Key::Enter),
            input.key_pressed(egui::Key::N),
            input.key_pressed(egui::Key::Escape),
        )
    });
    egui::Modal::new(egui::Id::new("import-plan-confirmation"))
        .show(ui.ctx(), |ui| {
            ui.heading("Confirm import");
            ui.add_space(8.0);
            for operation in &plan.ops {
                ui.monospace(import_operation_label(operation, Some(plan.effect)));
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
                confirm_choice_from_buttons(approve_clicked, cancel_clicked, key_choice)
            })
            .inner
        })
        .inner
}
/// 汎用の承認確認ダイアログ(shortcut作成・zip展開)。行はapp層で整形済み。
/// キー操作は他planダイアログと同じ(y/Enter=承認、n/Esc=キャンセル)。
pub fn draw_action_confirm(
    ui: &mut egui::Ui,
    title: &str,
    approve_label: &str,
    lines: &[String],
) -> Option<ConfirmChoice> {
    let key_choice = ui.ctx().input(|input| {
        plan_choice_from_keys(
            input.key_pressed(egui::Key::Y),
            input.key_pressed(egui::Key::Enter),
            input.key_pressed(egui::Key::N),
            input.key_pressed(egui::Key::Escape),
        )
    });

    egui::Modal::new(egui::Id::new("action-confirmation"))
        .show(ui.ctx(), |ui| {
            ui.heading(title);
            ui.add_space(8.0);
            for line in lines {
                ui.monospace(line);
            }
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let approve_clicked = ui.button(approve_label).clicked();
                let cancel_clicked = ui.button("Cancel (n / Esc)").clicked();
                confirm_choice_from_buttons(approve_clicked, cancel_clicked, key_choice)
            })
            .inner
        })
        .inner
}

/// 汎用の結果ダイアログ(zip展開結果)。行はapp層で整形済み。
/// `NG`で始まる行はエラー色で描画する(undo reportと同じ規約)。
pub fn draw_action_report(
    ui: &mut egui::Ui,
    title: &str,
    lines: &[String],
    any_failed: bool,
) -> bool {
    let dismiss_from_keyboard = ui
        .ctx()
        .input(|input| input.key_pressed(egui::Key::Enter) || input.key_pressed(egui::Key::Escape));

    egui::Modal::new(egui::Id::new("action-report"))
        .show(ui.ctx(), |ui| {
            ui.heading(title);
            ui.add_space(8.0);
            if any_failed {
                ui.colored_label(ui.visuals().warn_fg_color, "Some operations did not run.");
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

/// undo確認ダイアログを表示する。行はapp層で整形済み。
pub fn draw_undo_plan(ui: &mut egui::Ui, lines: &[String]) -> Option<ConfirmChoice> {
    let key_choice = ui.ctx().input(|input| {
        plan_choice_from_keys(
            input.key_pressed(egui::Key::Y),
            input.key_pressed(egui::Key::Enter),
            input.key_pressed(egui::Key::N),
            input.key_pressed(egui::Key::Escape),
        )
    });

    egui::Modal::new(egui::Id::new("undo-plan-confirmation"))
        .show(ui.ctx(), |ui| {
            ui.heading("Confirm undo");
            ui.add_space(8.0);
            for line in lines {
                if line.trim_start().starts_with("[Skipped]") {
                    ui.colored_label(ui.visuals().warn_fg_color, line);
                } else {
                    ui.monospace(line);
                }
            }
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let approve_clicked = ui.button("Undo (y)").clicked();
                let cancel_clicked = ui.button("Cancel (n / Esc)").clicked();
                confirm_choice_from_buttons(approve_clicked, cancel_clicked, key_choice)
            })
            .inner
        })
        .inner
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpKind {
    Create,
    Rename,
    Move,
    Copy,
    Delete,
}

impl OpKind {
    fn of(operation: &FsOperation) -> Self {
        match operation {
            FsOperation::Create { .. } => Self::Create,
            FsOperation::Move { from, to, .. } if from.parent() == to.parent() => Self::Rename,
            FsOperation::Move { .. } => Self::Move,
            FsOperation::Copy { .. } => Self::Copy,
            FsOperation::Delete { .. } => Self::Delete,
        }
    }

    fn heading(self) -> &'static str {
        match self {
            Self::Create => "CREATE",
            Self::Rename => "RENAME",
            Self::Move => "MOVE",
            Self::Copy => "COPY",
            Self::Delete => "DELETE",
        }
    }

    fn marker(self) -> &'static str {
        match self {
            Self::Create => "+",
            Self::Rename => "~",
            Self::Move | Self::Copy => "→",
            Self::Delete => "-",
        }
    }

    fn color(self) -> egui::Color32 {
        match self {
            Self::Create => theme::GREEN,
            Self::Rename | Self::Move | Self::Copy => theme::BLUE,
            Self::Delete => theme::RED,
        }
    }
}

struct OpGroup {
    kind: OpKind,
    entries: Vec<String>,
}

/// 操作を種別ごとにまとめ、見出し(種別)配下へ表示する内容を返す。
/// Full か 5件以下なら各操作を1行ずつ、それ以外は種別ごとの件数へ畳む。
fn plan_groups(plan: &OperationPlan, detail: ConfirmDetail) -> Vec<OpGroup> {
    let expanded = detail == ConfirmDetail::Full || plan.ops.len() <= 5;
    let mut groups: Vec<OpGroup> = Vec::new();
    for kind in [
        OpKind::Create,
        OpKind::Rename,
        OpKind::Move,
        OpKind::Copy,
        OpKind::Delete,
    ] {
        let matching: Vec<&FsOperation> = plan
            .ops
            .iter()
            .filter(|op| OpKind::of(op) == kind)
            .collect();
        if matching.is_empty() {
            continue;
        }
        let entries = if expanded {
            matching.iter().map(|op| operation_entry(op)).collect()
        } else {
            let count = matching.len();
            vec![format!("{count} item{}", if count == 1 { "" } else { "s" })]
        };
        groups.push(OpGroup { kind, entries });
    }
    groups
}

/// 操作の変更内容(種別語を除いたパス表現)。
fn operation_entry(operation: &FsOperation) -> String {
    match operation {
        FsOperation::Create { path, .. } | FsOperation::Delete { path, .. } => path.to_string(),
        FsOperation::Move { from, to, .. } | FsOperation::Copy { from, to, .. } => {
            format!("{from} → {to}")
        }
    }
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
    let succeeded = report
        .results
        .iter()
        .filter(|result| matches!(result.outcome, OpOutcome::Success))
        .count();
    let failed = report
        .results
        .iter()
        .filter(|result| matches!(result.outcome, OpOutcome::Failed { .. }))
        .count();
    let skipped = report.results.len().saturating_sub(succeeded + failed);

    egui::Modal::new(egui::Id::new("save-commit-report"))
        .show(ui.ctx(), |ui| {
            ui.set_min_width(460.0);
            ui.heading(if failed == 0 {
                "Changes applied"
            } else {
                "Applied with errors"
            });
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                summary_badge(ui, succeeded, "done", theme::GREEN);
                summary_badge(ui, failed, "failed", theme::RED);
                summary_badge(ui, skipped, "skipped", theme::YELLOW);
            });
            ui.add_space(8.0);
            ui.separator();
            egui::ScrollArea::vertical()
                .max_height(300.0)
                .show(ui, |ui| {
                    for result in &report.results {
                        let (prefix, color) = match &result.outcome {
                            OpOutcome::Success => ("✓", theme::GREEN),
                            OpOutcome::Failed { .. } => ("×", theme::RED),
                            OpOutcome::Skipped { .. } => ("·", theme::YELLOW),
                        };
                        ui.label(
                            egui::RichText::new(format!("{prefix}  {}", report_label(result)))
                                .monospace()
                                .size(12.0)
                                .color(color),
                        );
                    }
                });
            ui.separator();
            ui.add_space(6.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add(
                    egui::Button::new(egui::RichText::new("Done  ↵").strong().color(theme::CANVAS))
                        .fill(theme::ACCENT)
                        .stroke(egui::Stroke::new(1.0, theme::ACCENT)),
                )
                .clicked()
                    || dismiss_from_keyboard
            })
            .inner
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

/// clipboard・inbound drop取り込みのCommitReportを既存reportモーダルで表示する。
pub fn draw_import_report(
    ui: &mut egui::Ui,
    report: &CommitReport<ImportOp>,
    effect: DropEffect,
) -> bool {
    let dismiss_from_keyboard = ui
        .ctx()
        .input(|input| input.key_pressed(egui::Key::Enter) || input.key_pressed(egui::Key::Escape));
    egui::Modal::new(egui::Id::new("import-commit-report"))
        .show(ui.ctx(), |ui| {
            ui.heading("Import result");
            ui.add_space(8.0);
            for result in &report.results {
                let label = outcome_label(
                    import_operation_label(&result.op, Some(effect)),
                    &result.outcome,
                );
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
            input.key_pressed(egui::Key::Enter),
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
                confirm_choice_from_buttons(discard_clicked, keep_clicked, key_choice)
            })
            .inner
        })
        .inner
}

/// OLE drag-out完了後、Move報告でsourceが残存している場合の後始末確認。
///
/// **絶対ルール1**: 承認なしにsourceを消さない。承認 = ごみ箱へ退避
/// (実FSの削除・上書きではない)。target(Explorer等)が既に消していた
/// (optimized move)場合はこのダイアログ自体が呼ばれない。
pub fn draw_drag_cleanup_confirm(ui: &mut egui::Ui, paths: &[PathBuf]) -> Option<ConfirmChoice> {
    let key_choice = ui.ctx().input(|input| {
        plan_choice_from_keys(
            input.key_pressed(egui::Key::Y),
            input.key_pressed(egui::Key::Enter),
            input.key_pressed(egui::Key::N),
            input.key_pressed(egui::Key::Escape),
        )
    });

    egui::Modal::new(egui::Id::new("drag-cleanup-confirm"))
        .show(ui.ctx(), |ui| {
            ui.heading("Finish move");
            ui.add_space(8.0);
            ui.colored_label(
                ui.visuals().warn_fg_color,
                "The drop target reported a move, but these items are still present. \
                 Move them to the Recycle Bin to finish?",
            );
            ui.add_space(4.0);
            for path in paths {
                ui.monospace(path.display().to_string());
            }
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                let approve_clicked = ui.button("Move to Recycle Bin (y)").clicked();
                let cancel_clicked = ui.button("Leave in place (n / Esc)").clicked();
                confirm_choice_from_buttons(approve_clicked, cancel_clicked, key_choice)
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
    let queued = total
        .saturating_sub(completed)
        .saturating_sub(usize::from(current.is_some()));

    egui::Modal::new(egui::Id::new("save-apply-progress"))
        .show(ui.ctx(), |ui| {
            ui.set_min_width(480.0);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.heading("Applying changes");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(format!("{completed} / {total}"))
                            .monospace()
                            .size(12.0)
                            .color(theme::TEXT_MUTED),
                    );
                });
            });
            if let Some(current) = current {
                ui.add_space(10.0);
                ui.label(
                    egui::RichText::new(current)
                        .monospace()
                        .size(12.0)
                        .color(theme::TEXT_SECONDARY),
                );
            }
            ui.add_space(10.0);
            ui.add(
                egui::ProgressBar::new(fraction.clamp(0.0, 1.0))
                    .fill(theme::BLUE)
                    .desired_width(480.0)
                    .show_percentage(),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!("{completed} done"))
                        .monospace()
                        .size(11.0)
                        .color(theme::GREEN),
                );
                if current.is_some() {
                    ui.label(
                        egui::RichText::new("·  1 in progress")
                            .monospace()
                            .size(11.0)
                            .color(theme::TEXT_MUTED),
                    );
                }
                ui.label(
                    egui::RichText::new(format!("·  {queued} queued"))
                        .monospace()
                        .size(11.0)
                        .color(theme::TEXT_MUTED),
                );
            });
            ui.add_space(10.0);
            ui.separator();
            if cancel_requested {
                ui.label(
                    egui::RichText::new(
                        "Cancel requested; stopping after the current operation finishes",
                    )
                    .size(11.0)
                    .color(theme::YELLOW),
                );
            }
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Do not close the window until this finishes")
                        .size(11.0)
                        .color(theme::TEXT_FAINT),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_enabled(
                        !cancel_requested,
                        egui::Button::new("Cancel remaining  esc"),
                    )
                    .clicked()
                })
                .inner
            })
            .inner
                || cancel_from_keyboard
        })
        .inner
}

/// root scanまたは再帰directory loadを、総数不明の進捗として表示する。
pub fn draw_loader_progress(
    ui: &mut egui::Ui,
    title: &str,
    path: &std::path::Path,
    entries: usize,
    cancel_requested: bool,
) -> bool {
    let cancel_from_keyboard =
        !cancel_requested && ui.ctx().input(|input| input.key_pressed(egui::Key::Escape));

    egui::Modal::new(egui::Id::new("loader-progress"))
        .show(ui.ctx(), |ui| {
            ui.heading(title);
            ui.add_space(8.0);
            ui.spinner();
            ui.monospace(path.display().to_string());
            ui.label(format!("{entries} entries found"));

            ui.add_space(12.0);
            if cancel_requested {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    "Cancel requested; stopping the scan",
                );
            }
            let cancel_clicked = ui
                .add_enabled(!cancel_requested, egui::Button::new("Cancel (Esc)"))
                .clicked();
            cancel_clicked || cancel_from_keyboard
        })
        .inner
}

/// open-with候補をモーダルで表示し、選択があれば返す。
///
/// `choices` はapp層が構築した表示名の一覧で、末尾にOS標準ダイアログ項目を
/// 含めたものをそのまま描画する。選択行の更新値は戻り値の2要素目で返す。
pub fn draw_open_with(
    ui: &mut egui::Ui,
    file_name: &str,
    choices: &[String],
    selected: usize,
) -> (Option<ConfirmChoice>, Option<usize>) {
    let key_input = ui.ctx().input(|input| {
        let down = input.key_pressed(egui::Key::J) || input.key_pressed(egui::Key::ArrowDown);
        let up = input.key_pressed(egui::Key::K) || input.key_pressed(egui::Key::ArrowUp);
        let confirm = input.key_pressed(egui::Key::Enter);
        let cancel = input.key_pressed(egui::Key::Escape) || input.key_pressed(egui::Key::Q);
        (down, up, confirm, cancel)
    });
    let mut next_selected = None;
    if key_input.0 {
        next_selected = Some(open_with_selection_step(choices.len(), selected, 1));
    } else if key_input.1 {
        next_selected = Some(open_with_selection_step(choices.len(), selected, -1));
    }
    let effective_selected = next_selected.unwrap_or(selected);
    let keyboard_choice = if key_input.2 && !choices.is_empty() {
        Some(ConfirmChoice::OpenWithSelected(effective_selected))
    } else if key_input.3 {
        Some(ConfirmChoice::Cancel)
    } else {
        None
    };

    let choice = egui::Modal::new(egui::Id::new("open-with-selection"))
        .show(ui.ctx(), |ui| {
            ui.heading("Open with");
            ui.add_space(4.0);
            ui.monospace(file_name);
            ui.add_space(8.0);
            ui.set_width(ui.ctx().content_rect().width() * 0.3);

            let mut clicked_choice = None;
            for (index, choice) in choices.iter().enumerate() {
                let response = ui.selectable_label(index == effective_selected, choice);
                if response.hovered() {
                    next_selected = Some(index);
                }
                if response.clicked() {
                    clicked_choice = Some(ConfirmChoice::OpenWithSelected(index));
                }
            }

            ui.add_space(12.0);
            let open_clicked = ui
                .add_enabled(!choices.is_empty(), egui::Button::new("Open (Enter)"))
                .clicked();
            let cancel_clicked = ui.button("Cancel (Esc / q)").clicked();
            clicked_choice
                .or_else(|| {
                    open_clicked.then_some(ConfirmChoice::OpenWithSelected(effective_selected))
                })
                .or_else(|| cancel_clicked.then_some(ConfirmChoice::Cancel))
                .or(keyboard_choice)
        })
        .inner;

    (choice, next_selected)
}

/// open-with候補の選択位置を上下移動する。端では止まり、wrapしない。
pub fn open_with_selection_step(len: usize, selected: usize, delta: i32) -> usize {
    if len == 0 {
        return 0;
    }
    let selected = selected.min(len - 1);
    if delta < 0 {
        selected.saturating_sub(delta.unsigned_abs() as usize)
    } else {
        selected
            .saturating_add(delta as usize)
            .min(len.saturating_sub(1))
    }
}

/// plan確認キーの押下状態を、エンジン非依存の確認結果へ変換する。
fn plan_choice_from_keys(y: bool, enter: bool, n: bool, esc: bool) -> Option<ConfirmChoice> {
    if y || enter {
        Some(ConfirmChoice::Approve)
    } else if n || esc {
        Some(ConfirmChoice::Cancel)
    } else {
        None
    }
}

fn confirm_choice_from_buttons(
    approve_clicked: bool,
    cancel_clicked: bool,
    key_choice: Option<ConfirmChoice>,
) -> Option<ConfirmChoice> {
    if approve_clicked {
        Some(ConfirmChoice::Approve)
    } else if cancel_clicked {
        Some(ConfirmChoice::Cancel)
    } else {
        key_choice
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

/// import(clipboard・inbound drop)操作を確認・進捗・結果ダイアログで共通利用する
/// 表示ラベルへ変換する。effectが不明な進捗表示中は`IMPORT`を使う。
pub(crate) fn import_operation_label(operation: &ImportOp, effect: Option<DropEffect>) -> String {
    let kind = match effect {
        Some(DropEffect::Copy) => "COPY",
        Some(DropEffect::Move) => "MOVE",
        None => "IMPORT",
    };
    format!(
        "{kind} {} → {}",
        operation.source.display(),
        operation.target.display()
    )
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
        | ValidateError::TargetOccupiedByDirectory { .. }
        | ValidateError::IncompleteDirectory { .. } => None,
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
            plan_choice_from_keys(true, false, false, false),
            Some(ConfirmChoice::Approve)
        );
        assert_eq!(
            plan_choice_from_keys(false, true, false, false),
            Some(ConfirmChoice::Approve)
        );
        assert_eq!(
            plan_choice_from_keys(false, false, true, false),
            Some(ConfirmChoice::Cancel)
        );
        assert_eq!(
            plan_choice_from_keys(false, false, false, true),
            Some(ConfirmChoice::Cancel)
        );
        assert_eq!(plan_choice_from_keys(false, false, false, false), None);
    }

    #[test]
    fn confirm_buttons_take_priority_over_keyboard_choice() {
        assert_eq!(
            confirm_choice_from_buttons(false, true, Some(ConfirmChoice::Approve)),
            Some(ConfirmChoice::Cancel)
        );
        assert_eq!(
            confirm_choice_from_buttons(true, false, Some(ConfirmChoice::Cancel)),
            Some(ConfirmChoice::Approve)
        );
        assert_eq!(
            confirm_choice_from_buttons(false, false, Some(ConfirmChoice::Approve)),
            Some(ConfirmChoice::Approve)
        );
    }

    #[test]
    fn open_with_selection_step_saturates_without_wrapping() {
        assert_eq!(open_with_selection_step(3, 0, -1), 0);
        assert_eq!(open_with_selection_step(3, 2, 1), 2);
        assert_eq!(open_with_selection_step(3, 1, 1), 2);
        assert_eq!(open_with_selection_step(3, 1, -1), 0);
        assert_eq!(open_with_selection_step(0, 10, 1), 0);
        assert_eq!(open_with_selection_step(3, 10, -1), 1);
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
            PlanCounts::from(&plan),
            PlanCounts {
                rename: 2,
                copy: 3,
                delete: 1,
                ..PlanCounts::default()
            }
        );

        let groups = plan_groups(&plan, ConfirmDetail::Summary)
            .into_iter()
            .map(|group| (group.kind, group.entries))
            .collect::<Vec<_>>();
        assert_eq!(
            groups,
            vec![
                (OpKind::Rename, vec!["2 items".to_owned()]),
                (OpKind::Copy, vec!["3 items".to_owned()]),
                (OpKind::Delete, vec!["1 item".to_owned()]),
            ]
        );
    }

    #[test]
    fn short_plans_list_each_operation_under_its_kind() {
        let plan = OperationPlan {
            ops: vec![FsOperation::Delete {
                id: EntryId(1),
                path: TreePath::parse("old.txt"),
            }],
        };
        let groups = plan_groups(&plan, ConfirmDetail::Summary);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].kind, OpKind::Delete);
        assert_eq!(groups[0].entries, vec!["old.txt".to_owned()]);
    }
}
