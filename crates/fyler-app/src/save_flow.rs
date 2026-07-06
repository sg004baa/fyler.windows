//! 保存フロー: parse → validate → diff → confirm → apply → reconcile。

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use fyler_core::editor::{EditorCommand, EditorEngine, EditorLine};
use fyler_core::fileinfo::{FileInfo, human_readable_size};
use fyler_core::gitstatus::GitBadge;
use fyler_core::grammar::PrefixParse;
use fyler_core::id::{EntryId, IdAllocator};
use fyler_core::path::TreePath;
use fyler_core::plan::OperationPlan;
use fyler_core::report::CommitReport;
use fyler_core::save::{self, SaveEffect, SaveEvent, SaveState};
use fyler_core::tree::{BaselineEntry, BaselineTree, EditContext, EntryKind};
use fyler_core::validate::ValidateError;
use fyler_fsops::scan::ScanOptions;
use fyler_gui::confirm::ConfirmChoice;

/// 保存フローから配線層へ返す結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveFlowResult {
    /// 確認対象のplanと、実行時に発生し得るクラウド取得等の警告を表示する。
    ShowPlan {
        plan: OperationPlan,
        warnings: Vec<String>,
        /// 承認時に既存実体をごみ箱へ退避してから実行する移動先。plan順。
        overwrites: Vec<TreePath>,
    },
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
    pending_overwrites: HashSet<TreePath>,
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
            pending_overwrites: HashSet::new(),
            engine,
        }
    }

    /// 初回スキャンに使った表示設定を保持して保存フローを作成する。
    ///
    /// 設定ファイル由来の隠しファイル表示とソート順を、再スキャン・ルート移動でも
    /// 維持する必要がある場合に使う。既定設定では [`Self::new`] と同じである。
    pub fn new_with_scan_options(
        root: PathBuf,
        ids: IdAllocator,
        baseline: BaselineTree,
        engine: Arc<dyn EditorEngine>,
        scan_options: ScanOptions,
    ) -> Self {
        let mut controller = Self::new(root, ids, baseline, engine);
        controller.scan_options = scan_options;
        controller
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

    /// 現在の表示行から、指定した名前のトップレベルエントリの行indexを探す。
    ///
    /// 親ディレクトリへ移動した直後に、元いた子ディレクトリへカーソルを合わせる用途。
    /// 見つからない場合(隠しファイル設定で非表示等)は`None`。
    pub fn find_top_level_line(&self, name: &std::ffi::OsStr) -> Option<usize> {
        self.visible_lines().iter().position(|line| {
            let PrefixParse::WithId { rest, .. } = fyler_core::grammar::split_id_prefix(&line.text)
            else {
                return false;
            };
            let (indent, name_with_suffix) = fyler_core::grammar::split_indent(rest);
            if indent != 0 {
                return false;
            }
            let (entry_name, _) = fyler_core::grammar::split_dir_suffix(name_with_suffix);
            std::ffi::OsStr::new(entry_name) == name
        })
    }

    /// 現在のスキャンオプションを返す。
    ///
    /// 別ルートを先にスキャンしてから [`Self::change_root`] する配線では、この値を
    /// 引き継いで隠しファイル表示設定を維持すること。
    pub fn scan_options(&self) -> ScanOptions {
        self.scan_options
    }

    /// 表示ルート相対のGit状態を、現在のbaselineのエントリIDへ対応付けて返す。
    ///
    /// Gitがディレクトリ単位で報告した状態は同じパスのエントリだけへ付け、子孫や
    /// 親ディレクトリへ伝播しない。対応するエントリがない状態は無視する。
    pub fn map_git_badges(
        &self,
        statuses: &HashMap<PathBuf, GitBadge>,
    ) -> HashMap<EntryId, GitBadge> {
        if statuses.is_empty() {
            return HashMap::new();
        }
        self.baseline
            .entries()
            .iter()
            .filter_map(|entry| {
                let relative = entry.path.to_fs_path(Path::new(""));
                statuses.get(&relative).map(|badge| (entry.id, *badge))
            })
            .collect()
    }

    /// 現在表示中の行に対応するエントリの表示用メタデータをIDへ対応付けて返す。
    ///
    /// スキャン由来のメタデータを持たないエントリは含めず、モードラインでは
    /// 情報を表示しない。ここでは実FSへ問い合わせない。
    pub fn visible_file_infos(&self) -> HashMap<EntryId, FileInfo> {
        visible_entries(&self.baseline, &self.context)
            .into_iter()
            .filter_map(|entry| {
                self.baseline.meta(entry.id).map(|meta| {
                    (
                        entry.id,
                        FileInfo {
                            size: meta.size,
                            modified: meta
                                .modified
                                .and_then(fyler_fsops::info::format_modified_time),
                            is_placeholder: meta.is_placeholder,
                        },
                    )
                })
            })
            .collect()
    }

    /// すべてのディレクトリを折りたたみ状態へ初期化する。
    ///
    /// 展開は [`Self::toggle_collapse`] で1階層ずつ行う。baseline自体は全階層を
    /// 保持し、表示行だけから各ディレクトリの子孫を除く。
    pub fn collapse_all_dirs(&mut self) {
        self.context.collapsed_dirs.extend(
            self.baseline
                .entries()
                .iter()
                .filter(|entry| entry.kind == EntryKind::Dir)
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
            ..self.scan_options
        };
        let baseline = fyler_fsops::scan::rescan_preserving_ids_with(
            &self.root,
            &mut self.ids,
            &self.baseline,
            &options,
        )
        .context("隠しファイル表示切り替え後の実FS再スキャンに失敗しました")?;
        let context = carry_collapsed_dirs(&self.context, &self.baseline, &baseline);
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
        let conflicts = fyler_fsops::preflight::scan_plan_conflicts(&self.root, &plan);
        if !conflicts.blocked.is_empty() {
            return self.validation_failed(
                conflicts
                    .blocked
                    .into_iter()
                    .map(|path| ValidateError::TargetOccupiedByDirectory { path })
                    .collect(),
            );
        }
        let overwrites = conflicts.overwritable;
        self.pending_overwrites = overwrites.iter().cloned().collect();
        let display_plan = plan.clone();
        let warnings = plan_warnings(
            &self.root,
            &display_plan,
            fyler_fsops::onedrive::is_cloud_placeholder,
        );
        let effects = self.apply_event(SaveEvent::PlanReady { plan });
        if effects
            .iter()
            .any(|effect| matches!(effect, SaveEffect::ShowConfirmDialog))
        {
            SaveFlowResult::ShowPlan {
                plan: display_plan,
                warnings,
                overwrites,
            }
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
                self.pending_overwrites.clear();
                SaveFlowResult::Cancelled
            }
            ConfirmChoice::Approve => self.approve_and_apply(),
        }
    }

    pub fn on_external_change(&mut self, changed_paths: &BTreeSet<PathBuf>) -> SaveFlowResult {
        let baseline = match fyler_fsops::scan::rescan_changed_preserving_ids_with(
            &self.root,
            &mut self.ids,
            &self.baseline,
            changed_paths,
            &self.scan_options,
        )
        .context("外部変更後の実FS再スキャンに失敗しました")
        {
            Ok(baseline) => baseline,
            Err(error) => return SaveFlowResult::ExternalChangeFailed(error.to_string()),
        };

        if baseline == self.baseline {
            // 構造とIDが同一なら表示中planの前提は変わらない。メタデータだけは
            // 最新スキャン結果へ差し替え、サイズ・更新日時の表示を鮮度維持する。
            self.baseline = baseline;
            return SaveFlowResult::NoChanges;
        }

        // 確認ダイアログ表示中の外部変更は、表示中のplanを陳腐化させる。
        // 承認済みとして実行すると古いbaseline前提の操作が実FSへ流れるため、
        // ここでキャンセル扱いにしてダイアログを閉じ、ユーザーへ通知する。
        if matches!(self.state, SaveState::AwaitingConfirmation { .. }) {
            self.apply_event(SaveEvent::Cancelled);
            self.pending_overwrites.clear();
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

        let context = carry_collapsed_dirs(&self.context, &self.baseline, &baseline);
        let lines = baseline_to_lines(&baseline, &context);
        if let Err(error) = self
            .engine
            .send(EditorCommand::SetLines {
                lines,
                cursor_line: None,
            })
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

        // 絶対ルール1: applyは、ApprovedでApplyingへ遷移した上の経路からだけ呼ぶ。
        let report = fyler_fsops::apply::apply_plan_with_overwrites(
            &self.root,
            &plan,
            &self.pending_overwrites,
        );
        self.pending_overwrites.clear();
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
        let context = carry_collapsed_dirs(&self.context, &self.baseline, &baseline);
        let lines = baseline_to_lines(&baseline, &context);
        self.engine
            .send(EditorCommand::SetLines {
                lines,
                cursor_line: None,
            })
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
        self.execute_modifiable_effects(&effects);
        effects
    }

    /// 状態機械が発行したバッファロック効果をエンジンへ送る。
    ///
    /// GUI側の入力ゲートも残す二重防御なので、エンジン送信失敗だけで保存状態遷移を
    /// 中断しない。失敗は診断用に標準エラーへ記録し、残りのフローを続行する。
    fn execute_modifiable_effects(&self, effects: &[SaveEffect]) {
        for value in effects.iter().filter_map(|effect| match effect {
            SaveEffect::SetModifiable(value) => Some(*value),
            _ => None,
        }) {
            if let Err(error) = self.engine.send(EditorCommand::SetModifiable(value)) {
                eprintln!("バッファのmodifiable設定をエンジンへ送信できません: {error:#}");
            }
        }
    }
}

