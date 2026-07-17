//! Windows Shell clipboard(CF_HDROP)・inbound dropの取り込みapp層フロー。
//!
//! [`crate::transfer_flow`]と同じ確認→実行の直列化パターンを、外部source
//! (Explorer clipboard・inbound drop)から現在paneのdestinationへ取り込む
//! ケースへ適用する。pane間transferと異なりsourceは常に単一の外部パス集合、
//! targetは常に単一pane(paste/dropを発火したpane)。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use fyler_core::pane::PaneId;
use fyler_core::report::CommitReport;
use fyler_core::transfer::{DropEffect, ImportOp, ImportPlan};
use fyler_gui::confirm::ConfirmChoice;

use crate::transfer_flow::TransferPaneState;

/// clipboard copy/cut/paste・inbound dropを開始してよいかを判定する。
/// [`crate::transfer_flow::start_rejection`]と同じ意味論を単一paneへ適用する。
pub(super) fn start_rejection(
    pane: TransferPaneState,
    globally_busy: bool,
) -> Option<&'static str> {
    if globally_busy {
        Some("Another save, transfer, or import is in progress")
    } else if pane.offline {
        Some("Cannot use the clipboard with an offline or unreachable pane")
    } else if pane.crashed {
        Some("Cannot use the clipboard with a stopped pane")
    } else if pane.dirty {
        Some("Cannot use the clipboard with a pane being edited. Save or discard changes first.")
    } else if !pane.idle {
        Some("Cannot use the clipboard with a pane that is saving")
    } else {
        None
    }
}

#[derive(Debug)]
enum ImportState {
    Idle,
    Awaiting {
        pane: PaneId,
        plan: ImportPlan,
        overwrites: Vec<PathBuf>,
    },
    Running {
        pane: PaneId,
        effect: DropEffect,
        cancel: Arc<AtomicBool>,
    },
}

#[derive(Debug)]
pub(super) enum ImportFlowResult {
    StartApply {
        pane: PaneId,
        plan: ImportPlan,
        overwrites: std::collections::HashSet<PathBuf>,
        cancel: Arc<AtomicBool>,
    },
    Cancelled,
    CancelRequested,
    Finished {
        pane: PaneId,
        effect: DropEffect,
        report: CommitReport<ImportOp>,
    },
    Ignored,
}

#[derive(Debug)]
pub(super) struct ImportController {
    state: ImportState,
}

impl ImportController {
    pub fn new() -> Self {
        Self {
            state: ImportState::Idle,
        }
    }

    pub fn begin(&mut self, pane: PaneId, plan: ImportPlan, overwrites: Vec<PathBuf>) {
        debug_assert!(matches!(self.state, ImportState::Idle));
        self.state = ImportState::Awaiting {
            pane,
            plan,
            overwrites,
        };
    }

