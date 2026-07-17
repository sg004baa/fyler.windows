//! zipアーカイブ展開(:extract / context menu「Extract here」)のapp層フロー。
//!
//! [`crate::import_flow`]と同じ確認→実行の直列化パターンを、単一paneの
//! zip展開へ適用する。planは[`fyler_fsops::extract::preflight_extract`]が
//! 生成済みのものを受け取り、承認後にworkerで
//! [`fyler_fsops::extract::apply_extract_cancellable`]を実行する。
//! 展開はundo journal対象外(`fyler_fsops::extract`のdoc参照)。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use fyler_core::pane::PaneId;
use fyler_core::report::{CommitReport, OpOutcome};
use fyler_fsops::extract::{ExtractOp, ExtractPlan};
use fyler_gui::confirm::ConfirmChoice;

use crate::transfer_flow::TransferPaneState;

/// zip展開を開始してよいかを判定する。
/// [`crate::transfer_flow::start_rejection`]と同じ意味論を単一paneへ適用する。
pub(super) fn start_rejection(
    pane: TransferPaneState,
    globally_busy: bool,
) -> Option<&'static str> {
    if globally_busy {
        Some("Another save, transfer, or import is in progress")
    } else if pane.offline {
        Some("Cannot extract into an offline or unreachable pane")
    } else if pane.crashed {
        Some("Cannot extract into a pane that has stopped")
    } else if pane.dirty {
        Some("Cannot extract into a pane being edited. Save or discard changes first.")
    } else if !pane.idle {
        Some("Cannot extract into a pane that is saving")
    } else {
        None
    }
}

/// zip展開の確認ダイアログに渡す表示行を構築する。
pub(super) fn confirm_lines(plan: &ExtractPlan) -> Vec<String> {
    let entry_noun = if plan.ops.len() == 1 {
        "entry"
    } else {
        "entries"
    };
    let approx_mb = (plan.total_bytes as f64 / (1024.0 * 1024.0)).max(0.01);
    vec![format!(
        "EXTRACT {} \u{2192} {}/ ({} {entry_noun}, ~{approx_mb:.1} MB)",
        plan.archive.display(),
        plan.dest_dir.display(),
        plan.ops.len(),
    )]
}

/// zip展開結果を表示行へ整形する(`NG`始まりはエラー色描画の規約に合わせる)。
pub(super) fn report_lines(report: &CommitReport<ExtractOp>) -> (Vec<String>, bool) {
    let lines = report
        .results
        .iter()
        .map(|result| {
            let label = format!("EXTRACT {}", result.op.name);
            match &result.outcome {
                OpOutcome::Success => format!("OK  {label}"),
                OpOutcome::Failed { error, progress } => {
                    let progress = progress
                        .as_deref()
                        .map(|progress| format!(" / progress: {progress}"))
                        .unwrap_or_default();
                    format!("NG  {label} (reason: {error}{progress})")
                }
                OpOutcome::Skipped { reason } => format!("--  SKIP {label} (reason: {reason})"),
            }
        })
        .collect::<Vec<_>>();
    (lines, report.any_failed())
}
#[derive(Debug)]
enum ExtractState {
    Idle,
    Awaiting {
        pane: PaneId,
        plan: ExtractPlan,
    },
    Running {
        pane: PaneId,
        cancel: Arc<AtomicBool>,
    },
}

#[derive(Debug)]
pub(super) enum ExtractFlowResult {
    StartApply {
        pane: PaneId,
        plan: ExtractPlan,
        cancel: Arc<AtomicBool>,
    },
    Cancelled,
    CancelRequested,
    Finished {
        pane: PaneId,
        report: CommitReport<ExtractOp>,
    },
    Ignored,
}

/// 確認待ち→実行中→完了の状態機械。[`crate::import_flow::ImportController`]と
/// 同じ契約(同時に1フローのみ、キャンセルは操作間)。
#[derive(Debug)]
pub(super) struct ExtractController {
    state: ExtractState,
}

