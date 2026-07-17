//! OLE drag-out(fylerのentryを外部Shellターゲットへdragする)のapp層フロー。
//!
//! [`crate::import_flow`]・[`crate::transfer_flow`]と同じ確認→実行の直列化
//! パターンを踏襲する。ただし承認が必要なのは開始時ではなく、
//! [`fyler_fsops::drag::perform_drag`]完了後のMove報告に対する後始末
//! (source側のごみ箱退避)だけ(絶対ルール1: 承認なしにsourceを消さない)。
//! それ以外(drag開始・進行中)は実FSへ一切触れないため確認不要。

use std::path::PathBuf;

use fyler_core::pane::PaneId;
use fyler_core::transfer::DragOutcome;
use fyler_gui::confirm::ConfirmChoice;

use crate::transfer_flow::TransferPaneState;

/// drag-outを開始してよいかを判定する。[`crate::transfer_flow::start_rejection`]
/// と同じ意味論を単一paneへ適用する(`crate::import_flow::start_rejection`と
/// 同型だが、文言はdrag向けに書き分ける)。
pub(super) fn start_rejection(
    pane: TransferPaneState,
    globally_busy: bool,
) -> Option<&'static str> {
    if globally_busy {
        Some("Another save, transfer, or drag is in progress")
    } else if pane.offline {
        Some("Cannot start a drag with an offline or unreachable pane")
    } else if pane.crashed {
        Some("Cannot start a drag with a stopped pane")
    } else if pane.dirty {
        Some("Cannot start a drag with a pane being edited. Save or discard changes first.")
    } else if !pane.idle {
        Some("Cannot start a drag with a pane that is saving")
    } else {
        None
    }
}

#[derive(Debug)]
enum DragOutState {
    Idle,
    /// 使い捨てSTAスレッドで`perform_drag`がblocking実行中。
    Running {
        pane_id: PaneId,
    },
    /// `perform_drag`完了、Move報告のsource後始末を確認中。
    ConfirmingCleanup {
        pane_id: PaneId,
        remaining: Vec<PathBuf>,
    },
    /// 承認後、ごみ箱退避workerが実行中。
    CleaningUp {
        pane_id: PaneId,
    },
}

#[derive(Debug)]
pub(super) enum DragOutFlowResult {
    /// Cancelled/Copy、またはMove報告だがsourceが既に消えている(optimized
    /// move)。busyを即解除してよい(reconcileはwatcher任せ)。
    Done,
    /// Move報告、かつsourceが残存。確認ダイアログを出す必要がある。
    NeedsCleanupConfirm {
        pane_id: PaneId,
        remaining: Vec<PathBuf>,
    },
    Ignored,
}

#[derive(Debug)]
pub(super) enum CleanupChoiceResult {
    /// 承認: 呼び出し側がごみ箱へ退避してから[`DragOutController::finish_cleanup`]
    /// を呼ぶこと。
    Approved {
        pane_id: PaneId,
        remaining: Vec<PathBuf>,
    },
    /// キャンセル: 何もせずbusyを解除してよい。
    Cancelled {
        pane_id: PaneId,
    },
    Ignored,
}

#[derive(Debug)]
pub(super) struct DragOutController {
    state: DragOutState,
}

impl DragOutController {
    pub fn new() -> Self {
        Self {
            state: DragOutState::Idle,
        }
    }

    pub fn begin(&mut self, pane_id: PaneId) {
        debug_assert!(matches!(self.state, DragOutState::Idle));
        self.state = DragOutState::Running { pane_id };
    }

    /// `Idle`でない間、他のsave/transfer/import busyゲートへ合流させる。
    pub fn is_busy(&self) -> bool {
        !matches!(self.state, DragOutState::Idle)
    }

    fn pane(&self) -> Option<PaneId> {
        match &self.state {
            DragOutState::Idle => None,
            DragOutState::Running { pane_id }
            | DragOutState::ConfirmingCleanup { pane_id, .. }
            | DragOutState::CleaningUp { pane_id } => Some(*pane_id),
        }
    }

