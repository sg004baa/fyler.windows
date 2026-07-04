//! 保存フロー: parse → validate → diff → confirm → apply → reconcile。

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use fyler_core::editor::{EditorCommand, EditorEngine, EditorLine};
use fyler_core::id::IdAllocator;
use fyler_core::plan::OperationPlan;
use fyler_core::report::CommitReport;
use fyler_core::save::{self, SaveEffect, SaveEvent, SaveState};
use fyler_core::tree::{BaselineTree, EditContext, EntryKind};
use fyler_core::validate::ValidateError;
use fyler_gui::confirm::ConfirmChoice;

/// 保存フローから配線層へ返す結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveFlowResult {
    ShowPlan(OperationPlan),
    ShowValidationErrors(Vec<ValidateError>),
    ShowReport(CommitReport),
    ReconcileFailed {
        report: CommitReport,
        error: String,
    },
    ExternalChanged,
    ExternalChangeNotified(String),
    ExternalChangeFailed(String),
    /// 確認ダイアログ表示中に外部変更を検知し、表示中のplanを破棄した。
    /// 配線層はダイアログを閉じ、メッセージを表示すること。
    PlanInvalidated(String),
    NoChanges,
    Cancelled,
    Ignored,
}

pub struct SaveController {
    state: SaveState,
    root: PathBuf,
    ids: IdAllocator,
    baseline: BaselineTree,
    context: EditContext,
    engine: Arc<dyn EditorEngine>,
}

impl SaveController {
    pub fn new(
        root: PathBuf,
        ids: IdAllocator,
        baseline: BaselineTree,
        engine: Arc<dyn EditorEngine>,
    ) -> Self {
        Self {
            state: SaveState::Idle,
            root,
            ids,
            baseline,
            context: EditContext::default(),
            engine,
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

        let plan = match fyler_pipeline::diff::build_plan(&self.baseline, &desired, &self.context) {
            Ok(plan) => plan,
            Err(errors) => return self.validation_failed(errors),
        };
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

        match choice {
            ConfirmChoice::Cancel => {
                self.apply_event(SaveEvent::Cancelled);
                SaveFlowResult::Cancelled
            }
            ConfirmChoice::Approve => self.approve_and_apply(),
        }
    }

    pub fn on_external_change(&mut self) -> SaveFlowResult {
        let baseline = match fyler_fsops::scan::rescan_preserving_ids(
            &self.root,
            &mut self.ids,
            &self.baseline,
        )
        .context("外部変更後の実FS再スキャンに失敗しました")
        {
            Ok(baseline) => baseline,
            Err(error) => return SaveFlowResult::ExternalChangeFailed(error.to_string()),
        };

        if baseline == self.baseline {
            return SaveFlowResult::NoChanges;
        }

        // 確認ダイアログ表示中の外部変更は、表示中のplanを陳腐化させる。
        // 承認済みとして実行すると古いbaseline前提の操作が実FSへ流れるため、
        // ここでキャンセル扱いにしてダイアログを閉じ、ユーザーへ通知する。
        if matches!(self.state, SaveState::AwaitingConfirmation { .. }) {
            self.apply_event(SaveEvent::Cancelled);
            return SaveFlowResult::PlanInvalidated(
                "外部でファイルが変更されたため、保存を中断しました。内容を確認して再度 :w してください"
                    .to_owned(),
            );
        }

        if self.engine.snapshot().dirty {
            return SaveFlowResult::ExternalChangeNotified(
                "外部でファイルが変更されました。編集中のため表示は更新していません".to_owned(),
            );
        }

        let lines = baseline_to_lines(&baseline);
        if let Err(error) = self
            .engine
            .send(EditorCommand::SetLines(lines))
            .context("外部変更後のバッファ行をエンジンへ送信できません")
        {
            return SaveFlowResult::ExternalChangeFailed(error.to_string());
        }

        self.baseline = baseline;
        self.context = EditContext::default();
        SaveFlowResult::ExternalChanged
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

    fn approve_and_apply(&mut self) -> SaveFlowResult {
        let effects = self.apply_event(SaveEvent::Approved);
        if !effects
            .iter()
            .any(|effect| matches!(effect, SaveEffect::ExecutePlan))
        {
            return SaveFlowResult::Ignored;
        }

        let plan = match &self.state {
            SaveState::Applying { plan, .. } => plan.clone(),
            _ => return SaveFlowResult::Ignored,
        };

        // 絶対ルール1: apply_planは、ApprovedでApplyingへ遷移した上の経路からだけ呼ぶ。
        let report = fyler_fsops::apply::apply_plan(&self.root, &plan);
        let effects = self.apply_event(SaveEvent::ApplyFinished {
            report: report.clone(),
        });
        debug_assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, SaveEffect::ShowCommitReport(_)))
        );

