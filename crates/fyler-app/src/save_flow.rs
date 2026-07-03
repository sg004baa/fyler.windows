//! M2の保存フロー: parse → validate → diff → dry-run確認。
//!
//! 実ファイル操作はM3でのみ追加する。このモジュールには実行APIへの経路を持たせず、
//! 承認時にも `Applying` へ遷移しない。

use fyler_core::editor::EditorLine;
use fyler_core::plan::OperationPlan;
use fyler_core::save::{self, SaveEffect, SaveEvent, SaveState};
use fyler_core::tree::{BaselineTree, EditContext};
use fyler_core::validate::ValidateError;
use fyler_gui::confirm::ConfirmChoice;

/// 保存フローから配線層へ返す結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveFlowResult {
    ShowPlan(OperationPlan),
    ShowValidationErrors(Vec<ValidateError>),
    NoChanges,
    Cancelled,
    ApprovedDryRun,
    Ignored,
}

pub struct SaveController {
    state: SaveState,
    baseline: BaselineTree,
    context: EditContext,
}

impl SaveController {
    pub fn new(baseline: BaselineTree) -> Self {
        Self {
            state: SaveState::Idle,
            baseline,
            context: EditContext::default(),
        }
    }

    pub fn on_commit(&mut self, changedtick: u64, lines: &[EditorLine]) -> SaveFlowResult {
        let effects = self.apply_event(SaveEvent::CommitRequested { changedtick });
        if !effects
            .iter()
            .any(|effect| matches!(effect, SaveEffect::RunPipeline))
        {
            return SaveFlowResult::Ignored;
        }

        // SetModifiable(false) はM2時点ではエンジンAPIがないためno-op。
        // 確認/エラーダイアログ表示中のGUI入力ゲートで編集を止める。
        let parsed = fyler_pipeline::parse::parse(lines);
        let desired = match fyler_pipeline::parse::to_desired_tree(&parsed) {
            Ok(desired) => desired,
            Err(errors) => return self.validation_failed(errors),
        };

        let errors = fyler_pipeline::validate::validate(&self.baseline, &desired, &self.context);
        if !errors.is_empty() {
            return self.validation_failed(errors);
        }

        let plan = fyler_pipeline::diff::build_plan(&self.baseline, &desired, &self.context);
        let display_plan = plan.clone();
        let effects = self.apply_event(SaveEvent::PlanReady { plan });
        if effects
            .iter()
            .any(|effect| matches!(effect, SaveEffect::ShowConfirmDialog))
        {
            SaveFlowResult::ShowPlan(display_plan)
        } else {
            SaveFlowResult::NoChanges
        }
    }