    /// STAスレッドから[`DragOutcome`]が届いたときに呼ぶ。`existing`はdragした
    /// source絶対パスのうち、まだFS上に存在するもの(STAスレッドが
    /// `perform_drag`直後に確認済み)。
    pub fn on_outcome(
        &mut self,
        pane_id: PaneId,
        outcome: DragOutcome,
        existing: Vec<PathBuf>,
    ) -> DragOutFlowResult {
        let DragOutState::Running { pane_id: running } = &self.state else {
            return DragOutFlowResult::Ignored;
        };
        if *running != pane_id {
            // 別paneのdragに属する遅延イベント。無視する(controllerは単一drag
            // しか同時に持たないため取り違えないが、防御的に確認する)。
            return DragOutFlowResult::Ignored;
        }
        self.state = DragOutState::Idle;
        match outcome {
            DragOutcome::Cancelled => DragOutFlowResult::Done,
            DragOutcome::Dropped {
                move_reported: false,
                ..
            } => DragOutFlowResult::Done,
            DragOutcome::Dropped {
                move_reported: true,
                ..
            } => {
                if existing.is_empty() {
                    DragOutFlowResult::Done
                } else {
                    self.state = DragOutState::ConfirmingCleanup {
                        pane_id,
                        remaining: existing.clone(),
                    };
                    DragOutFlowResult::NeedsCleanupConfirm {
                        pane_id,
                        remaining: existing,
                    }
                }
            }
        }
    }

    pub fn is_confirming(&self) -> bool {
        matches!(self.state, DragOutState::ConfirmingCleanup { .. })
    }

    pub fn on_choice(&mut self, choice: ConfirmChoice) -> CleanupChoiceResult {
        let DragOutState::ConfirmingCleanup { pane_id, remaining } =
            std::mem::replace(&mut self.state, DragOutState::Idle)
        else {
            return CleanupChoiceResult::Ignored;
        };
        if choice == ConfirmChoice::Cancel {
            CleanupChoiceResult::Cancelled { pane_id }
        } else {
            self.state = DragOutState::CleaningUp { pane_id };
            CleanupChoiceResult::Approved { pane_id, remaining }
        }
    }

    /// ごみ箱退避workerの完了を受けて`Idle`へ戻す。
    pub fn finish_cleanup(&mut self, pane_id: PaneId) {
        if matches!(&self.state, DragOutState::CleaningUp { pane_id: p } if *p == pane_id) {
            self.state = DragOutState::Idle;
        }
    }