/// planの読み取り元を属性だけで検査し、クラウド取得を伴い得る操作の警告を返す。
///
/// `is_placeholder`はテストで差し替え可能にし、本番では
/// [`fyler_fsops::onedrive::is_cloud_placeholder`]を渡す。述語またはmetadata取得に
/// 失敗した場合は保存計画を妨げず、サイズを取得できない場合だけサイズ表記を省略する。
fn plan_warnings(
    root: &Path,
    plan: &OperationPlan,
    is_placeholder: impl Fn(&Path) -> anyhow::Result<bool>,
) -> Vec<String> {
    plan.ops
        .iter()
        .filter_map(|operation| {
            let from = match operation {
                fyler_core::plan::FsOperation::Move { from, .. }
                | fyler_core::plan::FsOperation::Copy { from, .. } => from,
                fyler_core::plan::FsOperation::Create { .. }
                | fyler_core::plan::FsOperation::Delete { .. } => return None,
            };
            let source = from.to_fs_path(root);
            if !is_placeholder(&source).unwrap_or(false) {
                return None;
            }

            let size = fs::metadata(&source)
                .ok()
                .map(|metadata| human_readable_size(metadata.len()));
            Some(match size {
                Some(size) => format!("クラウドから取得します: {from}({size})"),
                None => format!("クラウドから取得します: {from}"),
            })
        })
        .collect()
}