    pub fn on_choice(&mut self, choice: ConfirmChoice) -> ImportFlowResult {
        if let ImportState::Running { cancel, .. } = &self.state {
            if choice == ConfirmChoice::Cancel {
                cancel.store(true, Ordering::Relaxed);
                return ImportFlowResult::CancelRequested;
            }
            return ImportFlowResult::Ignored;
        }
        let state = std::mem::replace(&mut self.state, ImportState::Idle);
        let ImportState::Awaiting {
            pane,
            plan,
            overwrites,
        } = state
        else {
            return ImportFlowResult::Ignored;
        };
        if choice == ConfirmChoice::Cancel {
            return ImportFlowResult::Cancelled;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let effect = plan.effect;
        self.state = ImportState::Running {
            pane,
            effect,
            cancel: Arc::clone(&cancel),
        };
        ImportFlowResult::StartApply {
            pane,
            plan,
            overwrites: overwrites.into_iter().collect(),
            cancel,
        }
    }

    pub fn on_finished(&mut self, report: CommitReport<ImportOp>) -> ImportFlowResult {
        let state = std::mem::replace(&mut self.state, ImportState::Idle);
        let ImportState::Running { pane, effect, .. } = state else {
            return ImportFlowResult::Ignored;
        };
        ImportFlowResult::Finished {
            pane,
            effect,
            report,
        }
    }

    pub fn invalidate_if_involves(&mut self, pane: PaneId) -> bool {
        let involves = matches!(self.state, ImportState::Awaiting { pane: p, .. } if p == pane);
        if involves {
            self.state = ImportState::Idle;
        }
        involves
    }

    pub fn is_awaiting(&self) -> bool {
        matches!(self.state, ImportState::Awaiting { .. })
    }

    pub fn is_running(&self) -> bool {
        matches!(self.state, ImportState::Running { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane_ok() -> TransferPaneState {
        TransferPaneState {
            dirty: false,
            idle: true,
            crashed: false,
            offline: false,
        }
    }

    #[test]
    fn start_rejection_blocks_when_globally_busy() {
        assert_eq!(
            start_rejection(pane_ok(), true),
            Some("Another save, transfer, or import is in progress")
        );
    }

    #[test]
    fn start_rejection_blocks_offline_crashed_dirty_and_non_idle_panes() {
        let mut state = pane_ok();
        state.offline = true;
        assert!(start_rejection(state, false).is_some());

        let mut state = pane_ok();
        state.crashed = true;
        assert!(start_rejection(state, false).is_some());

        let mut state = pane_ok();
        state.dirty = true;
        assert!(start_rejection(state, false).is_some());

        let mut state = pane_ok();
        state.idle = false;
        assert!(start_rejection(state, false).is_some());
    }

    #[test]
    fn start_rejection_allows_idle_pane() {
        assert_eq!(start_rejection(pane_ok(), false), None);
    }

    fn plan(pane: PaneId) -> ImportPlan {
        let _ = pane;
        ImportPlan::build(
            vec![PathBuf::from("C:/src/a.txt")],
            PathBuf::from("C:/dest"),
            DropEffect::Copy,
        )
    }

    #[test]
    fn happy_path_moves_through_states() {
        let mut controller = ImportController::new();
        let pane = PaneId::new(1);
        controller.begin(pane, plan(pane), Vec::new());
        assert!(controller.is_awaiting());

        let ImportFlowResult::StartApply {
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
        let ImportFlowResult::Finished {
            pane: finished_pane,
            effect,
            ..
        } = controller.on_finished(report)
        else {
            panic!("expected Finished");
        };
        assert_eq!(finished_pane, pane);
        assert_eq!(effect, DropEffect::Copy);
        assert!(!controller.is_awaiting());
        assert!(!controller.is_running());
    }

    #[test]
    fn cancel_choice_while_awaiting_returns_to_idle() {
        let mut controller = ImportController::new();
        let pane = PaneId::new(1);
        controller.begin(pane, plan(pane), Vec::new());
        assert!(matches!(
            controller.on_choice(ConfirmChoice::Cancel),
            ImportFlowResult::Cancelled
        ));
        assert!(!controller.is_awaiting());
        assert!(!controller.is_running());
    }

    #[test]
    fn cancel_choice_while_running_requests_cancellation_without_leaving_running() {
        let mut controller = ImportController::new();
        let pane = PaneId::new(1);
        controller.begin(pane, plan(pane), Vec::new());
        let ImportFlowResult::StartApply { cancel, .. } =
            controller.on_choice(ConfirmChoice::Approve)
        else {
            panic!("expected StartApply");
        };
        assert!(matches!(
            controller.on_choice(ConfirmChoice::Cancel),
            ImportFlowResult::CancelRequested
        ));
        assert!(cancel.load(Ordering::Relaxed));
        assert!(controller.is_running());
    }

    #[test]
    fn invalidate_if_involves_only_clears_awaiting_matching_pane() {
        let mut controller = ImportController::new();
        let pane = PaneId::new(1);
        let other = PaneId::new(2);
        controller.begin(pane, plan(pane), Vec::new());
        assert!(!controller.invalidate_if_involves(other));
        assert!(controller.is_awaiting());
        assert!(controller.invalidate_if_involves(pane));
        assert!(!controller.is_awaiting());
    }
}