    /// paneが閉じられた・crashしたときに、関与する状態を強制的に`Idle`へ戻す。
    /// 戻り値は表示中のダイアログを閉じる必要があるか。
    pub fn invalidate_if_involves(&mut self, pane: PaneId) -> bool {
        let involves = self.pane() == Some(pane);
        if involves {
            self.state = DragOutState::Idle;
        }
        involves
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fyler_core::transfer::DropEffect;

    fn pane_state() -> TransferPaneState {
        TransferPaneState {
            dirty: false,
            idle: true,
            crashed: false,
            offline: false,
        }
    }

    #[test]
    fn start_rejection_blocks_on_busy_dirty_offline_crashed_and_saving() {
        assert_eq!(
            start_rejection(pane_state(), true),
            Some("Another save, transfer, or drag is in progress")
        );
        assert!(start_rejection(pane_state(), false).is_none());
        assert!(
            start_rejection(
                TransferPaneState {
                    offline: true,
                    ..pane_state()
                },
                false,
            )
            .is_some()
        );
        assert!(
            start_rejection(
                TransferPaneState {
                    crashed: true,
                    ..pane_state()
                },
                false,
            )
            .is_some()
        );
        assert!(
            start_rejection(
                TransferPaneState {
                    dirty: true,
                    ..pane_state()
                },
                false,
            )
            .is_some()
        );
        assert!(
            start_rejection(
                TransferPaneState {
                    idle: false,
                    ..pane_state()
                },
                false,
            )
            .is_some()
        );
    }

    #[test]
    fn cancelled_or_copy_outcome_finishes_without_cleanup() {
        let pane = PaneId::new(1);
        let mut controller = DragOutController::new();
        controller.begin(pane);
        assert!(matches!(
            controller.on_outcome(pane, DragOutcome::Cancelled, Vec::new()),
            DragOutFlowResult::Done
        ));
        assert!(!controller.is_busy());

        controller.begin(pane);
        let outcome = DragOutcome::Dropped {
            effect: DropEffect::Copy,
            move_reported: false,
        };
        assert!(matches!(
            controller.on_outcome(pane, outcome, Vec::new()),
            DragOutFlowResult::Done
        ));
        assert!(!controller.is_busy());
    }

    #[test]
    fn move_reported_with_vanished_source_finishes_without_cleanup() {
        let pane = PaneId::new(1);
        let mut controller = DragOutController::new();
        controller.begin(pane);
        let outcome = DragOutcome::Dropped {
            effect: DropEffect::Move,
            move_reported: true,
        };
        // optimized move: sourceは既にtargetが消している。
        assert!(matches!(
            controller.on_outcome(pane, outcome, Vec::new()),
            DragOutFlowResult::Done
        ));
        assert!(!controller.is_busy());
    }

    #[test]
    fn move_reported_with_remaining_source_requires_confirm_then_cleanup() {
        let pane = PaneId::new(1);
        let remaining = vec![PathBuf::from(r"C:\src\a.txt")];
        let mut controller = DragOutController::new();
        controller.begin(pane);
        let outcome = DragOutcome::Dropped {
            effect: DropEffect::Move,
            move_reported: true,
        };
        match controller.on_outcome(pane, outcome, remaining.clone()) {
            DragOutFlowResult::NeedsCleanupConfirm {
                pane_id,
                remaining: got,
            } => {
                assert_eq!(pane_id, pane);
                assert_eq!(got, remaining);
            }
            other => panic!("unexpected result: {other:?}"),
        }
        assert!(controller.is_busy());
        assert!(controller.is_confirming());

        match controller.on_choice(ConfirmChoice::Approve) {
            CleanupChoiceResult::Approved {
                pane_id,
                remaining: got,
            } => {
                assert_eq!(pane_id, pane);
                assert_eq!(got, remaining);
            }
            other => panic!("unexpected result: {other:?}"),
        }
        // 承認後もcleanup worker完了まではbusyのまま。
        assert!(controller.is_busy());
        assert!(!controller.is_confirming());

        controller.finish_cleanup(pane);
        assert!(!controller.is_busy());
    }

    #[test]
    fn cancelling_cleanup_confirm_leaves_source_untouched_and_clears_busy() {
        let pane = PaneId::new(1);
        let remaining = vec![PathBuf::from(r"C:\src\a.txt")];
        let mut controller = DragOutController::new();
        controller.begin(pane);
        let outcome = DragOutcome::Dropped {
            effect: DropEffect::Move,
            move_reported: true,
        };
        controller.on_outcome(pane, outcome, remaining);

        match controller.on_choice(ConfirmChoice::Cancel) {
            CleanupChoiceResult::Cancelled { pane_id } => assert_eq!(pane_id, pane),
            other => panic!("unexpected result: {other:?}"),
        }
        assert!(!controller.is_busy());
    }

    #[test]
    fn on_outcome_ignores_stale_event_from_a_different_pane() {
        let pane = PaneId::new(1);
        let other = PaneId::new(2);
        let mut controller = DragOutController::new();
        controller.begin(pane);
        assert!(matches!(
            controller.on_outcome(other, DragOutcome::Cancelled, Vec::new()),
            DragOutFlowResult::Ignored
        ));
        // 取り違えず、pane 1のdragはRunningのまま。
        assert!(controller.is_busy());
    }

    #[test]
    fn invalidate_if_involves_clears_pending_state_for_crashed_pane() {
        let pane = PaneId::new(1);
        let other = PaneId::new(2);
        let mut controller = DragOutController::new();
        controller.begin(pane);
        assert!(!controller.invalidate_if_involves(other));
        assert!(controller.is_busy());
        assert!(controller.invalidate_if_involves(pane));
        assert!(!controller.is_busy());
    }
}