/// 再スキャン後も既存ディレクトリの折りたたみ状態を維持し、新規ディレクトリは
/// 既定の折りたたみ状態で表示する。消滅したIDとディレクトリでなくなったIDは除く。
fn carry_collapsed_dirs(
    context: &EditContext,
    old_baseline: &BaselineTree,
    new_baseline: &BaselineTree,
) -> EditContext {
    let mut context = context.clone();
    context.collapsed_dirs.retain(|id| {
        new_baseline
            .get(*id)
            .is_some_and(|entry| entry.kind == EntryKind::Dir)
    });
    context.collapsed_dirs.extend(
        new_baseline
            .entries()
            .iter()
            .filter(|entry| entry.kind == EntryKind::Dir && old_baseline.get(entry.id).is_none())
            .map(|entry| entry.id),
    );
    context
}

/// 折りたたみ状態を反映した表示対象エントリを1パスで列挙する。
///
/// baselineの表示順は親の直後に全子孫が連続するDFS順であることを前提とする。
/// これは`fyler-fsops/src/scan.rs`の構築契約である。
fn visible_entries<'a>(
    baseline: &'a BaselineTree,
    context: &EditContext,
) -> Vec<&'a BaselineEntry> {
    let mut visible = Vec::new();
    let mut skip_prefix: Option<&TreePath> = None;

    for entry in baseline.entries() {
        if let Some(prefix) = skip_prefix {
            if prefix.is_strict_ancestor_of(&entry.path) {
                continue;
            }
            skip_prefix = None;
        }
        if entry.kind == EntryKind::Dir && context.collapsed_dirs.contains(&entry.id) {
            skip_prefix = Some(&entry.path);
        }
        visible.push(entry);
    }

    visible
}