    pub fn on_choice(&mut self, choice: ConfirmChoice) -> SaveFlowResult {
        if !matches!(self.state, SaveState::AwaitingConfirmation { .. }) {
            return SaveFlowResult::Ignored;
        }

        // M2のApproveはdry-run完了を意味する。SaveEvent::ApprovedはApplyingへ進み
        // ExecutePlanを返すため、M2では発火させない。Cancelと同じ安全な終了遷移で
        // Idleへ戻し、baselineを更新せずbufferをdirtyのまま保つ。
        let effects = self.apply_event(SaveEvent::Cancelled);
        debug_assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect, SaveEffect::ExecutePlan))
        );

        match choice {
            ConfirmChoice::Approve => SaveFlowResult::ApprovedDryRun,
            ConfirmChoice::Cancel => SaveFlowResult::Cancelled,
        }
    }

    #[cfg(test)]
    fn state(&self) -> &SaveState {
        &self.state
    }

    fn validation_failed(&mut self, errors: Vec<ValidateError>) -> SaveFlowResult {
        let effects = self.apply_event(SaveEvent::ValidationFailed { errors });
        effects
            .into_iter()
            .find_map(|effect| match effect {
                SaveEffect::ShowValidationErrors(errors) => {
                    Some(SaveFlowResult::ShowValidationErrors(errors))
                }
                _ => None,
            })
            .unwrap_or(SaveFlowResult::Ignored)
    }

    fn apply_event(&mut self, event: SaveEvent) -> Vec<SaveEffect> {
        let state = std::mem::replace(&mut self.state, SaveState::Idle);
        let (state, effects) = save::transition(state, event);
        self.state = state;
        effects
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use fyler_core::id::EntryId;
    use fyler_core::path::TreePath;
    use fyler_core::plan::FsOperation;
    use fyler_core::tree::{BaselineEntry, EntryKind};

    use super::*;

    fn baseline(root: impl Into<std::path::PathBuf>) -> BaselineTree {
        let mut baseline = BaselineTree::new(root);
        baseline.insert(BaselineEntry {
            id: EntryId(1),
            path: TreePath::parse("a.txt"),
            kind: EntryKind::File,
        });
        baseline
    }

    fn lines(lines: &[&str]) -> Vec<EditorLine> {
        lines.iter().map(|line| EditorLine::new(*line)).collect()
    }

    #[test]
    fn rename_returns_confirmation_plan() {
        let mut controller = SaveController::new(baseline("C:/test-root"));

        let result = controller.on_commit(7, &lines(&["/001 b.txt"]));

        assert_eq!(
            result,
            SaveFlowResult::ShowPlan(OperationPlan {
                ops: vec![FsOperation::Move {
                    id: EntryId(1),
                    from: TreePath::parse("a.txt"),
                    to: TreePath::parse("b.txt"),
                }],
            })
        );
        assert!(matches!(
            controller.state(),
            SaveState::AwaitingConfirmation { changedtick: 7, .. }
        ));
    }

    #[test]
    fn reserved_character_returns_validation_errors() {
        let mut controller = SaveController::new(baseline("C:/test-root"));

        let result = controller.on_commit(1, &lines(&["/001 bad<name.txt"]));

        assert!(matches!(
            result,
            SaveFlowResult::ShowValidationErrors(ref errors)
                if errors.iter().any(|error| matches!(
                    error,
                    ValidateError::ReservedChar { line: 0, ch: '<', .. }
                ))
        ));
        assert!(matches!(controller.state(), SaveState::Idle));
    }

    #[test]
    fn broken_prefix_returns_validation_errors() {
        let mut controller = SaveController::new(baseline("C:/test-root"));

        let result = controller.on_commit(1, &lines(&["/0"]));

        assert!(matches!(
            result,
            SaveFlowResult::ShowValidationErrors(ref errors)
                if errors == &[ValidateError::BrokenIdPrefix { line: 0 }]
        ));
        assert!(matches!(controller.state(), SaveState::Idle));
    }

    #[test]
    fn unchanged_buffer_skips_confirmation_and_returns_to_idle() {
        let mut controller = SaveController::new(baseline("C:/test-root"));

        let result = controller.on_commit(1, &lines(&["/001 a.txt"]));

        assert_eq!(result, SaveFlowResult::NoChanges);
        assert!(matches!(controller.state(), SaveState::Idle));
    }

    #[test]
    fn approve_is_dry_run_and_does_not_touch_filesystem() {
        static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

        let temp_root = std::env::temp_dir().join(format!(
            "fyler-save-flow-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&temp_root).unwrap();
        fs::write(temp_root.join("a.txt"), b"unchanged").unwrap();

        let mut controller = SaveController::new(baseline(&temp_root));
        assert!(matches!(
            controller.on_commit(1, &lines(&["/001 b.txt"])),
            SaveFlowResult::ShowPlan(_)
        ));

        let result = controller.on_choice(ConfirmChoice::Approve);

        assert_eq!(result, SaveFlowResult::ApprovedDryRun);
        assert!(matches!(controller.state(), SaveState::Idle));
        assert_eq!(fs::read(temp_root.join("a.txt")).unwrap(), b"unchanged");
        assert!(!temp_root.join("b.txt").exists());

        fs::remove_file(temp_root.join("a.txt")).unwrap();
        fs::remove_dir(temp_root).unwrap();
    }
}
