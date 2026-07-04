//! 保存フロー: parse → validate → diff → confirm → apply → reconcile。

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use fyler_core::editor::{EditorCommand, EditorEngine, EditorLine};
use fyler_core::grammar::PrefixParse;
use fyler_core::id::IdAllocator;
use fyler_core::path::TreePath;
use fyler_core::plan::OperationPlan;
use fyler_core::report::CommitReport;
use fyler_core::save::{self, SaveEffect, SaveEvent, SaveState};
use fyler_core::tree::{BaselineTree, EditContext, EntryKind};
use fyler_core::validate::ValidateError;
use fyler_fsops::scan::ScanOptions;
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

/// ディレクトリ折りたたみ操作の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToggleCollapseResult {
    /// 折りたたみ状態を切り替え、バッファへ設定すべき全行を返す。
    Toggled(Vec<EditorLine>),
    /// 対象行を現在のbaselineへ解決できない。
    NotFound,
    /// 対象行はディレクトリではない。
    NotADirectory,
    /// 保存状態機械が`Idle`ではないため、状態を変更しなかった。
    Busy,
}

pub struct SaveController {
    state: SaveState,
    root: PathBuf,
    ids: IdAllocator,
    baseline: BaselineTree,
    context: EditContext,
    scan_options: ScanOptions,
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
            scan_options: ScanOptions::default(),
            engine,
        }
    }

    /// 保存状態機械がルート差し替え可能な`Idle`状態かを返す。
    ///
    /// 確認ダイアログ表示中やapply/reconcile中のナビゲーションは、この判定で
    /// 副作用を起こす前に拒否すること。
    pub fn is_idle(&self) -> bool {
        matches!(self.state, SaveState::Idle)
    }

    /// バッファの`line`に埋め込まれたIDを現在のbaselineへ解決する。
    ///
    /// 戻り値は表示上の編集済みパスではなく、最後に実FSと同期したルート相対パスと
    /// エントリ種別である。行が範囲外、IDなし、壊れたID、またはbaselineに存在しない
    /// IDの場合は`None`を返す。
    pub fn resolve_line(&self, lines: &[EditorLine], line: usize) -> Option<(TreePath, EntryKind)> {
        let editor_line = lines.get(line)?;
        let PrefixParse::WithId { id, .. } =
            fyler_core::grammar::split_id_prefix(&editor_line.text)
        else {
            return None;
        };
        let entry = self.baseline.get(id)?;
        Some((entry.path.clone(), entry.kind))
    }

    /// 現在のbaselineと折りたたみ文脈から、バッファへ表示する全行を生成する。
    pub fn visible_lines(&self) -> Vec<EditorLine> {
        baseline_to_lines(&self.baseline, &self.context)
    }

    /// 現在のスキャンオプションを返す。
    ///
    /// 別ルートを先にスキャンしてから [`Self::change_root`] する配線では、この値を
    /// 引き継いで隠しファイル表示設定を維持すること。
    pub fn scan_options(&self) -> ScanOptions {
        self.scan_options
    }

    /// ルート直下の全ディレクトリを折りたたみ状態へ初期化する。
    ///
    /// baseline自体は全階層を保持し、表示行だけから各ディレクトリの子孫を除く。
    pub fn collapse_all_top_level(&mut self) {
        self.context.collapsed_dirs.extend(
            self.baseline
                .entries
                .iter()
                .filter(|entry| entry.kind == EntryKind::Dir && entry.path.depth() == 1)
                .map(|entry| entry.id),
        );
    }

    /// 指定行のディレクトリについて、折りたたみ状態を切り替える。
    ///
    /// 対象行は埋め込みIDを使って現在のbaselineへ解決する。展開時もbaselineから
    /// 全行を再生成し、別の折りたたみディレクトリの子孫は表示しない。dirty判定は
    /// 呼び出し元のapp層がこのAPIより先に行うこと。
    pub fn toggle_collapse(&mut self, lines: &[EditorLine], line: usize) -> ToggleCollapseResult {
        let Some((_, kind)) = self.resolve_line(lines, line) else {
            return ToggleCollapseResult::NotFound;
        };
        if kind != EntryKind::Dir {
            return ToggleCollapseResult::NotADirectory;
        }
        if !self.is_idle() {
            return ToggleCollapseResult::Busy;
        }

        let PrefixParse::WithId { id, .. } =
            fyler_core::grammar::split_id_prefix(&lines[line].text)
        else {
            return ToggleCollapseResult::NotFound;
        };
        if !self.context.collapsed_dirs.remove(&id) {
            self.context.collapsed_dirs.insert(id);
        }

        ToggleCollapseResult::Toggled(self.visible_lines())
    }

    /// 隠しファイル表示を切り替えて現在のルートを再スキャンする。
    ///
    /// 保存状態機械が`Idle`のときだけ実行し、同じパスのIDと新baselineにも実在する
    /// 折りたたみIDを維持する。戻り値はバッファへ設定すべき全行である。
    pub fn toggle_hidden(&mut self) -> anyhow::Result<Vec<EditorLine>> {
        if !self.is_idle() {
            anyhow::bail!("保存処理中は隠しファイル表示を変更できません");
        }

        let options = ScanOptions {
            show_hidden: !self.scan_options.show_hidden,
        };
        let baseline = fyler_fsops::scan::rescan_preserving_ids_with(
            &self.root,
            &mut self.ids,
            &self.baseline,
            &options,
        )
        .context("隠しファイル表示切り替え後の実FS再スキャンに失敗しました")?;
        let context = retain_existing_collapsed_dirs(&self.context, &baseline);
        let lines = baseline_to_lines(&baseline, &context);

        self.baseline = baseline;
        self.context = context;
        self.scan_options = options;
        Ok(lines)
    }

    /// 表示ルートとID採番器、baselineを新しいスキャン結果へ差し替える。
    ///
    /// 保存状態機械が`Idle`のときだけ成功する。成功時はルート固有の編集文脈も
    /// リセットする。`baseline.root`が`root`と一致しない入力は拒否し、既存状態を
    /// 変更しない。
    pub fn change_root(
        &mut self,
        root: PathBuf,
        ids: IdAllocator,
        baseline: BaselineTree,
    ) -> anyhow::Result<()> {
        if !self.is_idle() {
            anyhow::bail!("保存処理中は表示ルートを変更できません");
        }
        if baseline.root != root {
            anyhow::bail!(
                "表示ルートとbaselineのルートが一致しません: root={}, baseline={}",
                root.display(),
                baseline.root.display()
            );
        }

        self.root = root;
        self.ids = ids;
        self.baseline = baseline;
        self.context = EditContext::default();
        Ok(())
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
        let baseline = match fyler_fsops::scan::rescan_preserving_ids_with(
            &self.root,
            &mut self.ids,
            &self.baseline,
            &self.scan_options,
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

        let context = retain_existing_collapsed_dirs(&self.context, &baseline);
        let lines = baseline_to_lines(&baseline, &context);
        if let Err(error) = self
            .engine
            .send(EditorCommand::SetLines(lines))
            .context("外部変更後のバッファ行をエンジンへ送信できません")
        {
            return SaveFlowResult::ExternalChangeFailed(error.to_string());
        }

        self.baseline = baseline;
        self.context = context;
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
        let baseline = fyler_fsops::scan::rescan_preserving_ids_with(
            &self.root,
            &mut self.ids,
            &self.baseline,
            &self.scan_options,
        )
        .context("実FSの再スキャンに失敗しました")?;
        let context = retain_existing_collapsed_dirs(&self.context, &baseline);
        let lines = baseline_to_lines(&baseline, &context);
        self.engine
            .send(EditorCommand::SetLines(lines))
            .context("reconcile後のバッファ行をエンジンへ送信できません")?;

        self.baseline = baseline;
        self.context = context;
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

fn retain_existing_collapsed_dirs(context: &EditContext, baseline: &BaselineTree) -> EditContext {
    let mut context = context.clone();
    context.collapsed_dirs.retain(|id| {
        baseline
            .get(*id)
            .is_some_and(|entry| entry.kind == EntryKind::Dir)
    });
    context
}

pub(crate) fn baseline_to_lines(baseline: &BaselineTree, context: &EditContext) -> Vec<EditorLine> {
    let collapsed_paths = context
        .collapsed_dirs
        .iter()
        .filter_map(|id| baseline.get(*id))
        .filter(|entry| entry.kind == EntryKind::Dir)
        .map(|entry| &entry.path)
        .collect::<Vec<_>>();

    baseline
        .entries
        .iter()
        .filter(|entry| {
            !collapsed_paths
                .iter()
                .any(|path| path.is_strict_ancestor_of(&entry.path))
        })
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

    fn hierarchy_controller(root: impl Into<PathBuf>) -> (SaveController, Arc<RecordingEngine>) {
        let root = root.into();
        let mut ids = IdAllocator::new();
        let mut baseline = BaselineTree::new(&root);
        for (path, kind) in [
            ("a", EntryKind::Dir),
            ("a/nested", EntryKind::Dir),
            ("a/nested/leaf.txt", EntryKind::File),
            ("a/child.txt", EntryKind::File),
            ("top.txt", EntryKind::File),
        ] {
            baseline.insert(BaselineEntry {
                id: ids.allocate(),
                path: TreePath::parse(path),
                kind,
            });
        }
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

    #[test]
    fn resolve_line_uses_embedded_id_and_current_baseline() {
        let (controller, _) = controller("C:/test-root");
        let id = controller.baseline.entries[0].id;
        let buffer_lines = lines(&[
            "保存前の新規行.txt",
            &format!(
                "{}edited-name.txt",
                fyler_core::grammar::format_id_prefix(id)
            ),
            "/999 missing.txt",
        ]);

        assert_eq!(controller.resolve_line(&buffer_lines, 0), None);
        assert_eq!(
            controller.resolve_line(&buffer_lines, 1),
            Some((TreePath::parse("a.txt"), EntryKind::File))
        );
        assert_eq!(controller.resolve_line(&buffer_lines, 2), None);
        assert_eq!(controller.resolve_line(&buffer_lines, 3), None);
    }

    #[test]
    fn change_root_succeeds_only_while_idle() {
        let (mut controller, _) = controller("C:/old-root");
        let new_root = PathBuf::from("C:/new-root");
        let (new_baseline, new_ids) = baseline(&new_root);

        controller
            .change_root(new_root.clone(), new_ids, new_baseline)
            .unwrap();

        assert_eq!(controller.root, new_root);
        assert_eq!(controller.baseline.root, new_root);
        assert_eq!(
            controller.resolve_line(&lines(&["/001 a.txt"]), 0),
            Some((TreePath::parse("a.txt"), EntryKind::File))
        );

        assert!(matches!(
            controller.on_commit(7, &lines(&["/001 b.txt"])),
            SaveFlowResult::ShowPlan(_)
        ));
        let rejected_root = PathBuf::from("C:/rejected-root");
        let (rejected_baseline, rejected_ids) = baseline(&rejected_root);

        assert!(
            controller
                .change_root(rejected_root, rejected_ids, rejected_baseline)
                .is_err()
        );
        assert_eq!(controller.root, new_root);
        assert!(matches!(
            controller.state(),
            SaveState::AwaitingConfirmation { .. }
        ));
    }

    #[test]
    fn toggle_collapse_removes_descendants_and_expand_restores_them() {
        let (mut controller, _) = hierarchy_controller("C:/test-root");
        let expanded = controller.visible_lines();

        let collapsed = match controller.toggle_collapse(&expanded, 0) {
            ToggleCollapseResult::Toggled(lines) => lines,
            result => panic!("unexpected collapse result: {result:?}"),
        };
        assert_eq!(collapsed.len(), 2);
        assert!(collapsed[0].text.ends_with("a/"));
        assert!(collapsed[1].text.ends_with("top.txt"));

        let expanded_again = match controller.toggle_collapse(&collapsed, 0) {
            ToggleCollapseResult::Toggled(lines) => lines,
            result => panic!("unexpected expand result: {result:?}"),
        };
        assert_eq!(expanded_again, expanded);
    }

    #[test]
    fn collapse_all_top_level_hides_only_top_level_directory_descendants() {
        let (mut controller, _) = hierarchy_controller("C:/test-root");

        controller.collapse_all_top_level();

        let lines = controller.visible_lines();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].text.ends_with("a/"));
        assert!(lines[1].text.ends_with("top.txt"));
        assert_eq!(controller.context.collapsed_dirs, [EntryId(1)].into());
    }

    #[test]
    fn toggle_collapse_preserves_nested_collapsed_directory() {
        let (mut controller, _) = hierarchy_controller("C:/test-root");
        let expanded = controller.visible_lines();

        let nested_collapsed = match controller.toggle_collapse(&expanded, 1) {
            ToggleCollapseResult::Toggled(lines) => lines,
            result => panic!("unexpected nested collapse result: {result:?}"),
        };
        assert!(
            nested_collapsed
                .iter()
                .all(|line| !line.text.ends_with("leaf.txt"))
        );

        let parent_collapsed = match controller.toggle_collapse(&nested_collapsed, 0) {
            ToggleCollapseResult::Toggled(lines) => lines,
            result => panic!("unexpected parent collapse result: {result:?}"),
        };
        let parent_expanded = match controller.toggle_collapse(&parent_collapsed, 0) {
            ToggleCollapseResult::Toggled(lines) => lines,
            result => panic!("unexpected parent expand result: {result:?}"),
        };

        assert!(
            parent_expanded
                .iter()
                .any(|line| line.text.ends_with("nested/"))
        );
        assert!(
            parent_expanded
                .iter()
                .any(|line| line.text.ends_with("child.txt"))
        );
        assert!(
            parent_expanded
                .iter()
                .all(|line| !line.text.ends_with("leaf.txt"))
        );
    }

    #[test]
    fn toggle_collapse_rejects_non_directory() {
        let (mut controller, _) = hierarchy_controller("C:/test-root");
        let lines = controller.visible_lines();

        assert_eq!(
            controller.toggle_collapse(&lines, 2),
            ToggleCollapseResult::NotADirectory
        );
        assert_eq!(controller.visible_lines(), lines);
    }

    #[test]
    fn toggle_collapse_rejects_non_idle_save_state() {
        let (mut controller, _) = hierarchy_controller("C:/test-root");
        let mut lines = controller.visible_lines();
        lines[4].text = lines[4].text.replace("top.txt", "renamed.txt");
        assert!(matches!(
            controller.on_commit(7, &lines),
            SaveFlowResult::ShowPlan(_)
        ));

        assert_eq!(
            controller.toggle_collapse(&lines, 0),
            ToggleCollapseResult::Busy
        );
    }

    #[test]
    fn collapsed_directory_rename_builds_only_parent_move() {
        let (mut controller, _) = hierarchy_controller("C:/test-root");
        let lines = controller.visible_lines();
        let mut collapsed = match controller.toggle_collapse(&lines, 0) {
            ToggleCollapseResult::Toggled(lines) => lines,
            result => panic!("unexpected collapse result: {result:?}"),
        };
        collapsed[0].text = collapsed[0].text.replace("a/", "renamed/");

        let result = controller.on_commit(7, &collapsed);

        assert_eq!(
            result,
            SaveFlowResult::ShowPlan(OperationPlan {
                ops: vec![FsOperation::Move {
                    id: EntryId(1),
                    from: TreePath::parse("a"),
                    to: TreePath::parse("renamed"),
                }],
            })
        );
    }

    #[test]
    fn external_change_keeps_existing_collapsed_state() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("a")).unwrap();
        fs::write(root.path().join("a").join("child.txt"), b"child").unwrap();
        fs::write(root.path().join("top.txt"), b"top").unwrap();
        let mut ids = IdAllocator::new();
        let baseline = fyler_fsops::scan::scan_baseline(root.path(), &mut ids).unwrap();
        let engine = Arc::new(RecordingEngine::default());
        let mut controller = SaveController::new(
            root.path().to_path_buf(),
            ids,
            baseline,
            Arc::<RecordingEngine>::clone(&engine),
        );
        let expanded = controller.visible_lines();
        let collapsed = match controller.toggle_collapse(&expanded, 0) {
            ToggleCollapseResult::Toggled(lines) => lines,
            result => panic!("unexpected collapse result: {result:?}"),
        };
        assert!(
            collapsed
                .iter()
                .all(|line| !line.text.ends_with("child.txt"))
        );
        fs::write(root.path().join("new.txt"), b"new").unwrap();

        assert_eq!(
            controller.on_external_change(),
            SaveFlowResult::ExternalChanged
        );

        let commands = engine.commands.lock().unwrap();
        assert!(matches!(
            commands.last(),
            Some(EditorCommand::SetLines(lines))
                if lines.iter().all(|line| !line.text.ends_with("child.txt"))
                    && lines.iter().any(|line| line.text.ends_with("new.txt"))
        ));
    }

    #[test]
    fn toggle_hidden_rescans_and_preserves_the_option() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("visible.txt"), b"visible").unwrap();
        fs::write(root.path().join(".hidden.txt"), b"hidden").unwrap();
        let mut ids = IdAllocator::new();
        let baseline = fyler_fsops::scan::scan_baseline(root.path(), &mut ids).unwrap();
        let engine = Arc::new(RecordingEngine::default());
        let mut controller = SaveController::new(
            root.path().to_path_buf(),
            ids,
            baseline,
            Arc::<RecordingEngine>::clone(&engine),
        );

        let shown = controller.toggle_hidden().unwrap();
        assert!(controller.scan_options().show_hidden);
        assert!(shown.iter().any(|line| line.text.ends_with(".hidden.txt")));

        let hidden = controller.toggle_hidden().unwrap();
        assert!(!controller.scan_options().show_hidden);
        assert!(
            hidden
                .iter()
                .all(|line| !line.text.ends_with(".hidden.txt"))
        );
    }
}