pub(crate) fn baseline_to_lines(baseline: &BaselineTree, context: &EditContext) -> Vec<EditorLine> {
    visible_entries(baseline, context)
        .into_iter()
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

    fn nested_dirs_controller(root: impl Into<PathBuf>) -> (SaveController, Arc<RecordingEngine>) {
        let root = root.into();
        let mut ids = IdAllocator::new();
        let mut baseline = BaselineTree::new(&root);
        for path in ["a", "a/b", "a/b/c"] {
            baseline.insert(BaselineEntry {
                id: ids.allocate(),
                path: TreePath::parse(path),
                kind: EntryKind::Dir,
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

    fn legacy_baseline_to_lines(baseline: &BaselineTree, context: &EditContext) -> Vec<EditorLine> {
        let collapsed_paths = context
            .collapsed_dirs
            .iter()
            .filter_map(|id| baseline.get(*id))
            .filter(|entry| entry.kind == EntryKind::Dir)
            .map(|entry| &entry.path)
            .collect::<Vec<_>>();

        baseline
            .entries()
            .iter()
            .filter(|entry| {
                !collapsed_paths
                    .iter()
                    .any(|path| path.is_strict_ancestor_of(&entry.path))
            })
            .map(|entry| {
                let indent = " ".repeat(
                    entry.path.depth().saturating_sub(1) * fyler_core::grammar::INDENT_WIDTH,
                );
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

    fn modifiable_values(engine: &RecordingEngine) -> Vec<bool> {
        engine
            .commands
            .lock()
            .unwrap()
            .iter()
            .filter_map(|command| match command {
                EditorCommand::SetModifiable(value) => Some(*value),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn map_git_badges_matches_exact_root_relative_paths_only() {
        let (controller, _) = controller("C:/test-root");
        let entry_id = controller.baseline.entries()[0].id;
        let statuses = HashMap::from([
            (PathBuf::from("a.txt"), GitBadge::Modified),
            (PathBuf::from("other/a.txt"), GitBadge::Added),
        ]);

        assert_eq!(
            controller.map_git_badges(&statuses),
            HashMap::from([(entry_id, GitBadge::Modified)])
        );
    }

    #[test]
    fn map_git_badges_returns_empty_for_empty_statuses() {
        let (controller, _) = controller("C:/test-root");

        assert!(controller.map_git_badges(&HashMap::new()).is_empty());
    }

    #[test]
    fn map_git_badges_ignores_unmatched_paths() {
        let (controller, _) = controller("C:/test-root");
        let statuses = HashMap::from([(PathBuf::from("missing.txt"), GitBadge::Untracked)]);

        assert!(controller.map_git_badges(&statuses).is_empty());
    }

    #[test]
    fn rename_returns_confirmation_plan() {
        let (mut controller, engine) = controller("C:/test-root");

        let result = controller.on_commit(7, &lines(&["/001 b.txt"]));

        assert_eq!(
            result,
            SaveFlowResult::ShowPlan {
                plan: OperationPlan {
                    ops: vec![FsOperation::Move {
                        id: EntryId(1),
                        from: TreePath::parse("a.txt"),
                        to: TreePath::parse("b.txt"),
                    }],
                },
                warnings: Vec::new(),
                overwrites: Vec::new(),
            }
        );
        assert!(matches!(
            controller.state(),
            SaveState::AwaitingConfirmation { changedtick: 7, .. }
        ));
        assert_eq!(modifiable_values(&engine), [false]);
    }

    #[test]
    fn plan_warnings_include_placeholder_path_and_human_readable_size() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("cloud.bin"), vec![0_u8; 2048]).unwrap();
        let plan = OperationPlan {
            ops: vec![FsOperation::Copy {
                src: EntryId(1),
                from: TreePath::parse("cloud.bin"),
                to: TreePath::parse("cloud-copy.bin"),
            }],
        };

        let warnings = plan_warnings(root.path(), &plan, |path| {
            Ok(path == root.path().join("cloud.bin"))
        });

        assert_eq!(warnings, ["クラウドから取得します: cloud.bin(2.0 KB)"]);
    }

    #[test]
    fn plan_warnings_omit_size_when_metadata_is_unavailable() {
        let root = tempdir().unwrap();
        let plan = OperationPlan {
            ops: vec![FsOperation::Move {
                id: EntryId(1),
                from: TreePath::parse("missing.bin"),
                to: TreePath::parse("renamed.bin"),
            }],
        };

        let warnings = plan_warnings(root.path(), &plan, |_| Ok(true));

        assert_eq!(warnings, ["クラウドから取得します: missing.bin"]);
    }

    #[test]
    fn reserved_character_returns_validation_errors() {
        let (mut controller, engine) = controller("C:/test-root");

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
        assert_eq!(modifiable_values(&engine), [false, true]);
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
        let original_id = baseline.entries()[0].id;
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
            SaveFlowResult::ShowPlan { .. }
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
                .entries()
                .iter()
                .any(|entry| entry.path == TreePath::parse("b.txt"))
        );
        let commands = engine.commands.lock().unwrap();
        assert!(commands.iter().any(|command| matches!(
            command,
            EditorCommand::SetLines { lines, .. }
                if lines.iter().any(|line| line.text.ends_with("b.txt"))
                    && lines.iter().all(|line| !line.text.ends_with("a.txt"))
        )));
        assert_eq!(
            commands
                .iter()
                .filter_map(|command| match command {
                    EditorCommand::SetModifiable(value) => Some(*value),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            [false, true]
        );
    }

    #[test]
    fn hidden_file_conflict_is_approved_and_recycled_end_to_end() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("visible.txt"), b"source").unwrap();
        fs::write(root.path().join(".hidden"), b"existing").unwrap();
        let options = ScanOptions {
            show_hidden: false,
            ..ScanOptions::default()
        };
        let mut ids = IdAllocator::new();
        let baseline =
            fyler_fsops::scan::scan_baseline_with(root.path(), &mut ids, &options).unwrap();
        let visible_id = baseline.entries()[0].id;
        let engine = Arc::new(RecordingEngine::default());
        let mut controller = SaveController::new_with_scan_options(
            root.path().to_path_buf(),
            ids,
            baseline,
            Arc::<RecordingEngine>::clone(&engine),
            options,
        );
        let renamed_line = EditorLine::new(format!(
            "{}.hidden",
            fyler_core::grammar::format_id_prefix(visible_id)
        ));

        let plan_result = controller.on_commit(1, &[renamed_line]);

        assert!(matches!(
            plan_result,
            SaveFlowResult::ShowPlan {
                ref overwrites,
                ..
            } if overwrites == &[TreePath::parse(".hidden")]
        ));

        let apply_result = controller.on_choice(ConfirmChoice::Approve);

        assert!(matches!(
            apply_result,
            SaveFlowResult::ShowReport(ref report) if report.all_succeeded()
        ));
        assert!(!root.path().join("visible.txt").exists());
        assert_eq!(fs::read(root.path().join(".hidden")).unwrap(), b"source");
        assert!(controller.pending_overwrites.is_empty());
    }

    #[test]
    fn hidden_directory_conflict_returns_validation_error_and_keeps_dirty_buffer() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("visible.txt"), b"source").unwrap();
        fs::create_dir(root.path().join(".d")).unwrap();
        let options = ScanOptions {
            show_hidden: false,
            ..ScanOptions::default()
        };
        let mut ids = IdAllocator::new();
        let baseline =
            fyler_fsops::scan::scan_baseline_with(root.path(), &mut ids, &options).unwrap();
        let visible_id = baseline.entries()[0].id;
        let engine = Arc::new(RecordingEngine::default());
        engine.set_dirty(true);
        let mut controller = SaveController::new_with_scan_options(
            root.path().to_path_buf(),
            ids,
            baseline,
            Arc::<RecordingEngine>::clone(&engine),
            options,
        );
        let renamed_line = EditorLine::new(format!(
            "{}.d",
            fyler_core::grammar::format_id_prefix(visible_id)
        ));

        let result = controller.on_commit(1, &[renamed_line]);

        assert_eq!(
            result,
            SaveFlowResult::ShowValidationErrors(vec![ValidateError::TargetOccupiedByDirectory {
                path: TreePath::parse(".d"),
            },])
        );
        assert!(matches!(controller.state(), SaveState::Idle));
        assert!(engine.snapshot().dirty);
        assert!(root.path().join("visible.txt").is_file());
        assert!(root.path().join(".d").is_dir());
        assert_eq!(modifiable_values(&engine), [false, true]);
    }

    #[test]
    fn cancel_clears_pending_overwrites_and_recomputes_them_on_next_commit() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("visible.txt"), b"source").unwrap();
        fs::write(root.path().join(".hidden"), b"existing").unwrap();
        let options = ScanOptions {
            show_hidden: false,
            ..ScanOptions::default()
        };
        let mut ids = IdAllocator::new();
        let baseline =
            fyler_fsops::scan::scan_baseline_with(root.path(), &mut ids, &options).unwrap();
        let visible_id = baseline.entries()[0].id;
        let engine = Arc::new(RecordingEngine::default());
        let mut controller = SaveController::new_with_scan_options(
            root.path().to_path_buf(),
            ids,
            baseline,
            engine,
            options,
        );
        let renamed_line = EditorLine::new(format!(
            "{}.hidden",
            fyler_core::grammar::format_id_prefix(visible_id)
        ));
        assert!(matches!(
            controller.on_commit(1, std::slice::from_ref(&renamed_line)),
            SaveFlowResult::ShowPlan {
                ref overwrites,
                ..
            } if overwrites == &[TreePath::parse(".hidden")]
        ));

        assert_eq!(
            controller.on_choice(ConfirmChoice::Cancel),
            SaveFlowResult::Cancelled
        );
        assert!(controller.pending_overwrites.is_empty());

        assert!(matches!(
            controller.on_commit(2, &[renamed_line]),
            SaveFlowResult::ShowPlan {
                ref overwrites,
                ..
            } if overwrites == &[TreePath::parse(".hidden")]
        ));
    }

    #[test]
    fn cancel_leaves_filesystem_and_baseline_unchanged() {
        let temp_root = tempdir().unwrap();
        fs::write(temp_root.path().join("a.txt"), b"content").unwrap();
        let (mut controller, engine) = controller(temp_root.path());
        assert!(matches!(
            controller.on_commit(1, &lines(&["/001 b.txt"])),
            SaveFlowResult::ShowPlan { .. }
        ));

        assert_eq!(
            controller.on_choice(ConfirmChoice::Cancel),
            SaveFlowResult::Cancelled
        );

        assert!(matches!(controller.state(), SaveState::Idle));
        assert!(temp_root.path().join("a.txt").exists());
        assert!(!temp_root.path().join("b.txt").exists());
        assert_eq!(
            controller.baseline.entries()[0].path,
            TreePath::parse("a.txt")
        );
        assert_eq!(modifiable_values(&engine), [false, true]);
    }

    #[test]
    fn all_failed_returns_report_without_reconciling() {
        let temp_root = tempdir().unwrap();
        let (mut controller, engine) = controller(temp_root.path());
        assert!(matches!(
            controller.on_commit(1, &lines(&["/001 b.txt"])),
            SaveFlowResult::ShowPlan { .. }
        ));

        let result = controller.on_choice(ConfirmChoice::Approve);

        assert!(matches!(
            result,
            SaveFlowResult::ShowReport(ref report) if report.all_failed()
        ));
        assert!(matches!(controller.state(), SaveState::Idle));
        assert_eq!(
            controller.baseline.entries()[0].path,
            TreePath::parse("a.txt")
        );
        assert_eq!(modifiable_values(&engine), [false, true]);
    }

    #[test]
    fn external_change_replaces_clean_buffer_and_updates_baseline() {
        let temp_root = tempdir().unwrap();
        fs::write(temp_root.path().join("a.txt"), b"a").unwrap();
        let (mut controller, engine) = controller(temp_root.path());
        fs::write(temp_root.path().join("b.txt"), b"b").unwrap();

        let result = controller.on_external_change(&BTreeSet::new());

        assert_eq!(result, SaveFlowResult::ExternalChanged);
        assert!(
            controller
                .baseline
                .entries()
                .iter()
                .any(|entry| entry.path == TreePath::parse("b.txt"))
        );
        let commands = engine.commands.lock().unwrap();
        assert!(matches!(
            commands.as_slice(),
            [EditorCommand::SetLines { lines, .. }]
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

        let result = controller.on_external_change(&BTreeSet::new());

        assert!(matches!(
            result,
            SaveFlowResult::ExternalChangeNotified(ref message)
                if message.contains("外部でファイルが変更されました")
        ));
        assert!(
            controller
                .baseline
                .entries()
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

        let result = controller.on_external_change(&BTreeSet::new());

        assert_eq!(result, SaveFlowResult::NoChanges);
        assert!(engine.commands.lock().unwrap().is_empty());
    }

    #[test]
    fn metadata_only_external_change_refreshes_visible_file_info() {
        let temp_root = tempdir().unwrap();
        let file = temp_root.path().join("a.txt");
        fs::write(&file, b"a").unwrap();
        let (mut controller, engine) = controller(temp_root.path());
        fs::write(&file, b"longer content").unwrap();
        let changed_paths = BTreeSet::from([file]);

        let result = controller.on_external_change(&changed_paths);

        assert_eq!(result, SaveFlowResult::NoChanges);
        let id = controller.baseline.entries()[0].id;
        assert_eq!(
            controller.visible_file_infos().get(&id).unwrap().size,
            Some(14)
        );
        assert!(engine.commands.lock().unwrap().is_empty());
    }

    #[test]
    fn external_change_during_confirmation_invalidates_plan_and_blocks_approve() {
        // 確認ダイアログ表示中に実FSが変わると、表示中のplanは古いbaseline前提。
        // 承認済みとして実行せず破棄し、Idleへ戻す(その後のApproveは無効)。
        let temp_root = tempdir().unwrap();
        fs::write(temp_root.path().join("a.txt"), b"a").unwrap();
        let (mut controller, engine) = controller(temp_root.path());
        assert!(matches!(
            controller.on_commit(1, &lines(&["/001 b.txt"])),
            SaveFlowResult::ShowPlan { .. }
        ));
        fs::write(temp_root.path().join("c.txt"), b"c").unwrap();

        let result = controller.on_external_change(&BTreeSet::new());

        assert!(matches!(
            &result,
            SaveFlowResult::PlanInvalidated(message)
                if message.contains("外部でファイルが変更されたため、保存を中断しました")
        ));
        assert!(matches!(controller.state(), SaveState::Idle));
        assert_eq!(modifiable_values(&engine), [false, true]);
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
            SaveFlowResult::ShowPlan { .. }
        ));

        let result = controller.on_external_change(&BTreeSet::new());

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
        let id = controller.baseline.entries()[0].id;
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
    fn find_top_level_line_ignores_children_and_missing_names() {
        let (controller, _) = hierarchy_controller("C:/test-root");

        assert_eq!(
            controller.find_top_level_line(std::ffi::OsStr::new("a")),
            Some(0)
        );
        assert_eq!(
            controller.find_top_level_line(std::ffi::OsStr::new("top.txt")),
            Some(4)
        );
        assert_eq!(
            controller.find_top_level_line(std::ffi::OsStr::new("nested")),
            None
        );
        assert_eq!(
            controller.find_top_level_line(std::ffi::OsStr::new("missing")),
            None
        );
    }

    #[test]
    fn visible_file_infos_excludes_collapsed_descendants_and_restores_them_after_expand() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("directory")).unwrap();
        fs::write(root.path().join("directory").join("child.txt"), b"child").unwrap();
        fs::write(root.path().join("file.txt"), b"content").unwrap();
        let mut ids = IdAllocator::new();
        let baseline = fyler_fsops::scan::scan_baseline(root.path(), &mut ids).unwrap();
        let directory_id = baseline
            .entries()
            .iter()
            .find(|entry| entry.path == TreePath::parse("directory"))
            .unwrap()
            .id;
        let child_id = baseline
            .entries()
            .iter()
            .find(|entry| entry.path == TreePath::parse("directory/child.txt"))
            .unwrap()
            .id;
        let file_id = baseline
            .entries()
            .iter()
            .find(|entry| entry.path == TreePath::parse("file.txt"))
            .unwrap()
            .id;
        let engine = Arc::new(RecordingEngine::default());
        let mut controller = SaveController::new(
            root.path().to_path_buf(),
            ids,
            baseline,
            Arc::<RecordingEngine>::clone(&engine),
        );
        controller.context.collapsed_dirs.insert(directory_id);

        let collapsed_infos = controller.visible_file_infos();

        assert_eq!(collapsed_infos.len(), 2);
        assert!(collapsed_infos.contains_key(&directory_id));
        assert!(collapsed_infos.contains_key(&file_id));
        assert!(!collapsed_infos.contains_key(&child_id));

        let collapsed_lines = controller.visible_lines();
        assert!(matches!(
            controller.toggle_collapse(&collapsed_lines, 0),
            ToggleCollapseResult::Toggled(_)
        ));
        let expanded_infos = controller.visible_file_infos();
        assert_eq!(expanded_infos.len(), 3);
        assert!(expanded_infos.contains_key(&child_id));
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
            SaveFlowResult::ShowPlan { .. }
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
    fn visible_entries_matches_legacy_filter_for_nested_collapsed_dirs() {
        let (controller, _) = hierarchy_controller("C:/test-root");
        let context = EditContext {
            collapsed_dirs: [EntryId(1), EntryId(2)].into(),
        };

        assert_eq!(
            baseline_to_lines(&controller.baseline, &context),
            legacy_baseline_to_lines(&controller.baseline, &context)
        );
    }

    #[test]
    fn visible_entries_matches_legacy_filter_after_parent_is_expanded() {
        let (controller, _) = hierarchy_controller("C:/test-root");
        let context = EditContext {
            collapsed_dirs: [EntryId(2)].into(),
        };

        let actual = baseline_to_lines(&controller.baseline, &context);

        assert_eq!(
            actual,
            legacy_baseline_to_lines(&controller.baseline, &context)
        );
        assert!(actual.iter().any(|line| line.text.ends_with("nested/")));
        assert!(actual.iter().all(|line| !line.text.ends_with("leaf.txt")));
    }

    #[test]
    fn visible_entries_matches_legacy_filter_for_deep_hierarchy() {
        let mut baseline = BaselineTree::new("C:/test-root");
        for (index, (path, kind)) in [
            ("a", EntryKind::Dir),
            ("a/b", EntryKind::Dir),
            ("a/b/c", EntryKind::Dir),
            ("a/b/c/d", EntryKind::Dir),
            ("a/b/c/d/leaf.txt", EntryKind::File),
            ("top.txt", EntryKind::File),
        ]
        .into_iter()
        .enumerate()
        {
            baseline.insert(BaselineEntry {
                id: EntryId(index as u64 + 1),
                path: TreePath::parse(path),
                kind,
            });
        }
        let context = EditContext {
            collapsed_dirs: [EntryId(3)].into(),
        };

        assert_eq!(
            baseline_to_lines(&baseline, &context),
            legacy_baseline_to_lines(&baseline, &context)
        );
    }

    #[test]
    fn collapse_all_dirs_marks_every_directory_collapsed() {
        let (mut controller, _) = hierarchy_controller("C:/test-root");

        controller.collapse_all_dirs();

        let lines = controller.visible_lines();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].text.ends_with("a/"));
        assert!(lines[1].text.ends_with("top.txt"));
        assert_eq!(
            controller.context.collapsed_dirs,
            [EntryId(1), EntryId(2)].into()
        );
    }

    #[test]
    fn expanding_directory_reveals_only_one_level() {
        let (mut controller, _) = nested_dirs_controller("C:/test-root");
        controller.collapse_all_dirs();
        let collapsed = controller.visible_lines();

        let expanded_parent = match controller.toggle_collapse(&collapsed, 0) {
            ToggleCollapseResult::Toggled(lines) => lines,
            result => panic!("unexpected parent expand result: {result:?}"),
        };

        assert!(expanded_parent.iter().any(|line| line.text.ends_with("b/")));
        assert!(
            expanded_parent
                .iter()
                .all(|line| !line.text.ends_with("c/"))
        );
    }

    #[test]
    fn parent_collapse_and_reexpand_preserves_expanded_child() {
        let (mut controller, _) = nested_dirs_controller("C:/test-root");
        controller.collapse_all_dirs();
        let collapsed = controller.visible_lines();
        let expanded_parent = match controller.toggle_collapse(&collapsed, 0) {
            ToggleCollapseResult::Toggled(lines) => lines,
            result => panic!("unexpected parent expand result: {result:?}"),
        };
        let expanded_child = match controller.toggle_collapse(&expanded_parent, 1) {
            ToggleCollapseResult::Toggled(lines) => lines,
            result => panic!("unexpected child expand result: {result:?}"),
        };
        assert!(expanded_child.iter().any(|line| line.text.ends_with("c/")));

        let collapsed_parent = match controller.toggle_collapse(&expanded_child, 0) {
            ToggleCollapseResult::Toggled(lines) => lines,
            result => panic!("unexpected parent collapse result: {result:?}"),
        };
        let reexpanded_parent = match controller.toggle_collapse(&collapsed_parent, 0) {
            ToggleCollapseResult::Toggled(lines) => lines,
            result => panic!("unexpected parent re-expand result: {result:?}"),
        };

        assert!(
            reexpanded_parent
                .iter()
                .any(|line| line.text.ends_with("c/"))
        );
        assert!(!controller.context.collapsed_dirs.contains(&EntryId(2)));
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
            SaveFlowResult::ShowPlan { .. }
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
            SaveFlowResult::ShowPlan {
                plan: OperationPlan {
                    ops: vec![FsOperation::Move {
                        id: EntryId(1),
                        from: TreePath::parse("a"),
                        to: TreePath::parse("renamed"),
                    }],
                },
                warnings: Vec::new(),
                overwrites: Vec::new(),
            }
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
            controller.on_external_change(&BTreeSet::new()),
            SaveFlowResult::ExternalChanged
        );

        let commands = engine.commands.lock().unwrap();
        assert!(matches!(
            commands.last(),
            Some(EditorCommand::SetLines { lines, .. })
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

    #[test]
    fn toggle_hidden_adds_new_directory_as_collapsed() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join(".hidden")).unwrap();
        fs::write(root.path().join(".hidden").join("child.txt"), b"hidden").unwrap();
        let mut ids = IdAllocator::new();
        let baseline = fyler_fsops::scan::scan_baseline(root.path(), &mut ids).unwrap();
        let engine = Arc::new(RecordingEngine::default());
        let mut controller = SaveController::new(
            root.path().to_path_buf(),
            ids,
            baseline,
            Arc::<RecordingEngine>::clone(&engine),
        );
        controller.collapse_all_dirs();

        let shown = controller.toggle_hidden().unwrap();

        assert!(shown.iter().any(|line| line.text.ends_with(".hidden/")));
        assert!(shown.iter().all(|line| !line.text.ends_with("child.txt")));
        let hidden_dir = controller
            .baseline
            .entries()
            .iter()
            .find(|entry| entry.path == TreePath::parse(".hidden"))
            .unwrap();
        assert!(controller.context.collapsed_dirs.contains(&hidden_dir.id));
    }
}