impl ExtractController {
    pub fn new() -> Self {
        Self {
            state: ExtractState::Idle,
        }
    }

    pub fn begin(&mut self, pane: PaneId, plan: ExtractPlan) {
        debug_assert!(matches!(self.state, ExtractState::Idle));
        self.state = ExtractState::Awaiting { pane, plan };
    }

    pub fn on_choice(&mut self, choice: ConfirmChoice) -> ExtractFlowResult {
        if let ExtractState::Running { cancel, .. } = &self.state {
            if choice == ConfirmChoice::Cancel {
                cancel.store(true, Ordering::Relaxed);
                return ExtractFlowResult::CancelRequested;
            }
            return ExtractFlowResult::Ignored;
        }
        let state = std::mem::replace(&mut self.state, ExtractState::Idle);
        let ExtractState::Awaiting { pane, plan } = state else {
            return ExtractFlowResult::Ignored;
        };
        if choice == ConfirmChoice::Cancel {
            return ExtractFlowResult::Cancelled;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.state = ExtractState::Running {
            pane,
            cancel: Arc::clone(&cancel),
        };
        ExtractFlowResult::StartApply { pane, plan, cancel }
    }

    pub fn on_finished(&mut self, report: CommitReport<ExtractOp>) -> ExtractFlowResult {
        let state = std::mem::replace(&mut self.state, ExtractState::Idle);
        let ExtractState::Running { pane, .. } = state else {
            return ExtractFlowResult::Ignored;
        };
        ExtractFlowResult::Finished { pane, report }
    }

    pub fn invalidate_if_involves(&mut self, pane: PaneId) -> bool {
        let involves = matches!(self.state, ExtractState::Awaiting { pane: p, .. } if p == pane);
        if involves {
            self.state = ExtractState::Idle;
        }
        involves
    }

    pub fn is_awaiting(&self) -> bool {
        matches!(self.state, ExtractState::Awaiting { .. })
    }

    pub fn is_running(&self) -> bool {
        matches!(self.state, ExtractState::Running { .. })
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use fyler_core::report::OpResult;

    use super::*;

    fn plan() -> ExtractPlan {
        ExtractPlan {
            archive: PathBuf::from("C:/src/a.zip"),
            dest_dir: PathBuf::from("C:/src/a"),
            ops: Vec::new(),
            total_bytes: 0,
        }
    }

    #[test]
    fn happy_path_moves_through_states() {
        let mut controller = ExtractController::new();
        let pane = PaneId::new(1);
        controller.begin(pane, plan());
        assert!(controller.is_awaiting());

        let ExtractFlowResult::StartApply {
            pane: started_pane,
            cancel,
            ..
        } = controller.on_choice(ConfirmChoice::Approve)
        else {
            panic!("expected StartApply");
        };
        assert_eq!(started_pane, pane);
        assert!(controller.is_running());
        assert!(!cancel.load(Ordering::Relaxed));

        let report = CommitReport {
            results: Vec::new(),
        };
        let ExtractFlowResult::Finished {
            pane: finished_pane,
            ..
        } = controller.on_finished(report)
        else {
            panic!("expected Finished");
        };
        assert_eq!(finished_pane, pane);
        assert!(!controller.is_awaiting());
        assert!(!controller.is_running());
    }

    #[test]
    fn cancel_choice_while_awaiting_returns_to_idle() {
        let mut controller = ExtractController::new();
        let pane = PaneId::new(1);
        controller.begin(pane, plan());
        assert!(matches!(
            controller.on_choice(ConfirmChoice::Cancel),
            ExtractFlowResult::Cancelled
        ));
        assert!(!controller.is_awaiting());
        assert!(!controller.is_running());
    }

    #[test]
    fn cancel_choice_while_running_requests_cancellation_without_leaving_running() {
        let mut controller = ExtractController::new();
        let pane = PaneId::new(1);
        controller.begin(pane, plan());
        let ExtractFlowResult::StartApply { cancel, .. } =
            controller.on_choice(ConfirmChoice::Approve)
        else {
            panic!("expected StartApply");
        };
        assert!(matches!(
            controller.on_choice(ConfirmChoice::Cancel),
            ExtractFlowResult::CancelRequested
        ));
        assert!(cancel.load(Ordering::Relaxed));
        assert!(controller.is_running());
    }

    #[test]
    fn invalidate_if_involves_only_clears_awaiting_matching_pane() {
        let mut controller = ExtractController::new();
        let pane = PaneId::new(1);
        let other = PaneId::new(2);
        controller.begin(pane, plan());
        assert!(!controller.invalidate_if_involves(other));
        assert!(controller.is_awaiting());
        assert!(controller.invalidate_if_involves(pane));
        assert!(!controller.is_awaiting());
    }

    #[test]
    fn start_rejection_orders_checks() {
        let idle_pane = TransferPaneState {
            dirty: false,
            idle: true,
            crashed: false,
            offline: false,
        };
        assert_eq!(
            start_rejection(idle_pane, true),
            Some("Another save, transfer, or import is in progress")
        );
        assert_eq!(
            start_rejection(
                TransferPaneState {
                    offline: true,
                    ..idle_pane
                },
                false
            ),
            Some("Cannot extract into an offline or unreachable pane")
        );
        assert_eq!(
            start_rejection(
                TransferPaneState {
                    crashed: true,
                    ..idle_pane
                },
                false
            ),
            Some("Cannot extract into a pane that has stopped")
        );
        assert_eq!(
            start_rejection(
                TransferPaneState {
                    dirty: true,
                    ..idle_pane
                },
                false
            ),
            Some("Cannot extract into a pane being edited. Save or discard changes first.")
        );
        assert_eq!(
            start_rejection(
                TransferPaneState {
                    idle: false,
                    ..idle_pane
                },
                false
            ),
            Some("Cannot extract into a pane that is saving")
        );
        assert_eq!(start_rejection(idle_pane, false), None);
    }

    #[test]
    fn confirm_lines_reports_entry_count_and_size() {
        let mut single = plan();
        single.ops.push(ExtractOp {
            name: "a.txt".to_owned(),
            target: PathBuf::from("C:/src/a/a.txt"),
            is_dir: false,
        });
        single.total_bytes = 2 * 1024 * 1024;
        let lines = confirm_lines(&single);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("EXTRACT C:/src/a.zip"));
        assert!(lines[0].contains("1 entry"));
        assert!(lines[0].contains("2.0 MB"));

        let mut multi = plan();
        multi.ops.push(ExtractOp {
            name: "a.txt".to_owned(),
            target: PathBuf::from("C:/src/a/a.txt"),
            is_dir: false,
        });
        multi.ops.push(ExtractOp {
            name: "b.txt".to_owned(),
            target: PathBuf::from("C:/src/a/b.txt"),
            is_dir: false,
        });
        assert!(confirm_lines(&multi)[0].contains("2 entries"));
    }

    #[test]
    fn report_lines_marks_failures_with_ng_prefix() {
        let report = CommitReport {
            results: vec![
                OpResult {
                    op: ExtractOp {
                        name: "a.txt".to_owned(),
                        target: PathBuf::from("C:/src/a/a.txt"),
                        is_dir: false,
                    },
                    outcome: OpOutcome::Success,
                },
                OpResult {
                    op: ExtractOp {
                        name: "b.txt".to_owned(),
                        target: PathBuf::from("C:/src/a/b.txt"),
                        is_dir: false,
                    },
                    outcome: OpOutcome::Failed {
                        error: "disk full".to_owned(),
                        progress: None,
                    },
                },
            ],
        };
        let (lines, any_failed) = report_lines(&report);
        assert!(any_failed);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("OK  EXTRACT a.txt"));
        assert!(lines[1].starts_with("NG  EXTRACT b.txt"));
        assert!(lines[1].contains("disk full"));
    }
}