        if effects
            .iter()
            .any(|effect| matches!(effect, SaveEffect::ReconcileFromFs))
        {
            if let Err(error) = self.reconcile_from_fs() {
                return SaveFlowResult::ReconcileFailed {
                    report,
                    error: error.to_string(),
                };
            }
        }

        SaveFlowResult::ShowReport(report)
    }

    fn reconcile_from_fs(&mut self) -> anyhow::Result<()> {
        let baseline =
            fyler_fsops::scan::rescan_preserving_ids(&self.root, &mut self.ids, &self.baseline)
                .context("実FSの再スキャンに失敗しました")?;
        let lines = baseline_to_lines(&baseline);
        self.engine
            .send(EditorCommand::SetLines(lines))
            .context("reconcile後のバッファ行をエンジンへ送信できません")?;

        self.baseline = baseline;
        self.context = EditContext::default();
        let effects = self.apply_event(SaveEvent::ReconcileFinished);
        debug_assert!(matches!(self.state, SaveState::Idle));
        debug_assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, SaveEffect::SetModifiable(true)))
        );
        Ok(())
    }

    fn apply_event(&mut self, event: SaveEvent) -> Vec<SaveEffect> {
        let state = std::mem::replace(&mut self.state, SaveState::Idle);
        let (state, effects) = save::transition(state, event);
        self.state = state;
        effects
    }
}

