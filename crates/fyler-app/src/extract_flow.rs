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
use fyler_core::report::CommitReport;
use fyler_fsops::extract::{ExtractOp, ExtractPlan};
use fyler_gui::confirm::ConfirmChoice;

#[derive(Debug)]
enum ExtractState {
    Idle,
    Awaiting { pane: PaneId, plan: ExtractPlan },
    Running { pane: PaneId, cancel: Arc<AtomicBool> },
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
}