pub(crate) fn baseline_to_lines(baseline: &BaselineTree) -> Vec<EditorLine> {
    baseline
        .entries
        .iter()
        .map(|entry| {
            let indent = " "
                .repeat(entry.path.depth().saturating_sub(1) * fyler_core::grammar::INDENT_WIDTH);
            let directory_suffix = if entry.kind == EntryKind::Dir {
                fyler_core::grammar::DIR_SUFFIX.to_string()
            } else {
                String::new()
            };
            EditorLine::new(format!(
                "{}{}{}{}",
                fyler_core::grammar::format_id_prefix(entry.id),
                indent,
                entry.path.name().unwrap_or_default(),
                directory_suffix,
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use fyler_core::editor::EditorSnapshot;
    use fyler_core::id::EntryId;
    use fyler_core::path::TreePath;
    use fyler_core::plan::FsOperation;
    use fyler_core::tree::{BaselineEntry, EntryKind};
    use tempfile::tempdir;

    use super::*;

    #[derive(Default)]
    struct RecordingEngine {
        commands: Mutex<Vec<EditorCommand>>,
        dirty: AtomicBool,
    }

    impl RecordingEngine {
        fn set_dirty(&self, dirty: bool) {
            self.dirty.store(dirty, Ordering::Relaxed);
        }
    }

    impl EditorEngine for RecordingEngine {
        fn send(&self, command: EditorCommand) -> anyhow::Result<()> {
            self.commands.lock().unwrap().push(command);
            Ok(())
        }

        fn snapshot(&self) -> Arc<EditorSnapshot> {
            let mut snapshot = EditorSnapshot::empty();
            snapshot.dirty = self.dirty.load(Ordering::Relaxed);
            Arc::new(snapshot)
        }
    }

    fn baseline(root: impl Into<PathBuf>) -> (BaselineTree, IdAllocator) {
        let mut ids = IdAllocator::new();
        let id = ids.allocate();
        let mut baseline = BaselineTree::new(root);
        baseline.insert(BaselineEntry {
            id,
            path: TreePath::parse("a.txt"),
            kind: EntryKind::File,
        });
        (baseline, ids)
    }

    fn controller(root: impl Into<PathBuf>) -> (SaveController, Arc<RecordingEngine>) {
        let root = root.into();
        let (baseline, ids) = baseline(&root);
        let engine = Arc::new(RecordingEngine::default());
        let controller =
            SaveController::new(root, ids, baseline, Arc::<RecordingEngine>::clone(&engine));
        (controller, engine)
    }

    fn lines(lines: &[&str]) -> Vec<EditorLine> {
        lines.iter().map(|line| EditorLine::new(*line)).collect()
    }

    #[test]
    fn rename_returns_confirmation_plan() {
        let (mut controller, _) = controller("C:/test-root");

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
        let (mut controller, _) = controller("C:/test-root");

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
        let (mut controller, _) = controller("C:/test-root");

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
        let (mut controller, _) = controller("C:/test-root");

        let result = controller.on_commit(1, &lines(&["/001 a.txt"]));

        assert_eq!(result, SaveFlowResult::NoChanges);
        assert!(matches!(controller.state(), SaveState::Idle));
    }

    #[test]
    fn approve_applies_rename_and_reconciles_buffer_from_filesystem() {
        let temp_root = tempdir().unwrap();
        fs::write(temp_root.path().join("a.txt"), b"content").unwrap();
        let mut ids = IdAllocator::new();
        let baseline = fyler_fsops::scan::scan_baseline(temp_root.path(), &mut ids).unwrap();
        let original_id = baseline.entries[0].id;
        let engine = Arc::new(RecordingEngine::default());
        let mut controller = SaveController::new(
            temp_root.path().to_path_buf(),
            ids,
            baseline,
            Arc::<RecordingEngine>::clone(&engine),
        );
        let renamed_line = EditorLine::new(format!(
            "{}b.txt",
            fyler_core::grammar::format_id_prefix(original_id)
        ));
        assert!(matches!(
            controller.on_commit(1, &[renamed_line]),
            SaveFlowResult::ShowPlan(_)
        ));

        let result = controller.on_choice(ConfirmChoice::Approve);

        assert!(matches!(
            result,
            SaveFlowResult::ShowReport(ref report) if report.all_succeeded()
        ));
        assert!(matches!(controller.state(), SaveState::Idle));
        assert!(!temp_root.path().join("a.txt").exists());
        assert_eq!(
            fs::read(temp_root.path().join("b.txt")).unwrap(),
            b"content"
        );
        assert!(
            controller
                .baseline
                .entries
                .iter()
                .any(|entry| entry.path == TreePath::parse("b.txt"))
        );
        let commands = engine.commands.lock().unwrap();
        assert!(commands.iter().any(|command| matches!(
            command,
            EditorCommand::SetLines(lines)
                if lines.iter().any(|line| line.text.ends_with("b.txt"))
                    && lines.iter().all(|line| !line.text.ends_with("a.txt"))
        )));
    }

    #[test]
    fn cancel_leaves_filesystem_and_baseline_unchanged() {
        let temp_root = tempdir().unwrap();
        fs::write(temp_root.path().join("a.txt"), b"content").unwrap();
        let (mut controller, engine) = controller(temp_root.path());
        assert!(matches!(
            controller.on_commit(1, &lines(&["/001 b.txt"])),
            SaveFlowResult::ShowPlan(_)
        ));

        assert_eq!(
            controller.on_choice(ConfirmChoice::Cancel),
            SaveFlowResult::Cancelled
        );

        assert!(matches!(controller.state(), SaveState::Idle));
        assert!(temp_root.path().join("a.txt").exists());
        assert!(!temp_root.path().join("b.txt").exists());
        assert_eq!(
            controller.baseline.entries[0].path,
            TreePath::parse("a.txt")
        );
        assert!(engine.commands.lock().unwrap().is_empty());
    }

    #[test]
    fn all_failed_returns_report_without_reconciling() {
        let temp_root = tempdir().unwrap();
        let (mut controller, engine) = controller(temp_root.path());
        assert!(matches!(
            controller.on_commit(1, &lines(&["/001 b.txt"])),
            SaveFlowResult::ShowPlan(_)
        ));

        let result = controller.on_choice(ConfirmChoice::Approve);

        assert!(matches!(
            result,
            SaveFlowResult::ShowReport(ref report) if report.all_failed()
        ));
        assert!(matches!(controller.state(), SaveState::Idle));
        assert_eq!(
            controller.baseline.entries[0].path,
            TreePath::parse("a.txt")
        );
        assert!(engine.commands.lock().unwrap().is_empty());
    }

    #[test]
    fn external_change_replaces_clean_buffer_and_updates_baseline() {
        let temp_root = tempdir().unwrap();
        fs::write(temp_root.path().join("a.txt"), b"a").unwrap();
        let (mut controller, engine) = controller(temp_root.path());
        fs::write(temp_root.path().join("b.txt"), b"b").unwrap();

        let result = controller.on_external_change();

        assert_eq!(result, SaveFlowResult::ExternalChanged);
        assert!(
            controller
                .baseline
                .entries
                .iter()
                .any(|entry| entry.path == TreePath::parse("b.txt"))
        );
        let commands = engine.commands.lock().unwrap();
        assert!(matches!(
            commands.as_slice(),
            [EditorCommand::SetLines(lines)]
                if lines.iter().any(|line| line.text.ends_with("b.txt"))
        ));
    }

    #[test]
    fn external_change_does_not_replace_dirty_buffer_or_baseline() {
        let temp_root = tempdir().unwrap();
        fs::write(temp_root.path().join("a.txt"), b"a").unwrap();
        let (mut controller, engine) = controller(temp_root.path());
        engine.set_dirty(true);
        fs::write(temp_root.path().join("b.txt"), b"b").unwrap();

        let result = controller.on_external_change();

        assert!(matches!(
            result,
            SaveFlowResult::ExternalChangeNotified(ref message)
                if message.contains("外部でファイルが変更されました")
        ));
        assert!(
            controller
                .baseline
                .entries
                .iter()
                .all(|entry| entry.path != TreePath::parse("b.txt"))
        );
        assert!(engine.commands.lock().unwrap().is_empty());
    }

    #[test]
    fn external_change_matching_baseline_is_ignored() {
        let temp_root = tempdir().unwrap();
        fs::write(temp_root.path().join("a.txt"), b"a").unwrap();
        let (mut controller, engine) = controller(temp_root.path());

        let result = controller.on_external_change();

        assert_eq!(result, SaveFlowResult::NoChanges);
        assert!(engine.commands.lock().unwrap().is_empty());
    }

    #[test]
    fn external_change_during_confirmation_invalidates_plan_and_blocks_approve() {
        // 確認ダイアログ表示中に実FSが変わると、表示中のplanは古いbaseline前提。
        // 承認済みとして実行せず破棄し、Idleへ戻す(その後のApproveは無効)。
        let temp_root = tempdir().unwrap();
        fs::write(temp_root.path().join("a.txt"), b"a").unwrap();
        let (mut controller, _) = controller(temp_root.path());
        assert!(matches!(
            controller.on_commit(1, &lines(&["/001 b.txt"])),
            SaveFlowResult::ShowPlan(_)
        ));
        fs::write(temp_root.path().join("c.txt"), b"c").unwrap();

        let result = controller.on_external_change();

        assert!(matches!(
            &result,
            SaveFlowResult::PlanInvalidated(message)
                if message.contains("外部でファイルが変更されたため、保存を中断しました")
        ));
        assert!(matches!(controller.state(), SaveState::Idle));
        assert_eq!(
            controller.on_choice(ConfirmChoice::Approve),
            SaveFlowResult::Ignored
        );
        assert!(temp_root.path().join("a.txt").exists());
        assert!(!temp_root.path().join("b.txt").exists());
    }

    #[test]
    fn external_change_event_matching_baseline_keeps_confirmation_approvable() {
        // 実FSがbaselineと一致するままの外部変更イベント(誤検知)では
        // planを破棄せず、ダイアログはそのまま承認可能。
        let temp_root = tempdir().unwrap();
        fs::write(temp_root.path().join("a.txt"), b"a").unwrap();
        let (mut controller, _) = controller(temp_root.path());
        assert!(matches!(
            controller.on_commit(1, &lines(&["/001 b.txt"])),
            SaveFlowResult::ShowPlan(_)
        ));

        let result = controller.on_external_change();

        assert_eq!(result, SaveFlowResult::NoChanges);
        assert!(matches!(
            controller.state(),
            SaveState::AwaitingConfirmation { .. }
        ));
        assert!(matches!(
            controller.on_choice(ConfirmChoice::Approve),
            SaveFlowResult::ShowReport(report) if report.all_succeeded()
        ));
        assert!(temp_root.path().join("b.txt").exists());
    }
}
