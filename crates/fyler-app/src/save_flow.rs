//! 保存フロー: parse → validate → diff → confirm → apply → reconcile。

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use fyler_core::editor::{EditorCommand, EditorEngine, EditorLine, FoldOp};
use fyler_core::fileinfo::{FileInfo, human_readable_size};
use fyler_core::gitstatus::GitBadge;
use fyler_core::grammar::PrefixParse;
use fyler_core::id::{EntryId, IdAllocator};
use fyler_core::options::SortKey;
use fyler_core::path::TreePath;
use fyler_core::plan::OperationPlan;
use fyler_core::report::CommitReport;
use fyler_core::save::{self, SaveEffect, SaveEvent, SaveState};
use fyler_core::tree::{BaselineEntry, BaselineTree, EditContext, EntryKind};
use fyler_core::undo::{UndoStep, UndoStepStatus, UndoTransaction};
use fyler_core::validate::ValidateError;
use fyler_fsops::scan::ScanOptions;
use fyler_gui::confirm::ConfirmChoice;

/// 保存フローから配線層へ返す結果。
#[derive(Debug, Clone)]
pub enum SaveFlowResult {
    /// 確認対象のplanと、実行時に発生し得るクラウド取得等の警告を表示する。
    ShowPlan {
        plan: OperationPlan,
        warnings: Vec<String>,
        /// 承認時に既存実体をごみ箱へ退避してから実行する移動先。plan順。
        overwrites: Vec<TreePath>,
    },
    /// 承認済みplanをworkerスレッドで実行する。
    ///
    /// この結果は保存状態機械が`Applying`へ遷移済みであることを保証する。
    /// 配線層は完了時に [`SaveController::on_apply_finished`] を呼ぶこと。
    StartApply {
        /// workerで実行する承認済みplan。
        plan: OperationPlan,
        /// ユーザーが承認した上書き対象。
        overwrites: HashSet<TreePath>,
        /// 操作間キャンセルをworkerへ通知する共有フラグ。
        cancel: Arc<AtomicBool>,
    },
    /// undo確認対象のtransactionと、step単位のpreflight結果を表示する。
    ShowUndoPlan {
        transaction: UndoTransaction,
        statuses: Vec<UndoStepStatus>,
    },
    /// undo preflightの結果、実行可能なstepが残っていない。
    UndoNothingLeft {
        reasons: Vec<String>,
    },
    /// 承認済みundo transactionをworkerスレッドで実行する。
    StartUndo {
        transaction: UndoTransaction,
        cancel: Arc<AtomicBool>,
    },
    /// apply実行中のキャンセル要求を受理した。残りの操作は操作間で停止する。
    ApplyCancelRequested,
    /// undo確認ダイアログをキャンセルした。transactionは呼び出し元がslotへ戻す。
    UndoCancelled {
        transaction: UndoTransaction,
    },
    /// undo確認ダイアログ表示中に外部変更を検知し、transactionを破棄せず返す。
    UndoInvalidated {
        transaction: UndoTransaction,
        message: String,
    },
    ShowValidationErrors(Vec<ValidateError>),
    ShowReport(CommitReport),
    ShowUndoReport(CommitReport<UndoStep>),
    ReconcileFailed {
        report: CommitReport,
        error: String,
    },
    UndoReconcileFailed {
        report: CommitReport<UndoStep>,
        error: String,
    },
    ExternalChanged,
    ExternalChangeNotified(String),
    ExternalChangeFailed(String),
    /// rootレベルの再スキャン失敗により、最後の成功表示を保持してofflineへ遷移した。
    WentOffline(String),
    /// offline rootの再スキャンに成功し、表示を再同期した。
    Reconnected(String),
    /// offline中の変更操作を拒否した。
    OfflineRejected(String),
    /// 確認ダイアログ表示中に外部変更を検知し、表示中のplanを破棄した。
    /// 配線層はダイアログを閉じ、メッセージを表示すること。
    PlanInvalidated(String),
    NoChanges,
    Cancelled,
    Ignored,
}

impl PartialEq for SaveFlowResult {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::ShowPlan {
                    plan: left_plan,
                    warnings: left_warnings,
                    overwrites: left_overwrites,
                },
                Self::ShowPlan {
                    plan: right_plan,
                    warnings: right_warnings,
                    overwrites: right_overwrites,
                },
            ) => {
                left_plan == right_plan
                    && left_warnings == right_warnings
                    && left_overwrites == right_overwrites
            }
            (
                Self::StartApply {
                    plan: left_plan,
                    overwrites: left_overwrites,
                    cancel: left_cancel,
                },
                Self::StartApply {
                    plan: right_plan,
                    overwrites: right_overwrites,
                    cancel: right_cancel,
                },
            ) => {
                left_plan == right_plan
                    && left_overwrites == right_overwrites
                    && Arc::ptr_eq(left_cancel, right_cancel)
            }
            (
                Self::ShowUndoPlan {
                    transaction: left_transaction,
                    statuses: left_statuses,
                },
                Self::ShowUndoPlan {
                    transaction: right_transaction,
                    statuses: right_statuses,
                },
            ) => left_transaction == right_transaction && left_statuses == right_statuses,
            (
                Self::UndoNothingLeft {
                    reasons: left_reasons,
                },
                Self::UndoNothingLeft {
                    reasons: right_reasons,
                },
            ) => left_reasons == right_reasons,
            (
                Self::StartUndo {
                    transaction: left_transaction,
                    cancel: left_cancel,
                },
                Self::StartUndo {
                    transaction: right_transaction,
                    cancel: right_cancel,
                },
            ) => left_transaction == right_transaction && Arc::ptr_eq(left_cancel, right_cancel),
            (
                Self::UndoCancelled {
                    transaction: left_transaction,
                },
                Self::UndoCancelled {
                    transaction: right_transaction,
                },
            ) => left_transaction == right_transaction,
            (
                Self::UndoInvalidated {
                    transaction: left_transaction,
                    message: left_message,
                },
                Self::UndoInvalidated {
                    transaction: right_transaction,
                    message: right_message,
                },
            ) => left_transaction == right_transaction && left_message == right_message,
            (Self::ApplyCancelRequested, Self::ApplyCancelRequested)
            | (Self::ExternalChanged, Self::ExternalChanged)
            | (Self::NoChanges, Self::NoChanges)
            | (Self::Cancelled, Self::Cancelled)
            | (Self::Ignored, Self::Ignored) => true,
            (Self::ShowValidationErrors(left), Self::ShowValidationErrors(right)) => left == right,
            (Self::ShowReport(left), Self::ShowReport(right)) => left == right,
            (Self::ShowUndoReport(left), Self::ShowUndoReport(right)) => left == right,
            (
                Self::ReconcileFailed {
                    report: left_report,
                    error: left_error,
                },
                Self::ReconcileFailed {
                    report: right_report,
                    error: right_error,
                },
            ) => left_report == right_report && left_error == right_error,
            (
                Self::UndoReconcileFailed {
                    report: left_report,
                    error: left_error,
                },
                Self::UndoReconcileFailed {
                    report: right_report,
                    error: right_error,
                },
            ) => left_report == right_report && left_error == right_error,
            (Self::ExternalChangeNotified(left), Self::ExternalChangeNotified(right))
            | (Self::ExternalChangeFailed(left), Self::ExternalChangeFailed(right))
            | (Self::WentOffline(left), Self::WentOffline(right))
            | (Self::Reconnected(left), Self::Reconnected(right))
            | (Self::OfflineRejected(left), Self::OfflineRejected(right))
            | (Self::PlanInvalidated(left), Self::PlanInvalidated(right)) => left == right,
            _ => false,
        }
    }
}

impl Eq for SaveFlowResult {}

/// ディレクトリ折りたたみ操作の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToggleCollapseResult {
    /// 折りたたみ状態を切り替え、バッファへ設定すべき全行を返す。
    Toggled(Vec<EditorLine>),
    /// 対象行を現在のbaselineへ解決できない。
    NotFound,
    /// 対象行はディレクトリではない。
    NotADirectory,
    /// scan不完全なディレクトリはstale子孫を見せないため展開できない。
    CannotExpandIncomplete,
    /// 対象ディレクトリは未列挙のため、app層で直下をロードしてから再試行する。
    NeedsLoad { dir: TreePath },
    /// 保存状態機械が`Idle`ではないため、状態を変更しなかった。
    Busy,
}

/// 折りたたみ操作の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FoldResult {
    /// 折りたたみ状態が変化した。linesをバッファへ設定し、cursor_lineへカーソルを移す。
    Applied {
        lines: Vec<EditorLine>,
        cursor_line: Option<usize>,
    },
    /// 状態が変化しなかった(既に開いている等)。
    NoOp,
    /// 行を解決できない(ID無し行・baseline不在)。
    NotFound,
    /// scan不完全なディレクトリを展開しようとした。
    CannotExpandIncomplete,
    /// 単一ディレクトリの直下ロード後に同じfold操作を再試行する。
    NeedsLoad { dir: TreePath },
    /// 再帰展開に必要な未ロード部分木をロードしてから同じfold操作を再試行する。
    NeedsLoadRecursive { dirs: Vec<TreePath> },
    /// 保存状態機械がIdleでない。
    Busy,
}

/// 折りたたまれた祖先を展開して、指定エントリを表示する結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevealResult {
    /// 対象は既に表示されている。`line`は0始まりの表示行index。
    AlreadyVisible { line: usize },
    /// 祖先を展開した。バッファへ設定すべき全行と対象の0始まり行index。
    Revealed { lines: Vec<EditorLine>, line: usize },
    /// 対象IDを現在のbaselineへ解決できない。
    NotFound,
    /// 保存状態機械が`Idle`ではないため、状態を変更しなかった。
    Busy,
}

pub struct SaveController {
    state: SaveState,
    root: PathBuf,
    ids: Arc<Mutex<IdAllocator>>,
    baseline: BaselineTree,
    context: EditContext,
    scan_options: ScanOptions,
    pending_overwrites: HashSet<TreePath>,
    apply_cancel: Option<Arc<AtomicBool>>,
    offline: Option<String>,
    reported_incomplete: BTreeSet<TreePath>,
    engine: Arc<dyn EditorEngine>,
}

impl SaveController {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(
        root: PathBuf,
        ids: IdAllocator,
        baseline: BaselineTree,
        engine: Arc<dyn EditorEngine>,
    ) -> Self {
        Self::new_shared(root, Arc::new(Mutex::new(ids)), baseline, engine)
    }

    /// 複数paneで共有するID採番器を使って保存フローを作成する。
    pub fn new_shared(
        root: PathBuf,
        ids: Arc<Mutex<IdAllocator>>,
        baseline: BaselineTree,
        engine: Arc<dyn EditorEngine>,
    ) -> Self {
        let context = context_with_unloaded_dirs(EditContext::default(), &baseline);
        Self {
            state: SaveState::Idle,
            root,
            ids,
            baseline,
            context,
            scan_options: ScanOptions::default(),
            pending_overwrites: HashSet::new(),
            apply_cancel: None,
            offline: None,
            reported_incomplete: BTreeSet::new(),
            engine,
        }
    }

    /// 初回スキャンに使った表示設定を保持して保存フローを作成する。
    ///
    /// 設定ファイル由来の隠しファイル表示とソート順を、再スキャン・ルート移動でも
    /// 維持する必要がある場合に使う。既定設定では [`Self::new`] と同じである。
    #[cfg_attr(not(test), allow(dead_code))]
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

    /// 複数paneで共有するID採番器と表示設定を使って保存フローを作成する。
    pub fn new_shared_with_scan_options(
        root: PathBuf,
        ids: Arc<Mutex<IdAllocator>>,
        baseline: BaselineTree,
        engine: Arc<dyn EditorEngine>,
        scan_options: ScanOptions,
    ) -> Self {
        let mut controller = Self::new_shared(root, ids, baseline, engine);
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

    /// rootの再スキャンが失敗し、最後の成功表示を保持しているかを返す。
    pub fn is_offline(&self) -> bool {
        self.offline.is_some()
    }

    /// GUI modelineへ渡す、読み取り不能なディレクトリ数。
    pub fn unreadable_count(&self) -> usize {
        self.baseline.incomplete_dirs().len()
    }

    /// 読み取り不能ディレクトリを現在baselineのEntryIdへ対応付ける。
    pub fn incomplete_dir_ids(&self) -> HashSet<EntryId> {
        self.baseline
            .incomplete_dirs()
            .keys()
            .filter_map(|path| self.baseline.get_by_path(path).map(|entry| entry.id))
            .collect()
    }

    /// 5秒tickerで再評価する不完全ディレクトリの実FSパスを返す。
    ///
    /// dirty中は回復してもbaselineを更新できず通知だけが反復するため休止する。
    /// offlineまたは保存状態機械がbusyの場合も、既存の専用フローへ委ねる。
    pub fn incomplete_probe_paths(&self) -> Option<BTreeSet<PathBuf>> {
        if self.is_offline() || !self.is_idle() || self.engine.snapshot().dirty {
            return None;
        }
        let paths = self
            .baseline
            .incomplete_dirs()
            .keys()
            .map(|path| path.to_fs_path(&self.root))
            .collect::<BTreeSet<_>>();
        (!paths.is_empty()).then_some(paths)
    }

    /// incomplete集合の変化時だけ、集約したユーザー向けメッセージを返す。
    pub fn take_scan_health_message(
        &mut self,
    ) -> Option<(fyler_core::editor::MessageKind, String)> {
        let current = self
            .baseline
            .incomplete_dirs()
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        if current == self.reported_incomplete {
            return None;
        }
        let was_incomplete = !self.reported_incomplete.is_empty();
        self.reported_incomplete = current.clone();
        if current.is_empty() {
            return was_incomplete.then(|| {
                (
                    fyler_core::editor::MessageKind::Info,
                    "All locations are readable again".to_owned(),
                )
            });
        }
        let first = current
            .iter()
            .next()
            .map(|path| path.to_fs_path(&self.root).display().to_string())
            .unwrap_or_else(|| self.root.display().to_string());
        let noun = if current.len() == 1 {
            "location"
        } else {
            "locations"
        };
        Some((
            fyler_core::editor::MessageKind::Warn,
            format!(
                "{} {noun} could not be fully read (first: {first}). They are shown collapsed; fix access and they will rescan automatically.",
                current.len(),
            ),
        ))
    }

    /// applyまたはundo workerの実行中かを返す。
    ///
    /// app層はこの判定を使い、外部変更イベントをworker完了後まで遅延する。
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_applying(&self) -> bool {
        matches!(
            self.state,
            SaveState::Applying { .. } | SaveState::ApplyingUndo { .. }
        )
    }

    /// 実行中undo transactionのIDを返す。
    ///
    /// app層がundo worker完了後にjournalを消費済みへ進めるために使う。状態機械の
    /// 所有権は移さず、`ApplyingUndo`以外では`None`を返す。
    pub fn applying_undo_transaction_id(&self) -> Option<&str> {
        match &self.state {
            SaveState::ApplyingUndo { transaction } => Some(&transaction.id),
            _ => None,
        }
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

    /// picker候補の構築と選択時のstale再解決に使う現在のbaselineを返す。
    pub fn baseline(&self) -> &BaselineTree {
        &self.baseline
    }

    /// 指定IDのエントリを隠している折りたたみ祖先をすべて展開する。
    ///
    /// 展開後はbaselineから全行を再生成し、その行列と1:1対応する0始まりindexを
    /// 返す。dirtyバッファへ全行差し替えを行うと編集を失うため、dirty判定は
    /// [`Self::toggle_collapse`] と同様に呼び出し元のapp層が先に行うこと。
    pub fn reveal_entry(&mut self, id: EntryId) -> RevealResult {
        if !self.is_idle() {
            return RevealResult::Busy;
        }
        let Some(target_path) = self.baseline.get(id).map(|entry| entry.path.clone()) else {
            return RevealResult::NotFound;
        };

        if let Some(line) = visible_entries(&self.baseline, &self.context)
            .iter()
            .position(|entry| entry.id == id)
        {
            return RevealResult::AlreadyVisible { line };
        }

        let collapsed_ancestors = self
            .baseline
            .entries()
            .iter()
            .filter(|entry| {
                entry.kind == EntryKind::Dir
                    && self.context.collapsed_dirs.contains(&entry.id)
                    && entry.path.is_strict_ancestor_of(&target_path)
            })
            .map(|entry| entry.id)
            .collect::<Vec<_>>();
        for ancestor in collapsed_ancestors {
            self.context.collapsed_dirs.remove(&ancestor);
        }

        let visible = visible_entries(&self.baseline, &self.context);
        let Some(line) = visible.iter().position(|entry| entry.id == id) else {
            return RevealResult::NotFound;
        };
        let lines = baseline_to_lines(&self.baseline, &self.context);
        debug_assert_eq!(visible.len(), lines.len());
        RevealResult::Revealed { lines, line }
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

    /// 現在のソート条件を返す。
    ///
    /// `:sort`引数なしの表示で使う。第1要素がソートキー、第2要素が降順フラグである。
    pub fn sort_state(&self) -> (SortKey, bool) {
        (self.scan_options.key, self.scan_options.reverse)
    }

    /// 表示ルート相対のGit状態を、現在のbaselineのエントリIDへ対応付けて返す。
    ///
    /// Gitがディレクトリ単位で報告した状態は同じパスのエントリだけへ付け、子孫や
    /// 親ディレクトリへ伝播しない。対応するエントリがない状態は無視する。
    pub fn map_git_badges(
        &self,
        statuses: &HashMap<PathBuf, GitBadge>,
    ) -> HashMap<EntryId, GitBadge> {
        statuses
            .iter()
            .filter_map(|(path, badge)| {
                let tree_path = tree_path_from_relative(path)?;
                self.baseline
                    .get_by_path(&tree_path)
                    .map(|entry| (entry.id, *badge))
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

    /// 現在折りたたまれているディレクトリのID集合を返す。
    ///
    /// GUIの展開/折りたたみアイコン判定に使う。子を持たない空ディレクトリは
    /// 表示行だけからは展開状態を判別できないため、この正典を渡す必要がある。
    pub fn collapsed_dirs(&self) -> HashSet<EntryId> {
        self.context.collapsed_dirs.clone()
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
        if self
            .baseline
            .get(id)
            .is_some_and(|entry| self.baseline.is_within_incomplete(&entry.path))
        {
            return ToggleCollapseResult::CannotExpandIncomplete;
        }
        if self.context.collapsed_dirs.contains(&id)
            && let Some(entry) = self.baseline.get(id)
            && self.baseline.is_unloaded(&entry.path)
        {
            return ToggleCollapseResult::NeedsLoad {
                dir: entry.path.clone(),
            };
        }
        if !self.context.collapsed_dirs.remove(&id) {
            self.context.collapsed_dirs.insert(id);
        }

        ToggleCollapseResult::Toggled(self.visible_lines())
    }

    /// z系コマンドによる折りたたみ状態変更を行う。
    ///
    /// 対象行は埋め込みIDを使って現在のbaselineへ解決する。Close系はファイル行や
    /// 既に閉じたディレクトリ行から親の展開中ディレクトリへ遡り、Open系は現在行の
    /// ディレクトリだけを対象にする。dirty判定は呼び出し元のapp層が先に行うこと。
    pub fn fold(&mut self, lines: &[EditorLine], line: usize, op: FoldOp) -> FoldResult {
        let Some(entry) = self.resolve_line_entry(lines, line) else {
            return FoldResult::NotFound;
        };
        if !self.is_idle() {
            return FoldResult::Busy;
        }

        let before = self.context.collapsed_dirs.clone();
        let expansion_intersects_incomplete = match op {
            FoldOp::Open | FoldOp::Toggle => {
                entry.kind == EntryKind::Dir && self.baseline.is_within_incomplete(&entry.path)
            }
            FoldOp::OpenRecursive => {
                entry.kind == EntryKind::Dir
                    && self.baseline.subtree_intersects_incomplete(&entry.path)
            }
            FoldOp::OpenAll => !self.baseline.incomplete_dirs().is_empty(),
            FoldOp::Close | FoldOp::CloseRecursive | FoldOp::CloseAll => false,
        };
        if expansion_intersects_incomplete {
            return FoldResult::CannotExpandIncomplete;
        }
        let unloaded = match op {
            FoldOp::Open | FoldOp::Toggle
                if entry.kind == EntryKind::Dir
                    && self.context.collapsed_dirs.contains(&entry.id)
                    && self.baseline.is_unloaded(&entry.path) =>
            {
                return FoldResult::NeedsLoad {
                    dir: entry.path.clone(),
                };
            }
            FoldOp::OpenRecursive if entry.kind == EntryKind::Dir => {
                unloaded_roots_within(&self.baseline, &entry.path)
            }
            FoldOp::OpenAll => self.baseline.unloaded_dirs().iter().cloned().collect(),
            _ => Vec::new(),
        };
        if !unloaded.is_empty() {
            return FoldResult::NeedsLoadRecursive { dirs: unloaded };
        }
        let cursor_id = match op {
            FoldOp::Close => {
                let Some(target) = self.close_target_for_entry(&entry) else {
                    return FoldResult::NoOp;
                };
                self.context.collapsed_dirs.insert(target.id);
                target.id
            }
            FoldOp::Open => {
                if entry.kind != EntryKind::Dir {
                    return FoldResult::NoOp;
                }
                self.context.collapsed_dirs.remove(&entry.id);
                entry.id
            }
            FoldOp::Toggle => {
                if entry.kind == EntryKind::Dir {
                    if !self.context.collapsed_dirs.remove(&entry.id) {
                        self.context.collapsed_dirs.insert(entry.id);
                    }
                    entry.id
                } else {
                    let Some(target) = self.close_target_for_entry(&entry) else {
                        return FoldResult::NoOp;
                    };
                    self.context.collapsed_dirs.insert(target.id);
                    target.id
                }
            }
            FoldOp::CloseRecursive => {
                let Some(target) = self.close_target_for_entry(&entry) else {
                    return FoldResult::NoOp;
                };
                self.collapse_dir_recursive(&target.path);
                target.id
            }
            FoldOp::OpenRecursive => {
                if entry.kind != EntryKind::Dir {
                    return FoldResult::NoOp;
                }
                self.expand_dir_recursive(&entry.path);
                entry.id
            }
            FoldOp::CloseAll => {
                self.collapse_all_dirs();
                top_level_ancestor_entry(&self.baseline, &entry)
                    .map(|entry| entry.id)
                    .unwrap_or(entry.id)
            }
            FoldOp::OpenAll => {
                self.context.collapsed_dirs.clear();
                entry.id
            }
        };

        if self.context.collapsed_dirs == before {
            return FoldResult::NoOp;
        }

        FoldResult::Applied {
            lines: self.visible_lines(),
            cursor_line: visible_position_by_id(&self.baseline, &self.context, cursor_id),
        }
    }

    fn resolve_line_entry(&self, lines: &[EditorLine], line: usize) -> Option<BaselineEntry> {
        let editor_line = lines.get(line)?;
        let PrefixParse::WithId { id, .. } =
            fyler_core::grammar::split_id_prefix(&editor_line.text)
        else {
            return None;
        };
        self.baseline.get(id).cloned()
    }

    /// 非同期loaderが返した同一rootのbaselineを、現在の折りたたみ状態を保って受領する。
    pub fn replace_baseline_after_load(&mut self, baseline: BaselineTree) -> anyhow::Result<()> {
        if !self.is_idle() {
            anyhow::bail!("Cannot install a loaded directory while saving");
        }
        if baseline.root != self.root {
            anyhow::bail!(
                "Root does not match loaded baseline: root={}, baseline={}",
                self.root.display(),
                baseline.root.display()
            );
        }
        let context = carry_collapsed_dirs(&self.context, &self.baseline, &baseline);
        self.baseline = baseline;
        self.context = context;
        Ok(())
    }

    fn close_target_for_entry(&self, entry: &BaselineEntry) -> Option<BaselineEntry> {
        if entry.kind == EntryKind::Dir && !self.context.collapsed_dirs.contains(&entry.id) {
            return Some(entry.clone());
        }

        let mut parent = entry.path.parent();
        while let Some(path) = parent {
            if let Some(candidate) = entry_by_path(&self.baseline, &path)
                && candidate.kind == EntryKind::Dir
                && !self.context.collapsed_dirs.contains(&candidate.id)
            {
                return Some(candidate.clone());
            }
            parent = path.parent();
        }

        None
    }

    fn collapse_dir_recursive(&mut self, path: &TreePath) {
        let ids = self
            .baseline
            .entries()
            .iter()
            .filter(|entry| {
                entry.kind == EntryKind::Dir
                    && (&entry.path == path || path.is_strict_ancestor_of(&entry.path))
            })
            .map(|entry| entry.id)
            .collect::<Vec<_>>();
        self.context.collapsed_dirs.extend(ids);
    }

    fn expand_dir_recursive(&mut self, path: &TreePath) {
        let ids = self
            .baseline
            .entries()
            .iter()
            .filter(|entry| {
                entry.kind == EntryKind::Dir
                    && (&entry.path == path || path.is_strict_ancestor_of(&entry.path))
            })
            .map(|entry| entry.id)
            .collect::<HashSet<_>>();
        self.context.collapsed_dirs.retain(|id| !ids.contains(id));
    }

    /// 隠しファイル表示を切り替えて現在のルートを再スキャンする。
    ///
    /// 保存状態機械が`Idle`のときだけ実行し、同じパスのIDと新baselineにも実在する
    /// 折りたたみIDを維持する。戻り値はバッファへ設定すべき全行である。
    pub fn toggle_hidden(&mut self) -> anyhow::Result<Vec<EditorLine>> {
        if self.is_offline() {
            anyhow::bail!("Cannot change hidden-file visibility while the folder is offline");
        }
        if !self.is_idle() {
            anyhow::bail!("Cannot change hidden-file visibility while saving");
        }

        let options = ScanOptions {
            show_hidden: !self.scan_options.show_hidden,
            ..self.scan_options
        };
        let mut ids = self
            .ids
            .lock()
            .map_err(|_| anyhow::anyhow!("ID allocator lock is poisoned"))?;
        let baseline_result = fyler_fsops::scan::rescan_preserving_ids_with(
            &self.root,
            &mut ids,
            &self.baseline,
            &options,
        )
        .context("Failed to rescan file system after toggling hidden files");
        drop(ids);
        let baseline = match baseline_result {
            Ok(baseline) => baseline,
            Err(error) => {
                let _ = self.enter_offline(error.to_string());
                return Err(error);
            }
        };
        let context = carry_collapsed_dirs(&self.context, &self.baseline, &baseline);
        let lines = baseline_to_lines(&baseline, &context);

        self.baseline = baseline;
        self.context = context;
        self.scan_options = options;
        Ok(lines)
    }

    /// ソート条件を変更して現在のルートを再スキャンする。
    ///
    /// 保存状態機械が`Idle`のときだけ実行し、IDと折りたたみ状態を維持する。
    /// 戻り値はバッファへ設定すべき全行である。
    pub fn change_sort(&mut self, key: SortKey, reverse: bool) -> anyhow::Result<Vec<EditorLine>> {
        if self.is_offline() {
            anyhow::bail!("Cannot change sorting while the folder is offline");
        }
        if !self.is_idle() {
            anyhow::bail!("Cannot change sorting while saving");
        }

        let options = ScanOptions {
            key,
            reverse,
            ..self.scan_options
        };
        if options == self.scan_options {
            return Ok(self.visible_lines());
        }

        let mut ids = self
            .ids
            .lock()
            .map_err(|_| anyhow::anyhow!("ID allocator lock is poisoned"))?;
        let baseline_result = fyler_fsops::scan::rescan_preserving_ids_with(
            &self.root,
            &mut ids,
            &self.baseline,
            &options,
        )
        .context("Failed to rescan file system after changing sorting");
        drop(ids);
        let baseline = match baseline_result {
            Ok(baseline) => baseline,
            Err(error) => {
                let _ = self.enter_offline(error.to_string());
                return Err(error);
            }
        };
        let context = carry_collapsed_dirs(&self.context, &self.baseline, &baseline);
        let lines = baseline_to_lines(&baseline, &context);

        self.baseline = baseline;
        self.context = context;
        self.scan_options = options;
        Ok(lines)
    }

    /// 共有ID採番器を維持したまま表示ルートとbaselineを差し替える。
    ///
    /// 保存状態機械が`Idle`のときだけ成功する。成功時はルート固有の編集文脈も
    /// リセットする。`baseline.root`が`root`と一致しない入力は拒否し、既存状態を
    /// 変更しない。
    pub fn change_root_preserving_allocator(
        &mut self,
        root: PathBuf,
        baseline: BaselineTree,
    ) -> anyhow::Result<()> {
        if !self.is_idle() {
            anyhow::bail!("Cannot change root while saving");
        }
        if baseline.root != root {
            anyhow::bail!(
                "Root does not match baseline root: root={}, baseline={}",
                root.display(),
                baseline.root.display()
            );
        }
        let was_offline = self.is_offline();
        self.root = root;
        self.baseline = baseline;
        self.context = context_with_unloaded_dirs(EditContext::default(), &self.baseline);
        self.offline = None;
        self.reported_incomplete.clear();
        if was_offline && let Err(error) = self.engine.send(EditorCommand::SetModifiable(true)) {
            eprintln!("Failed to restore buffer modifiable setting after root change: {error:#}");
        }
        Ok(())
    }

    pub fn on_commit(&mut self, changedtick: u64, lines: &[EditorLine]) -> SaveFlowResult {
        if self.is_offline() {
            return SaveFlowResult::OfflineRejected(
                "The folder is offline or unreachable. Reconnect and it will refresh automatically."
                    .to_owned(),
            );
        }
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
            &self.baseline,
            &display_plan,
            &overwrites,
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

    /// `:FylerUndo` 起点。Idle以外やdirty状態は呼び出し元でゲートするが、
    /// 防御としてIdle以外では状態を変更しない。
    ///
    /// `preflight_undo`で現在の実FSに対するundo可否を検査し、実行可能なstepが
    /// 1つも残っていない場合は状態機械へ入らず理由だけを返す。
    pub fn request_undo(&mut self, transaction: UndoTransaction) -> SaveFlowResult {
        if self.is_offline() {
            return SaveFlowResult::OfflineRejected(
                "Cannot undo while the folder is offline or unreachable".to_owned(),
            );
        }
        if !matches!(self.state, SaveState::Idle) {
            return SaveFlowResult::Ignored;
        }

        let statuses = fyler_fsops::preflight_undo(&transaction);
        if !statuses
            .iter()
            .any(|status| matches!(status, UndoStepStatus::Ready))
        {
            let mut reasons = statuses
                .into_iter()
                .filter_map(|status| match status {
                    UndoStepStatus::Ready => None,
                    UndoStepStatus::Rejected { reason } => Some(reason),
                })
                .collect::<Vec<_>>();
            if reasons.is_empty() {
                reasons.push("No operations are available to undo".to_owned());
            }
            return SaveFlowResult::UndoNothingLeft { reasons };
        }

        let effects = self.apply_event(SaveEvent::UndoRequested {
            transaction: transaction.clone(),
        });
        if effects
            .iter()
            .any(|effect| matches!(effect, SaveEffect::ShowUndoConfirmDialog))
        {
            SaveFlowResult::ShowUndoPlan {
                transaction,
                statuses,
            }
        } else {
            SaveFlowResult::Ignored
        }
    }

    pub fn on_choice(&mut self, choice: ConfirmChoice) -> SaveFlowResult {
        if matches!(
            self.state,
            SaveState::Applying { .. } | SaveState::ApplyingUndo { .. }
        ) {
            if matches!(choice, ConfirmChoice::Cancel)
                && let Some(cancel) = &self.apply_cancel
            {
                cancel.store(true, Ordering::Relaxed);
                return SaveFlowResult::ApplyCancelRequested;
            }
            return SaveFlowResult::Ignored;
        }

        if matches!(self.state, SaveState::AwaitingUndoConfirmation { .. }) {
            return match choice {
                ConfirmChoice::Cancel => {
                    let transaction = match &self.state {
                        SaveState::AwaitingUndoConfirmation { transaction } => transaction.clone(),
                        _ => return SaveFlowResult::Ignored,
                    };
                    self.apply_event(SaveEvent::Cancelled);
                    SaveFlowResult::UndoCancelled { transaction }
                }
                ConfirmChoice::Approve => self.approve_and_undo(),
                ConfirmChoice::OpenWithSelected(_) => SaveFlowResult::Ignored,
            };
        }

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
            ConfirmChoice::OpenWithSelected(_) => SaveFlowResult::Ignored,
        }
    }

    /// workerスレッドでのapply完了を状態機械へ反映し、必要ならreconcileする。
    ///
    /// `Applying`状態以外で呼ばれた場合は状態を変更せず
    /// [`SaveFlowResult::Ignored`]を返す。
    pub fn on_apply_finished(&mut self, report: CommitReport) -> SaveFlowResult {
        if !matches!(self.state, SaveState::Applying { .. }) {
            return SaveFlowResult::Ignored;
        }

        let effects = self.apply_event(SaveEvent::ApplyFinished {
            report: report.clone(),
        });
        debug_assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, SaveEffect::ShowCommitReport(_)))
        );

        let result = if effects
            .iter()
            .any(|effect| matches!(effect, SaveEffect::ReconcileFromFs))
        {
            match self.reconcile_from_fs() {
                Ok(()) => SaveFlowResult::ShowReport(report),
                Err(error) => SaveFlowResult::ReconcileFailed {
                    report,
                    error: error.to_string(),
                },
            }
        } else {
            SaveFlowResult::ShowReport(report)
        };

        self.apply_cancel = None;
        self.pending_overwrites.clear();
        result
    }

    /// undo worker完了を状態機械へ反映し、成功があれば実FSから再同期する。
    ///
    /// `ApplyingUndo`状態以外で呼ばれた場合は状態を変更せず
    /// [`SaveFlowResult::Ignored`]を返す。
    pub fn on_undo_finished(&mut self, report: CommitReport<UndoStep>) -> SaveFlowResult {
        if !matches!(self.state, SaveState::ApplyingUndo { .. }) {
            return SaveFlowResult::Ignored;
        }

        let effects = self.apply_event(SaveEvent::UndoApplyFinished {
            report: report.clone(),
        });
        debug_assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, SaveEffect::ShowUndoReport(_)))
        );

        if effects
            .iter()
            .any(|effect| matches!(effect, SaveEffect::ReconcileFromFs))
            && let Err(error) = self.reconcile_from_fs()
        {
            self.apply_cancel = None;
            return SaveFlowResult::UndoReconcileFailed {
                report,
                error: error.to_string(),
            };
        }

        self.apply_cancel = None;
        SaveFlowResult::ShowUndoReport(report)
    }

    pub fn on_external_change(&mut self, changed_paths: &BTreeSet<PathBuf>) -> SaveFlowResult {
        let mut ids = match self.ids.lock() {
            Ok(ids) => ids,
            Err(_) => {
                return SaveFlowResult::ExternalChangeFailed(
                    "ID allocator lock is poisoned".to_owned(),
                );
            }
        };
        let baseline_result = fyler_fsops::scan::rescan_changed_preserving_ids_with(
            &self.root,
            &mut ids,
            &self.baseline,
            changed_paths,
            &self.scan_options,
        )
        .context("Failed to rescan file system after external change");
        drop(ids);
        let baseline = match baseline_result {
            Ok(baseline) => baseline,
            Err(error) => return self.enter_offline(error.to_string()),
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
        if let SaveState::AwaitingUndoConfirmation { transaction } = &self.state {
            let transaction = transaction.clone();
            self.apply_event(SaveEvent::Cancelled);
            return SaveFlowResult::UndoInvalidated {
                transaction,
                message:
                    "Undo was cancelled because files changed externally. Review the changes and run :FylerUndo again."
                        .to_owned(),
            };
        }

        if matches!(self.state, SaveState::AwaitingConfirmation { .. }) {
            self.apply_event(SaveEvent::Cancelled);
            self.pending_overwrites.clear();
            return SaveFlowResult::PlanInvalidated(
                "Save was cancelled because files changed externally. Review the changes and run :w again."
                    .to_owned(),
            );
        }

        if self.engine.snapshot().dirty {
            return SaveFlowResult::ExternalChangeNotified(
                "Files changed externally. The view was not updated because you are editing."
                    .to_owned(),
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
            .context("Failed to send buffer lines to the engine after external change")
        {
            return SaveFlowResult::ExternalChangeFailed(error.to_string());
        }

        self.baseline = baseline;
        self.context = context;
        SaveFlowResult::ExternalChanged
    }

    /// 5秒retry tickからoffline rootをフル再スキャンする。
    pub fn retry_offline(&mut self) -> SaveFlowResult {
        if !self.is_offline() {
            return SaveFlowResult::Ignored;
        }
        let mut ids = match self.ids.lock() {
            Ok(ids) => ids,
            Err(_) => {
                return SaveFlowResult::ExternalChangeFailed(
                    "ID allocator lock is poisoned".to_owned(),
                );
            }
        };
        let baseline = match fyler_fsops::scan::rescan_preserving_ids_with(
            &self.root,
            &mut ids,
            &self.baseline,
            &self.scan_options,
        ) {
            Ok(baseline) => baseline,
            Err(error) => {
                self.offline = Some(error.to_string());
                return SaveFlowResult::Ignored;
            }
        };
        drop(ids);

        let context = carry_collapsed_dirs(&self.context, &self.baseline, &baseline);
        if !self.engine.snapshot().dirty
            && let Err(error) = self.engine.send(EditorCommand::SetLines {
                lines: baseline_to_lines(&baseline, &context),
                cursor_line: None,
            })
        {
            return SaveFlowResult::ExternalChangeFailed(error.to_string());
        }
        self.baseline = baseline;
        self.context = context;
        self.offline = None;
        if let Err(error) = self.engine.send(EditorCommand::SetModifiable(true)) {
            eprintln!("Failed to restore buffer modifiable setting after reconnect: {error:#}");
        }
        SaveFlowResult::Reconnected(format!("Reconnected: {}", self.root.display()))
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

        let cancel = Arc::new(AtomicBool::new(false));
        self.apply_cancel = Some(Arc::clone(&cancel));
        SaveFlowResult::StartApply {
            plan,
            overwrites: self.pending_overwrites.clone(),
            cancel,
        }
    }

    fn approve_and_undo(&mut self) -> SaveFlowResult {
        let effects = self.apply_event(SaveEvent::Approved);
        if !effects
            .iter()
            .any(|effect| matches!(effect, SaveEffect::ExecuteUndo))
        {
            return SaveFlowResult::Ignored;
        }

        let transaction = match &self.state {
            SaveState::ApplyingUndo { transaction } => transaction.clone(),
            _ => return SaveFlowResult::Ignored,
        };

        let cancel = Arc::new(AtomicBool::new(false));
        self.apply_cancel = Some(Arc::clone(&cancel));
        SaveFlowResult::StartUndo {
            transaction,
            cancel,
        }
    }

    fn reconcile_from_fs(&mut self) -> anyhow::Result<()> {
        let result = self.reconcile_from_fs_preserving_state();
        let effects = self.apply_event(SaveEvent::ReconcileFinished);
        debug_assert!(matches!(self.state, SaveState::Idle));
        debug_assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, SaveEffect::SetModifiable(true)))
        );
        if result.is_err()
            && self.is_offline()
            && let Err(error) = self.engine.send(EditorCommand::SetModifiable(false))
        {
            eprintln!("Failed to keep offline buffer read-only: {error:#}");
        }
        result
    }

    /// pane間transfer完了後に、保存状態機械を変更せず実FSから再同期する。
    pub fn reconcile_after_transfer(&mut self) -> anyhow::Result<()> {
        debug_assert!(self.is_idle());
        self.reconcile_from_fs_preserving_state()
    }

    fn reconcile_from_fs_preserving_state(&mut self) -> anyhow::Result<()> {
        let mut ids = self
            .ids
            .lock()
            .map_err(|_| anyhow::anyhow!("ID allocator lock is poisoned"))?;
        let baseline_result = fyler_fsops::scan::rescan_preserving_ids_with(
            &self.root,
            &mut ids,
            &self.baseline,
            &self.scan_options,
        )
        .context("Failed to rescan file system");
        drop(ids);
        let baseline = match baseline_result {
            Ok(baseline) => baseline,
            Err(error) => {
                let _ = self.enter_offline(error.to_string());
                return Err(error);
            }
        };
        let context = carry_collapsed_dirs(&self.context, &self.baseline, &baseline);
        let lines = baseline_to_lines(&baseline, &context);
        self.engine
            .send(EditorCommand::SetLines {
                lines,
                cursor_line: None,
            })
            .context("Failed to send buffer lines to the engine after reconcile")?;

        self.baseline = baseline;
        self.context = context;
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
                eprintln!("Failed to send buffer modifiable setting to the engine: {error:#}");
            }
        }
    }

    fn enter_offline(&mut self, message: String) -> SaveFlowResult {
        let transitioned = self.offline.is_none();
        self.offline = Some(message.clone());
        if transitioned
            && self.is_idle()
            && let Err(error) = self.engine.send(EditorCommand::SetModifiable(false))
        {
            eprintln!("Failed to make offline buffer read-only: {error:#}");
        }
        if transitioned {
            SaveFlowResult::WentOffline(message)
        } else {
            SaveFlowResult::Ignored
        }
    }
}

/// 表示ルート相対のOSパスをエンジン非依存のツリーパスへ変換する。
///
/// UTF-8で表現できない成分を含む場合は、Git状態とbaselineを安全に対応付けられない
/// ため`None`を返す。
fn tree_path_from_relative(path: &Path) -> Option<TreePath> {
    path.components()
        .map(|component| match component {
            std::path::Component::Normal(name) => name.to_str().map(ToOwned::to_owned),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()
        .map(TreePath::from_components)
}

/// planの読み取り元を属性だけで検査し、クラウド取得を伴い得る操作の警告を返す。
///
/// `is_placeholder`はテストで差し替え可能にし、本番では
/// [`fyler_fsops::onedrive::is_cloud_placeholder`]を渡す。述語またはmetadata取得に
/// 失敗した場合は保存計画を妨げず、サイズを取得できない場合だけサイズ表記を省略する。
fn plan_warnings(
    root: &Path,
    baseline: &BaselineTree,
    plan: &OperationPlan,
    overwrites: &[TreePath],
    is_placeholder: impl Fn(&Path) -> anyhow::Result<bool>,
) -> Vec<String> {
    let mut warnings = Vec::new();

    for operation in &plan.ops {
        let from = match operation {
            fyler_core::plan::FsOperation::Move { from, .. }
            | fyler_core::plan::FsOperation::Copy { from, .. } => from,
            fyler_core::plan::FsOperation::Create { .. }
            | fyler_core::plan::FsOperation::Delete { .. } => continue,
        };
        let source = from.to_fs_path(root);
        if !is_placeholder(&source).unwrap_or(false) {
            continue;
        }

        let size = fs::metadata(&source).ok().map(|metadata| metadata.len());
        warnings.push(hydration_warning(from, size));
    }

    append_delete_backup_warnings(&mut warnings, baseline, plan);
    append_overwrite_backup_warnings(&mut warnings, root, overwrites, is_placeholder);
    warnings
}

fn append_delete_backup_warnings(
    warnings: &mut Vec<String>,
    baseline: &BaselineTree,
    plan: &OperationPlan,
) {
    let mut saw_delete = false;
    let mut total_size = 0_u64;
    let mut has_unknown_size = false;

    for operation in &plan.ops {
        let fyler_core::plan::FsOperation::Delete { path, .. } = operation else {
            continue;
        };
        saw_delete = true;

        let mut matched = false;
        for entry in baseline
            .entries()
            .iter()
            .filter(|entry| &entry.path == path || path.is_strict_ancestor_of(&entry.path))
        {
            matched = true;
            let Some(meta) = baseline.meta(entry.id) else {
                has_unknown_size = true;
                continue;
            };
            if let Some(size) = meta.size {
                total_size = total_size.saturating_add(size);
            } else if entry.kind != EntryKind::Dir {
                has_unknown_size = true;
            }
            if meta.is_placeholder {
                warnings.push(hydration_warning(&entry.path, meta.size));
            }
        }
        if !matched {
            has_unknown_size = true;
        }
    }

    if saw_delete {
        warnings.push(backup_warning(
            "A backup will be created before deletion",
            total_size,
            has_unknown_size,
        ));
    }
}

fn append_overwrite_backup_warnings(
    warnings: &mut Vec<String>,
    root: &Path,
    overwrites: &[TreePath],
    is_placeholder: impl Fn(&Path) -> anyhow::Result<bool>,
) {
    if overwrites.is_empty() {
        return;
    }

    let mut total_size = 0_u64;
    let mut has_unknown_size = false;
    for path in overwrites {
        let fs_path = path.to_fs_path(root);
        let size = match fs::symlink_metadata(&fs_path) {
            Ok(metadata) if metadata.is_dir() => None,
            Ok(metadata) => Some(metadata.len()),
            Err(_) => {
                has_unknown_size = true;
                None
            }
        };
        if let Some(size) = size {
            total_size = total_size.saturating_add(size);
        }
        if is_placeholder(&fs_path).unwrap_or(false) {
            warnings.push(hydration_warning(path, size));
        }
    }

    warnings.push(backup_warning(
        "A backup will be created before overwrite",
        total_size,
        has_unknown_size,
    ));
}

fn hydration_warning(path: &TreePath, size: Option<u64>) -> String {
    match size {
        Some(size) => format!(
            "Will download from the cloud: {path}({})",
            human_readable_size(size)
        ),
        None => format!("Will download from the cloud: {path}"),
    }
}

fn backup_warning(prefix: &str, total_size: u64, has_unknown_size: bool) -> String {
    let unknown_suffix = if has_unknown_size {
        " (some sizes unknown)"
    } else {
        ""
    };
    format!(
        "{prefix}: approximately {}{}",
        human_readable_size(total_size),
        unknown_suffix
    )
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
    context_with_unloaded_dirs(context, new_baseline)
}

/// 未ロードdirはdiff・表示の既存collapsed意味論へ必ず合流させる。
fn context_with_unloaded_dirs(mut context: EditContext, baseline: &BaselineTree) -> EditContext {
    context.collapsed_dirs.extend(
        baseline
            .unloaded_dirs()
            .iter()
            .filter_map(|path| baseline.get_by_path(path).map(|entry| entry.id)),
    );
    context
}

/// 指定部分木内の未ロードdirから、別の未ロードdir配下にある重複対象を除く。
fn unloaded_roots_within(baseline: &BaselineTree, root: &TreePath) -> Vec<TreePath> {
    let mut roots = Vec::<TreePath>::new();
    for path in baseline.unloaded_dirs() {
        if path != root && !root.is_strict_ancestor_of(path) {
            continue;
        }
        if roots
            .iter()
            .any(|ancestor| ancestor == path || ancestor.is_strict_ancestor_of(path))
        {
            continue;
        }
        roots.push(path.clone());
    }
    roots
}

fn entry_by_path<'a>(baseline: &'a BaselineTree, path: &TreePath) -> Option<&'a BaselineEntry> {
    baseline.entries().iter().find(|entry| &entry.path == path)
}

fn top_level_ancestor_entry<'a>(
    baseline: &'a BaselineTree,
    entry: &BaselineEntry,
) -> Option<&'a BaselineEntry> {
    let name = entry.path.components().first()?;
    let path = TreePath::from_components([name.clone()]);
    entry_by_path(baseline, &path)
}

fn visible_position_by_id(
    baseline: &BaselineTree,
    context: &EditContext,
    id: EntryId,
) -> Option<usize> {
    visible_entries(baseline, context)
        .into_iter()
        .position(|entry| entry.id == id)
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
        if entry.kind == EntryKind::Dir
            && (context.collapsed_dirs.contains(&entry.id)
                || baseline.incomplete_dirs().contains_key(&entry.path))
        {
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
            let indent =
                fyler_core::grammar::INDENT_UNIT.repeat(entry.path.depth().saturating_sub(1));
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
    use std::time::Instant;

    use fyler_core::editor::{EditorSnapshot, FoldOp};
    use fyler_core::fileinfo::EntryMeta;
    use fyler_core::options::{SortKey, SortOrder};
    use fyler_core::path::TreePath;
    use fyler_core::plan::FsOperation;
    use fyler_core::report::{OpOutcome, OpResult};
    use fyler_core::tree::{BaselineEntry, EntryKind};
    use fyler_core::undo::{Fingerprint, UndoStep};
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

    fn scanned_controller(
        root: &Path,
        options: ScanOptions,
    ) -> (SaveController, Arc<RecordingEngine>) {
        let mut ids = IdAllocator::new();
        let baseline = fyler_fsops::scan::scan_baseline_with(root, &mut ids, &options).unwrap();
        let engine = Arc::new(RecordingEngine::default());
        let controller = SaveController::new_with_scan_options(
            root.to_path_buf(),
            ids,
            baseline,
            Arc::<RecordingEngine>::clone(&engine),
            options,
        );
        (controller, engine)
    }

    fn begin_rename_apply(
        controller: &mut SaveController,
    ) -> (OperationPlan, HashSet<TreePath>, Arc<AtomicBool>) {
        assert!(matches!(
            controller.on_commit(1, &lines(&["/001 b.txt"])),
            SaveFlowResult::ShowPlan { .. }
        ));
        match controller.on_choice(ConfirmChoice::Approve) {
            SaveFlowResult::StartApply {
                plan,
                overwrites,
                cancel,
            } => (plan, overwrites, cancel),
            result => panic!("StartApply was not returned after approval: {result:?}"),
        }
    }

    fn undo_transaction_for_existing_file(root: &Path, name: &str) -> UndoTransaction {
        let path = root.join(name);
        let post = fyler_fsops::identity::capture_fingerprint(&path).unwrap();
        UndoTransaction {
            id: format!("tx-{name}"),
            root: root.to_path_buf(),
            steps: vec![UndoStep::RemoveCreated {
                path,
                identity: None,
                post,
            }],
            backup_dir: None,
        }
    }

    fn rejected_undo_transaction(root: &Path) -> UndoTransaction {
        UndoTransaction {
            id: "tx-rejected".to_owned(),
            root: root.to_path_buf(),
            steps: vec![UndoStep::RemoveCreated {
                path: root.join("missing.txt"),
                identity: None,
                post: Fingerprint {
                    kind: EntryKind::File,
                    size: Some(1),
                    mtime: None,
                    link_target: None,
                },
            }],
            backup_dir: None,
        }
    }

    fn undo_report(step: UndoStep, outcome: OpOutcome) -> CommitReport<UndoStep> {
        CommitReport {
            results: vec![OpResult { op: step, outcome }],
        }
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

    fn fold_applied(result: FoldResult) -> (Vec<EditorLine>, Option<usize>) {
        match result {
            FoldResult::Applied { lines, cursor_line } => (lines, cursor_line),
            result => panic!("unexpected fold result: {result:?}"),
        }
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
                let indent =
                    fyler_core::grammar::INDENT_UNIT.repeat(entry.path.depth().saturating_sub(1));
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
    #[ignore = "environment-dependent performance measurement"]
    fn bench_git_badge_mapping_on_50k_baseline() {
        const ENTRY_COUNT: usize = 50_000;
        const ITERATIONS: usize = 20;

        let root = PathBuf::from("C:/test-root");
        let mut ids = IdAllocator::new();
        let mut baseline = BaselineTree::new(&root);
        for index in 0..ENTRY_COUNT {
            baseline.insert(BaselineEntry {
                id: ids.allocate(),
                path: TreePath::parse(&format!("file-{index:05}.txt")),
                kind: EntryKind::File,
            });
        }
        let engine = Arc::new(RecordingEngine::default());
        let controller =
            SaveController::new(root, ids, baseline, Arc::<RecordingEngine>::clone(&engine));

        for status_count in [1_usize, 100, 10_000] {
            let statuses = (0..status_count)
                .map(|index| {
                    (
                        PathBuf::from(format!("file-{index:05}.txt")),
                        GitBadge::Modified,
                    )
                })
                .collect::<HashMap<_, _>>();
            let started = Instant::now();
            for _ in 0..ITERATIONS {
                std::hint::black_box(controller.map_git_badges(&statuses));
            }
            let elapsed = started.elapsed();
            eprintln!(
                "50k git badge mapping ({status_count} statuses): {:.3} ms/iteration ({ITERATIONS} iterations)",
                elapsed.as_secs_f64() * 1_000.0 / ITERATIONS as f64,
            );
        }
    }

    #[test]
    fn change_sort_rescans_and_preserves_collapsed_dirs() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("dir")).unwrap();
        fs::write(root.path().join("dir").join("child.txt"), b"child").unwrap();
        fs::write(root.path().join("small.txt"), b"1").unwrap();
        fs::write(root.path().join("large.txt"), b"12345").unwrap();
        let options = ScanOptions {
            sort: SortOrder::Mixed,
            ..ScanOptions::default()
        };
        let (mut controller, _) = scanned_controller(root.path(), options);
        controller.collapse_all_dirs();
        let dir_id = controller
            .baseline
            .entries()
            .iter()
            .find(|entry| entry.path == TreePath::parse("dir"))
            .unwrap()
            .id;

        let lines = controller.change_sort(SortKey::Size, false).unwrap();

        assert_eq!(controller.sort_state(), (SortKey::Size, false));
        assert!(controller.collapsed_dirs().contains(&dir_id));
        assert_eq!(lines.len(), 3);
        assert!(lines[0].text.ends_with("small.txt"));
        assert!(lines[1].text.ends_with("large.txt"));
        assert!(lines[2].text.ends_with("dir/"));
    }

    #[test]
    fn change_sort_requires_idle_state() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("a.txt"), b"content").unwrap();
        let (mut controller, _) = scanned_controller(root.path(), ScanOptions::default());
        let entry_id = controller.baseline.entries()[0].id;
        let renamed_line = EditorLine::new(format!(
            "{}b.txt",
            fyler_core::grammar::format_id_prefix(entry_id)
        ));
        assert!(matches!(
            controller.on_commit(1, &[renamed_line]),
            SaveFlowResult::ShowPlan { .. }
        ));

        let error = controller.change_sort(SortKey::Date, false).unwrap_err();

        assert!(error.to_string().contains("while saving"));
        assert_eq!(controller.sort_state(), (SortKey::Name, false));
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

        let warnings = plan_warnings(
            root.path(),
            &BaselineTree::new(root.path()),
            &plan,
            &[],
            |path| Ok(path == root.path().join("cloud.bin")),
        );

        assert_eq!(
            warnings,
            ["Will download from the cloud: cloud.bin(2.0 KB)"]
        );
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

        let warnings = plan_warnings(
            root.path(),
            &BaselineTree::new(root.path()),
            &plan,
            &[],
            |_| Ok(true),
        );

        assert_eq!(warnings, ["Will download from the cloud: missing.bin"]);
    }

    #[test]
    fn plan_warnings_include_delete_backup_estimate_and_unknown_size() {
        let root = tempdir().unwrap();
        let mut baseline = BaselineTree::new(root.path());
        baseline.insert_with_meta(
            BaselineEntry {
                id: EntryId(1),
                path: TreePath::parse("old"),
                kind: EntryKind::Dir,
            },
            EntryMeta {
                size: None,
                modified: None,
                is_placeholder: false,
            },
        );
        baseline.insert_with_meta(
            BaselineEntry {
                id: EntryId(2),
                path: TreePath::parse("old/a.bin"),
                kind: EntryKind::File,
            },
            EntryMeta {
                size: Some(1024 * 1024),
                modified: None,
                is_placeholder: false,
            },
        );
        baseline.insert_with_meta(
            BaselineEntry {
                id: EntryId(3),
                path: TreePath::parse("old/b.bin"),
                kind: EntryKind::File,
            },
            EntryMeta {
                size: Some(512 * 1024),
                modified: None,
                is_placeholder: false,
            },
        );
        baseline.insert(BaselineEntry {
            id: EntryId(4),
            path: TreePath::parse("old/unknown.bin"),
            kind: EntryKind::File,
        });
        let plan = OperationPlan {
            ops: vec![FsOperation::Delete {
                id: EntryId(1),
                path: TreePath::parse("old"),
            }],
        };

        let warnings = plan_warnings(root.path(), &baseline, &plan, &[], |_| Ok(false));

        assert_eq!(
            warnings,
            ["A backup will be created before deletion: approximately 1.5 MB (some sizes unknown)"]
        );
    }

    #[test]
    fn plan_warnings_include_overwrite_backup_estimate() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("existing.bin"), vec![0_u8; 2048]).unwrap();
        let plan = OperationPlan::default();

        let warnings = plan_warnings(
            root.path(),
            &BaselineTree::new(root.path()),
            &plan,
            &[TreePath::parse("existing.bin")],
            |_| Ok(false),
        );

        assert_eq!(
            warnings,
            ["A backup will be created before overwrite: approximately 2.0 KB"]
        );
    }

    #[test]
    fn plan_warnings_include_placeholder_backup_warnings() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("overwrite.bin"), vec![0_u8; 1024]).unwrap();
        let mut baseline = BaselineTree::new(root.path());
        baseline.insert_with_meta(
            BaselineEntry {
                id: EntryId(1),
                path: TreePath::parse("cloud.bin"),
                kind: EntryKind::File,
            },
            EntryMeta {
                size: Some(4096),
                modified: None,
                is_placeholder: true,
            },
        );
        let plan = OperationPlan {
            ops: vec![FsOperation::Delete {
                id: EntryId(1),
                path: TreePath::parse("cloud.bin"),
            }],
        };

        let warnings = plan_warnings(
            root.path(),
            &baseline,
            &plan,
            &[TreePath::parse("overwrite.bin")],
            |path| Ok(path == root.path().join("overwrite.bin")),
        );

        assert_eq!(
            warnings,
            [
                "Will download from the cloud: cloud.bin(4.0 KB)",
                "A backup will be created before deletion: approximately 4.0 KB",
                "Will download from the cloud: overwrite.bin(1.0 KB)",
                "A backup will be created before overwrite: approximately 1.0 KB",
            ]
        );
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

        let start = controller.on_choice(ConfirmChoice::Approve);
        let SaveFlowResult::StartApply {
            plan,
            overwrites,
            cancel,
        } = start
        else {
            panic!("StartApply was not returned after approval");
        };

        assert!(matches!(controller.state(), SaveState::Applying { .. }));
        assert!(!cancel.load(Ordering::Relaxed));
        assert!(temp_root.path().join("a.txt").exists());
        assert!(!temp_root.path().join("b.txt").exists());
        let report =
            fyler_fsops::apply::apply_plan_with_overwrites(temp_root.path(), &plan, &overwrites);
        let result = controller.on_apply_finished(report);
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

        let start = controller.on_choice(ConfirmChoice::Approve);
        let SaveFlowResult::StartApply {
            plan,
            overwrites,
            cancel,
        } = start
        else {
            panic!("StartApply was not returned after approval");
        };

        assert!(matches!(controller.state(), SaveState::Applying { .. }));
        assert!(!cancel.load(Ordering::Relaxed));
        assert_eq!(
            fs::read(root.path().join("visible.txt")).unwrap(),
            b"source"
        );
        assert_eq!(fs::read(root.path().join(".hidden")).unwrap(), b"existing");
        let report =
            fyler_fsops::apply::apply_plan_with_overwrites(root.path(), &plan, &overwrites);
        let apply_result = controller.on_apply_finished(report);
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

        let start = controller.on_choice(ConfirmChoice::Approve);
        let SaveFlowResult::StartApply {
            plan,
            overwrites,
            cancel,
        } = start
        else {
            panic!("StartApply was not returned after approval");
        };

        assert!(matches!(controller.state(), SaveState::Applying { .. }));
        assert!(!cancel.load(Ordering::Relaxed));
        assert!(!temp_root.path().join("b.txt").exists());
        let report =
            fyler_fsops::apply::apply_plan_with_overwrites(temp_root.path(), &plan, &overwrites);
        let result = controller.on_apply_finished(report);
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
    fn all_skipped_apply_returns_idle_without_reconciling_baseline() {
        let temp_root = tempdir().unwrap();
        fs::write(temp_root.path().join("a.txt"), b"content").unwrap();
        let (mut controller, engine) = controller(temp_root.path());
        let (plan, _, _) = begin_rename_apply(&mut controller);
        let report = CommitReport {
            results: plan
                .ops
                .into_iter()
                .map(|op| OpResult {
                    op,
                    outcome: OpOutcome::Skipped {
                        reason: "Cancelled by user".to_owned(),
                    },
                })
                .collect(),
        };

        let result = controller.on_apply_finished(report);

        assert!(matches!(
            result,
            SaveFlowResult::ShowReport(ref report) if report.all_failed()
        ));
        assert!(matches!(controller.state(), SaveState::Idle));
        assert_eq!(
            controller.baseline.entries()[0].path,
            TreePath::parse("a.txt")
        );
        assert!(temp_root.path().join("a.txt").is_file());
        assert!(!temp_root.path().join("b.txt").exists());
        assert_eq!(modifiable_values(&engine), [false, true]);
    }

    #[test]
    fn cancel_while_applying_sets_worker_flag_and_keeps_applying() {
        let temp_root = tempdir().unwrap();
        fs::write(temp_root.path().join("a.txt"), b"content").unwrap();
        let (mut controller, _) = controller(temp_root.path());
        let (_, _, cancel) = begin_rename_apply(&mut controller);

        let result = controller.on_choice(ConfirmChoice::Cancel);

        assert_eq!(result, SaveFlowResult::ApplyCancelRequested);
        assert!(cancel.load(Ordering::Relaxed));
        assert!(controller.is_applying());
    }

    #[test]
    fn approve_while_applying_is_ignored() {
        let temp_root = tempdir().unwrap();
        fs::write(temp_root.path().join("a.txt"), b"content").unwrap();
        let (mut controller, _) = controller(temp_root.path());
        let (_, _, cancel) = begin_rename_apply(&mut controller);

        let result = controller.on_choice(ConfirmChoice::Approve);

        assert_eq!(result, SaveFlowResult::Ignored);
        assert!(!cancel.load(Ordering::Relaxed));
        assert!(controller.is_applying());
    }

    #[test]
    fn request_undo_ready_transaction_returns_plan_and_locks_buffer() {
        let temp_root = tempdir().unwrap();
        fs::write(temp_root.path().join("created.txt"), b"content").unwrap();
        let (mut controller, engine) = controller(temp_root.path());
        let transaction = undo_transaction_for_existing_file(temp_root.path(), "created.txt");

        let result = controller.request_undo(transaction.clone());

        assert!(matches!(
            result,
            SaveFlowResult::ShowUndoPlan {
                transaction: actual,
                statuses
            } if actual == transaction
                && statuses == vec![UndoStepStatus::Ready]
        ));
        assert!(matches!(
            controller.state(),
            SaveState::AwaitingUndoConfirmation { .. }
        ));
        assert_eq!(modifiable_values(&engine), [false]);
    }

    #[test]
    fn request_undo_all_rejected_returns_nothing_left_without_state_change() {
        let temp_root = tempdir().unwrap();
        let (mut controller, engine) = controller(temp_root.path());

        let result = controller.request_undo(rejected_undo_transaction(temp_root.path()));

        assert!(matches!(
            result,
            SaveFlowResult::UndoNothingLeft { reasons } if !reasons.is_empty()
        ));
        assert!(matches!(controller.state(), SaveState::Idle));
        assert!(engine.commands.lock().unwrap().is_empty());
    }

    #[test]
    fn request_undo_while_not_idle_is_ignored() {
        let temp_root = tempdir().unwrap();
        fs::write(temp_root.path().join("created.txt"), b"content").unwrap();
        let (mut controller, _) = controller(temp_root.path());
        assert!(matches!(
            controller.on_commit(1, &lines(&["/001 b.txt"])),
            SaveFlowResult::ShowPlan { .. }
        ));
        let transaction = undo_transaction_for_existing_file(temp_root.path(), "created.txt");

        let result = controller.request_undo(transaction);

        assert_eq!(result, SaveFlowResult::Ignored);
        assert!(matches!(
            controller.state(),
            SaveState::AwaitingConfirmation { .. }
        ));
    }

    #[test]
    fn approving_undo_starts_worker_and_enters_applying_undo() {
        let temp_root = tempdir().unwrap();
        fs::write(temp_root.path().join("created.txt"), b"content").unwrap();
        let (mut controller, _) = controller(temp_root.path());
        let transaction = undo_transaction_for_existing_file(temp_root.path(), "created.txt");
        assert!(matches!(
            controller.request_undo(transaction.clone()),
            SaveFlowResult::ShowUndoPlan { .. }
        ));

        let result = controller.on_choice(ConfirmChoice::Approve);

        let SaveFlowResult::StartUndo {
            transaction: actual,
            cancel,
        } = result
        else {
            panic!("StartUndo was not returned after undo approval: {result:?}");
        };
        assert_eq!(actual, transaction);
        assert!(!cancel.load(Ordering::Relaxed));
        assert!(matches!(controller.state(), SaveState::ApplyingUndo { .. }));
    }

    #[test]
    fn canceling_undo_returns_transaction_and_unlocks_buffer() {
        let temp_root = tempdir().unwrap();
        fs::write(temp_root.path().join("created.txt"), b"content").unwrap();
        let (mut controller, engine) = controller(temp_root.path());
        let transaction = undo_transaction_for_existing_file(temp_root.path(), "created.txt");
        assert!(matches!(
            controller.request_undo(transaction.clone()),
            SaveFlowResult::ShowUndoPlan { .. }
        ));

        let result = controller.on_choice(ConfirmChoice::Cancel);

        assert_eq!(
            result,
            SaveFlowResult::UndoCancelled {
                transaction: transaction.clone()
            }
        );
        assert!(matches!(controller.state(), SaveState::Idle));
        assert_eq!(modifiable_values(&engine), [false, true]);
    }

    #[test]
    fn cancel_while_applying_undo_sets_worker_flag_and_keeps_applying() {
        let temp_root = tempdir().unwrap();
        fs::write(temp_root.path().join("created.txt"), b"content").unwrap();
        let (mut controller, _) = controller(temp_root.path());
        let transaction = undo_transaction_for_existing_file(temp_root.path(), "created.txt");
        assert!(matches!(
            controller.request_undo(transaction),
            SaveFlowResult::ShowUndoPlan { .. }
        ));
        let SaveFlowResult::StartUndo { cancel, .. } = controller.on_choice(ConfirmChoice::Approve)
        else {
            panic!("StartUndo was not returned after undo approval");
        };

        let result = controller.on_choice(ConfirmChoice::Cancel);

        assert_eq!(result, SaveFlowResult::ApplyCancelRequested);
        assert!(cancel.load(Ordering::Relaxed));
        assert!(matches!(controller.state(), SaveState::ApplyingUndo { .. }));
    }

    #[test]
    fn undo_finished_all_failed_reports_without_reconcile() {
        let temp_root = tempdir().unwrap();
        fs::write(temp_root.path().join("created.txt"), b"content").unwrap();
        let (mut controller, engine) = controller(temp_root.path());
        let transaction = undo_transaction_for_existing_file(temp_root.path(), "created.txt");
        let step = transaction.steps[0].clone();
        assert!(matches!(
            controller.request_undo(transaction),
            SaveFlowResult::ShowUndoPlan { .. }
        ));
        assert!(matches!(
            controller.on_choice(ConfirmChoice::Approve),
            SaveFlowResult::StartUndo { .. }
        ));
        let report = undo_report(
            step,
            OpOutcome::Failed {
                error: "stale".to_owned(),
                progress: None,
            },
        );

        let result = controller.on_undo_finished(report.clone());

        assert_eq!(result, SaveFlowResult::ShowUndoReport(report));
        assert!(matches!(controller.state(), SaveState::Idle));
        assert!(
            engine
                .commands
                .lock()
                .unwrap()
                .iter()
                .all(|command| matches!(command, EditorCommand::SetModifiable(_)))
        );
    }

    #[test]
    fn undo_finished_partial_success_reconciles_from_filesystem() {
        let temp_root = tempdir().unwrap();
        fs::write(temp_root.path().join("created.txt"), b"content").unwrap();
        let (mut controller, engine) = controller(temp_root.path());
        let transaction = undo_transaction_for_existing_file(temp_root.path(), "created.txt");
        let step = transaction.steps[0].clone();
        assert!(matches!(
            controller.request_undo(transaction),
            SaveFlowResult::ShowUndoPlan { .. }
        ));
        assert!(matches!(
            controller.on_choice(ConfirmChoice::Approve),
            SaveFlowResult::StartUndo { .. }
        ));
        let report = undo_report(step, OpOutcome::Success);

        let result = controller.on_undo_finished(report.clone());

        assert_eq!(result, SaveFlowResult::ShowUndoReport(report));
        assert!(matches!(controller.state(), SaveState::Idle));
        assert!(engine.commands.lock().unwrap().iter().any(|command| {
            matches!(command, EditorCommand::SetLines { lines, .. }
                if lines.iter().any(|line| line.text.ends_with("created.txt")))
        }));
    }

    #[test]
    fn external_change_during_undo_confirmation_invalidates_and_returns_transaction() {
        let temp_root = tempdir().unwrap();
        fs::write(temp_root.path().join("created.txt"), b"content").unwrap();
        let (mut controller, engine) = controller(temp_root.path());
        let transaction = undo_transaction_for_existing_file(temp_root.path(), "created.txt");
        assert!(matches!(
            controller.request_undo(transaction.clone()),
            SaveFlowResult::ShowUndoPlan { .. }
        ));
        fs::write(temp_root.path().join("external.txt"), b"external").unwrap();

        let result = controller.on_external_change(&BTreeSet::new());

        assert!(matches!(
            result,
            SaveFlowResult::UndoInvalidated {
                transaction: actual,
                message
            } if actual == transaction && message.contains("Undo was cancelled")
        ));
        assert!(matches!(controller.state(), SaveState::Idle));
        assert_eq!(modifiable_values(&engine), [false, true]);
    }

    #[test]
    fn undo_flow_sends_only_modifiable_and_set_lines_commands_to_engine() {
        let temp_root = tempdir().unwrap();
        fs::write(temp_root.path().join("created.txt"), b"content").unwrap();
        let (mut controller, engine) = controller(temp_root.path());
        let transaction = undo_transaction_for_existing_file(temp_root.path(), "created.txt");
        let step = transaction.steps[0].clone();
        assert!(matches!(
            controller.request_undo(transaction),
            SaveFlowResult::ShowUndoPlan { .. }
        ));
        assert!(matches!(
            controller.on_choice(ConfirmChoice::Approve),
            SaveFlowResult::StartUndo { .. }
        ));
        let report = undo_report(step, OpOutcome::Success);

        let result = controller.on_undo_finished(report);

        assert!(matches!(result, SaveFlowResult::ShowUndoReport(_)));
        assert!(engine.commands.lock().unwrap().iter().all(|command| {
            matches!(
                command,
                EditorCommand::SetModifiable(_) | EditorCommand::SetLines { .. }
            )
        }));
    }

    #[test]
    fn commit_while_applying_is_ignored() {
        let temp_root = tempdir().unwrap();
        fs::write(temp_root.path().join("a.txt"), b"content").unwrap();
        let (mut controller, _) = controller(temp_root.path());
        begin_rename_apply(&mut controller);

        let result = controller.on_commit(2, &lines(&["/001 c.txt"]));

        assert_eq!(result, SaveFlowResult::Ignored);
        assert!(controller.is_applying());
        assert!(temp_root.path().join("a.txt").is_file());
        assert!(!temp_root.path().join("b.txt").exists());
        assert!(!temp_root.path().join("c.txt").exists());
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
    fn missing_root_goes_offline_preserves_view_and_reconnects() {
        let parent = tempdir().unwrap();
        let root = parent.path().join("root");
        let parked = parent.path().join("parked");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a.txt"), b"a").unwrap();
        let (mut controller, engine) = scanned_controller(&root, ScanOptions::default());
        let before_entries = controller.baseline.entries().to_vec();
        let before_lines = controller.visible_lines();
        fs::rename(&root, &parked).unwrap();

        let result = controller.on_external_change(&BTreeSet::new());

        assert!(matches!(result, SaveFlowResult::WentOffline(_)));
        assert!(controller.is_offline());
        assert_eq!(controller.baseline.entries(), before_entries);
        assert_eq!(controller.visible_lines(), before_lines);
        assert_eq!(modifiable_values(&engine), [false]);
        assert!(matches!(
            controller.on_commit(1, &before_lines),
            SaveFlowResult::OfflineRejected(message) if message.contains("offline or unreachable")
        ));

        fs::rename(&parked, &root).unwrap();
        assert!(matches!(
            controller.retry_offline(),
            SaveFlowResult::Reconnected(message) if message.contains(root.to_string_lossy().as_ref())
        ));
        assert!(!controller.is_offline());
        let commands = engine.commands.lock().unwrap();
        assert!(
            commands
                .iter()
                .any(|command| matches!(command, EditorCommand::SetLines { .. }))
        );
        assert!(matches!(
            commands.last(),
            Some(EditorCommand::SetModifiable(true))
        ));
    }

    #[test]
    fn dirty_reconnect_updates_baseline_without_replacing_buffer() {
        let parent = tempdir().unwrap();
        let root = parent.path().join("root");
        let parked = parent.path().join("parked");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a.txt"), b"a").unwrap();
        let (mut controller, engine) = scanned_controller(&root, ScanOptions::default());
        fs::rename(&root, &parked).unwrap();
        assert!(matches!(
            controller.on_external_change(&BTreeSet::new()),
            SaveFlowResult::WentOffline(_)
        ));
        engine.commands.lock().unwrap().clear();
        engine.set_dirty(true);
        fs::write(parked.join("b.txt"), b"b").unwrap();
        fs::rename(&parked, &root).unwrap();

        assert!(matches!(
            controller.retry_offline(),
            SaveFlowResult::Reconnected(_)
        ));
        assert!(
            controller
                .baseline
                .entries()
                .iter()
                .any(|entry| entry.path == TreePath::parse("b.txt"))
        );
        assert!(
            engine
                .commands
                .lock()
                .unwrap()
                .iter()
                .all(|command| !matches!(command, EditorCommand::SetLines { .. }))
        );
    }

    #[test]
    fn scan_health_message_is_aggregated_until_incomplete_set_changes() {
        let (mut controller, _) = controller("C:/test-root");
        controller.baseline.insert(BaselineEntry {
            id: EntryId(2),
            path: TreePath::parse("blocked"),
            kind: EntryKind::Dir,
        });
        controller.baseline.mark_incomplete(
            TreePath::parse("blocked"),
            fyler_core::scanwarn::ScanErrorKind::PermissionDenied,
        );

        let first = controller.take_scan_health_message().unwrap();
        assert_eq!(first.0, fyler_core::editor::MessageKind::Warn);
        assert!(first.1.contains("1 location could not be fully read"));
        assert!(controller.take_scan_health_message().is_none());

        let mut recovered = BaselineTree::new(controller.root.clone());
        recovered.insert(BaselineEntry {
            id: EntryId(1),
            path: TreePath::parse("a.txt"),
            kind: EntryKind::File,
        });
        controller.baseline = recovered;
        assert_eq!(
            controller.take_scan_health_message(),
            Some((
                fyler_core::editor::MessageKind::Info,
                "All locations are readable again".to_owned()
            ))
        );
        assert!(controller.take_scan_health_message().is_none());
    }

    #[test]
    fn incomplete_probe_pauses_while_dirty() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("blocked")).unwrap();
        let mut ids = IdAllocator::new();
        let mut baseline = fyler_fsops::scan::scan_baseline(root.path(), &mut ids).unwrap();
        baseline.mark_incomplete(
            TreePath::parse("blocked"),
            fyler_core::scanwarn::ScanErrorKind::PermissionDenied,
        );
        let engine = Arc::new(RecordingEngine::default());
        let controller = SaveController::new(
            root.path().to_path_buf(),
            ids,
            baseline,
            Arc::<RecordingEngine>::clone(&engine),
        );

        assert_eq!(
            controller.incomplete_probe_paths(),
            Some(BTreeSet::from([root.path().join("blocked")]))
        );
        engine.set_dirty(true);
        assert!(controller.incomplete_probe_paths().is_none());
    }

    #[test]
    fn incomplete_probe_reports_recovery_once() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("blocked")).unwrap();
        fs::write(root.path().join("blocked/kept.txt"), b"kept").unwrap();
        let mut ids = IdAllocator::new();
        let mut baseline = fyler_fsops::scan::scan_baseline(root.path(), &mut ids).unwrap();
        baseline.mark_incomplete(
            TreePath::parse("blocked"),
            fyler_core::scanwarn::ScanErrorKind::PermissionDenied,
        );
        let engine = Arc::new(RecordingEngine::default());
        let mut controller = SaveController::new(
            root.path().to_path_buf(),
            ids,
            baseline,
            Arc::<RecordingEngine>::clone(&engine),
        );
        assert_eq!(
            controller.take_scan_health_message().unwrap().0,
            fyler_core::editor::MessageKind::Warn
        );

        let paths = controller.incomplete_probe_paths().unwrap();
        assert_eq!(
            controller.on_external_change(&paths),
            SaveFlowResult::NoChanges
        );
        assert_eq!(
            controller.take_scan_health_message(),
            Some((
                fyler_core::editor::MessageKind::Info,
                "All locations are readable again".to_owned()
            ))
        );
        assert!(controller.take_scan_health_message().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn incomplete_probe_is_silent_while_directory_remains_incomplete() {
        use std::os::unix::ffi::OsStringExt;

        let root = tempdir().unwrap();
        fs::write(
            root.path()
                .join(std::ffi::OsString::from_vec(vec![b'x', 0xff])),
            b"unrepresentable",
        )
        .unwrap();
        let (mut controller, engine) = scanned_controller(root.path(), ScanOptions::default());
        assert_eq!(
            controller.take_scan_health_message().unwrap().0,
            fyler_core::editor::MessageKind::Warn
        );
        engine.commands.lock().unwrap().clear();

        let paths = controller.incomplete_probe_paths().unwrap();
        assert_eq!(
            controller.on_external_change(&paths),
            SaveFlowResult::NoChanges
        );
        assert!(controller.take_scan_health_message().is_none());
        assert!(engine.commands.lock().unwrap().is_empty());
    }

    #[test]
    fn changing_root_clears_offline_state_and_restores_editing() {
        let (mut controller, engine) = controller("C:/old-root");
        controller.offline = Some("unreachable".to_owned());
        let new_root = PathBuf::from("C:/new-root");
        let new_baseline = BaselineTree::new(new_root.clone());

        controller
            .change_root_preserving_allocator(new_root, new_baseline)
            .unwrap();

        assert!(!controller.is_offline());
        assert_eq!(modifiable_values(&engine), [true]);
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
                if message.contains("Files changed externally")
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
    fn external_mtime_change_resorts_date_order_and_sends_lines() {
        // WindowsのSetFileTimeは書き込みアクセスのハンドルを要求するため、
        // File::open(読み取り専用)ではAccess deniedになる。
        fn set_mtime(path: &Path, secs: u64) {
            fs::OpenOptions::new()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                .unwrap();
        }

        let temp_root = tempdir().unwrap();
        let first = temp_root.path().join("a.txt");
        let second = temp_root.path().join("b.txt");
        fs::write(&first, b"a").unwrap();
        fs::write(&second, b"b").unwrap();
        set_mtime(&first, 10);
        set_mtime(&second, 20);
        let options = ScanOptions {
            key: SortKey::Date,
            sort: SortOrder::Mixed,
            ..ScanOptions::default()
        };
        let (mut controller, engine) = scanned_controller(temp_root.path(), options);
        set_mtime(&first, 30);

        let result = controller.on_external_change(&BTreeSet::from([first]));

        assert_eq!(result, SaveFlowResult::ExternalChanged);
        let commands = engine.commands.lock().unwrap();
        assert!(matches!(
            commands.as_slice(),
            [EditorCommand::SetLines { lines, .. }]
                if lines.len() == 2
                    && lines[0].text.ends_with("b.txt")
                    && lines[1].text.ends_with("a.txt")
        ));
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
                if message.contains("Save was cancelled because files changed externally")
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
        let start = controller.on_choice(ConfirmChoice::Approve);
        let SaveFlowResult::StartApply {
            plan,
            overwrites,
            cancel,
        } = start
        else {
            panic!("StartApply was not returned after approval");
        };
        assert!(matches!(controller.state(), SaveState::Applying { .. }));
        assert!(!cancel.load(Ordering::Relaxed));
        assert!(temp_root.path().join("a.txt").exists());
        assert!(!temp_root.path().join("b.txt").exists());
        let report =
            fyler_fsops::apply::apply_plan_with_overwrites(temp_root.path(), &plan, &overwrites);
        assert!(matches!(
            controller.on_apply_finished(report),
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
        let (new_baseline, _) = baseline(&new_root);

        controller
            .change_root_preserving_allocator(new_root.clone(), new_baseline)
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
        let (rejected_baseline, _) = baseline(&rejected_root);

        assert!(
            controller
                .change_root_preserving_allocator(rejected_root, rejected_baseline)
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
    fn shallow_baseline_starts_unloaded_directories_collapsed() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("lazy")).unwrap();
        fs::write(root.path().join("lazy").join("child.txt"), b"child").unwrap();
        let mut ids = IdAllocator::new();
        let baseline = fyler_fsops::scan::scan_baseline_shallow_with(
            root.path(),
            &mut ids,
            &ScanOptions::default(),
        )
        .unwrap();
        let lazy_id = baseline.get_by_path(&TreePath::parse("lazy")).unwrap().id;
        let controller = SaveController::new(
            root.path().to_path_buf(),
            ids,
            baseline,
            Arc::new(RecordingEngine::default()),
        );

        assert!(controller.collapsed_dirs().contains(&lazy_id));
        assert_eq!(controller.visible_lines().len(), 1);
    }

    #[test]
    fn unloaded_toggle_and_fold_request_load_without_mutating_folds() {
        let (mut controller, _) = hierarchy_controller("C:/test-root");
        let dir = TreePath::parse("a");
        controller.baseline.mark_unloaded(dir.clone());
        let id = controller.baseline.get_by_path(&dir).unwrap().id;
        controller.context.collapsed_dirs.insert(id);
        let visible = controller.visible_lines();

        assert_eq!(
            controller.toggle_collapse(&visible, 0),
            ToggleCollapseResult::NeedsLoad { dir: dir.clone() }
        );
        assert_eq!(
            controller.fold(&visible, 0, FoldOp::Open),
            FoldResult::NeedsLoad { dir: dir.clone() }
        );
        assert_eq!(
            controller.fold(&visible, 0, FoldOp::OpenRecursive),
            FoldResult::NeedsLoadRecursive { dirs: vec![dir] }
        );
        assert!(controller.collapsed_dirs().contains(&id));
    }

    #[test]
    fn incomplete_rejection_remains_unchanged_after_adding_unloaded_results() {
        let (mut controller, _) = hierarchy_controller("C:/test-root");
        let dir = TreePath::parse("a");
        controller
            .baseline
            .mark_incomplete(dir, fyler_core::scanwarn::ScanErrorKind::PermissionDenied);
        controller.collapse_all_dirs();
        let visible = controller.visible_lines();

        assert_eq!(
            controller.toggle_collapse(&visible, 0),
            ToggleCollapseResult::CannotExpandIncomplete
        );
        assert_eq!(
            controller.fold(&visible, 0, FoldOp::OpenRecursive),
            FoldResult::CannotExpandIncomplete
        );
    }

    #[test]
    fn single_directory_load_can_be_installed_and_expanded() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("lazy")).unwrap();
        fs::write(root.path().join("lazy").join("child.txt"), b"child").unwrap();
        let mut ids = IdAllocator::new();
        let baseline = fyler_fsops::scan::scan_baseline_shallow_with(
            root.path(),
            &mut ids,
            &ScanOptions::default(),
        )
        .unwrap();
        let mut controller = SaveController::new(
            root.path().to_path_buf(),
            ids,
            baseline,
            Arc::new(RecordingEngine::default()),
        );
        let visible = controller.visible_lines();
        let loaded = {
            let mut ids = controller.ids.lock().unwrap();
            fyler_fsops::scan::load_directory(
                root.path(),
                &TreePath::parse("lazy"),
                &mut ids,
                &controller.baseline,
                &ScanOptions::default(),
            )
            .unwrap()
        };
        controller.replace_baseline_after_load(loaded).unwrap();

        let ToggleCollapseResult::Toggled(lines) = controller.toggle_collapse(&visible, 0) else {
            panic!("loaded directory did not expand");
        };
        assert_eq!(lines.len(), 2);
        assert!(lines[1].text.ends_with("child.txt"));
    }

    #[test]
    fn cancelled_recursive_load_keeps_the_previous_shallow_baseline() {
        let root = tempdir().unwrap();
        fs::create_dir_all(root.path().join("lazy").join("nested")).unwrap();
        fs::write(
            root.path().join("lazy").join("nested").join("child.txt"),
            b"child",
        )
        .unwrap();
        let mut ids = IdAllocator::new();
        let baseline = fyler_fsops::scan::scan_baseline_shallow_with(
            root.path(),
            &mut ids,
            &ScanOptions::default(),
        )
        .unwrap();
        let before = baseline.clone();
        let cancel = AtomicBool::new(true);

        let loaded = fyler_fsops::scan::load_directory_recursive_cancellable(
            root.path(),
            &TreePath::parse("lazy"),
            |_| Ok(ids.allocate()),
            &baseline,
            &ScanOptions::default(),
            |_| {},
            &cancel,
        )
        .unwrap();

        assert!(loaded.is_none());
        assert_eq!(baseline.entries(), before.entries());
        assert_eq!(baseline.unloaded_dirs(), before.unloaded_dirs());
    }

    #[test]
    fn failed_single_load_leaves_directory_unloaded_for_retry() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("lazy")).unwrap();
        let mut ids = IdAllocator::new();
        let baseline = fyler_fsops::scan::scan_baseline_shallow_with(
            root.path(),
            &mut ids,
            &ScanOptions::default(),
        )
        .unwrap();
        fs::remove_dir(root.path().join("lazy")).unwrap();

        assert!(
            fyler_fsops::scan::load_directory(
                root.path(),
                &TreePath::parse("lazy"),
                &mut ids,
                &baseline,
                &ScanOptions::default(),
            )
            .is_err()
        );
        assert!(baseline.is_unloaded(&TreePath::parse("lazy")));
    }

    #[test]
    fn offline_retry_preserves_shallow_coverage_instead_of_recursively_expanding() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("root");
        fs::create_dir_all(root.join("lazy")).unwrap();
        fs::write(root.join("lazy").join("child.txt"), b"child").unwrap();
        let mut ids = IdAllocator::new();
        let baseline =
            fyler_fsops::scan::scan_baseline_shallow_with(&root, &mut ids, &ScanOptions::default())
                .unwrap();
        let mut controller = SaveController::new(
            root.clone(),
            ids,
            baseline,
            Arc::new(RecordingEngine::default()),
        );
        fs::remove_dir_all(&root).unwrap();
        assert!(matches!(
            controller.on_external_change(&BTreeSet::new()),
            SaveFlowResult::WentOffline(_)
        ));
        fs::create_dir_all(root.join("lazy")).unwrap();
        fs::write(root.join("lazy").join("new-child.txt"), b"child").unwrap();

        assert!(matches!(
            controller.retry_offline(),
            SaveFlowResult::Reconnected(_)
        ));
        assert_eq!(controller.baseline.entries().len(), 1);
        assert!(controller.baseline.is_unloaded(&TreePath::parse("lazy")));
        assert!(
            controller
                .baseline
                .get_by_path(&TreePath::parse("lazy/new-child.txt"))
                .is_none()
        );
    }

    #[test]
    fn shallow_baseline_consumers_ignore_unloaded_descendants_without_health_errors() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join("lazy")).unwrap();
        fs::write(root.path().join("lazy").join("child.txt"), b"child").unwrap();
        let mut ids = IdAllocator::new();
        let baseline = fyler_fsops::scan::scan_baseline_shallow_with(
            root.path(),
            &mut ids,
            &ScanOptions::default(),
        )
        .unwrap();
        let controller = SaveController::new(
            root.path().to_path_buf(),
            ids,
            baseline,
            Arc::new(RecordingEngine::default()),
        );
        let statuses = HashMap::from([
            (PathBuf::from("lazy"), GitBadge::Modified),
            (PathBuf::from("lazy/child.txt"), GitBadge::Added),
        ]);

        assert_eq!(controller.map_git_badges(&statuses).len(), 1);
        assert_eq!(controller.visible_file_infos().len(), 1);
        assert_eq!(controller.unreadable_count(), 0);
    }

    #[test]
    fn incomplete_directory_is_forced_closed_without_polluting_collapsed_context() {
        let (mut controller, _) = hierarchy_controller("C:/test-root");
        controller.baseline.mark_incomplete(
            TreePath::parse("a"),
            fyler_core::scanwarn::ScanErrorKind::PermissionDenied,
        );

        let visible = controller.visible_lines();

        assert_eq!(visible.len(), 2);
        assert!(visible[0].text.ends_with("a/"));
        assert!(visible[1].text.ends_with("top.txt"));
        assert!(controller.context.collapsed_dirs.is_empty());
        assert_eq!(
            controller.toggle_collapse(&visible, 0),
            ToggleCollapseResult::CannotExpandIncomplete
        );
        assert!(controller.context.collapsed_dirs.is_empty());
    }

    #[test]
    fn fold_rejects_expanding_incomplete_directory() {
        let (mut controller, _) = hierarchy_controller("C:/test-root");
        controller.baseline.mark_incomplete(
            TreePath::parse("a/nested"),
            fyler_core::scanwarn::ScanErrorKind::PermissionDenied,
        );
        controller.context.collapsed_dirs.insert(EntryId(2));
        let lines = controller.visible_lines();

        assert_eq!(
            controller.fold(&lines, 1, FoldOp::Open),
            FoldResult::CannotExpandIncomplete
        );
        assert_eq!(
            controller.fold(&lines, 0, FoldOp::OpenRecursive),
            FoldResult::CannotExpandIncomplete
        );
        assert_eq!(
            controller.fold(&lines, 0, FoldOp::OpenAll),
            FoldResult::CannotExpandIncomplete
        );
        assert!(controller.context.collapsed_dirs.contains(&EntryId(2)));
    }

    #[test]
    fn close_on_file_collapses_parent_and_moves_cursor() {
        let (mut controller, _) = hierarchy_controller("C:/test-root");
        let expanded = controller.visible_lines();

        let (collapsed, cursor_line) = fold_applied(controller.fold(&expanded, 2, FoldOp::Close));

        assert_eq!(cursor_line, Some(1));
        assert!(collapsed[1].text.ends_with("nested/"));
        assert!(
            collapsed
                .iter()
                .all(|line| !line.text.ends_with("leaf.txt"))
        );
        assert_eq!(controller.context.collapsed_dirs, [EntryId(2)].into());
    }

    #[test]
    fn close_on_expanded_dir_collapses_it() {
        let (mut controller, _) = hierarchy_controller("C:/test-root");
        let expanded = controller.visible_lines();

        let (collapsed, cursor_line) = fold_applied(controller.fold(&expanded, 0, FoldOp::Close));

        assert_eq!(cursor_line, Some(0));
        assert_eq!(collapsed.len(), 2);
        assert!(collapsed[0].text.ends_with("a/"));
        assert_eq!(controller.context.collapsed_dirs, [EntryId(1)].into());
    }

    #[test]
    fn close_on_collapsed_dir_climbs_to_parent() {
        let (mut controller, _) = hierarchy_controller("C:/test-root");
        let expanded = controller.visible_lines();
        let (nested_collapsed, _) = fold_applied(controller.fold(&expanded, 1, FoldOp::Close));

        let (parent_collapsed, cursor_line) =
            fold_applied(controller.fold(&nested_collapsed, 1, FoldOp::Close));

        assert_eq!(cursor_line, Some(0));
        assert_eq!(parent_collapsed.len(), 2);
        assert!(parent_collapsed[0].text.ends_with("a/"));
        assert_eq!(
            controller.context.collapsed_dirs,
            [EntryId(1), EntryId(2)].into()
        );
    }

    #[test]
    fn close_on_top_level_file_is_noop() {
        let (mut controller, _) = hierarchy_controller("C:/test-root");
        let expanded = controller.visible_lines();

        assert_eq!(
            controller.fold(&expanded, 4, FoldOp::Close),
            FoldResult::NoOp
        );
        assert_eq!(controller.visible_lines(), expanded);
    }

    #[test]
    fn open_on_collapsed_dir_expands() {
        let (mut controller, _) = hierarchy_controller("C:/test-root");
        let expanded = controller.visible_lines();
        let (nested_collapsed, _) = fold_applied(controller.fold(&expanded, 1, FoldOp::Close));

        let (opened, cursor_line) =
            fold_applied(controller.fold(&nested_collapsed, 1, FoldOp::Open));

        assert_eq!(cursor_line, Some(1));
        assert!(opened.iter().any(|line| line.text.ends_with("leaf.txt")));
        assert!(controller.context.collapsed_dirs.is_empty());
    }

    #[test]
    fn toggle_on_file_closes_parent() {
        let (mut controller, _) = hierarchy_controller("C:/test-root");
        let expanded = controller.visible_lines();

        let (collapsed, cursor_line) = fold_applied(controller.fold(&expanded, 3, FoldOp::Toggle));

        assert_eq!(cursor_line, Some(0));
        assert_eq!(collapsed.len(), 2);
        assert_eq!(controller.context.collapsed_dirs, [EntryId(1)].into());
    }

    #[test]
    fn close_recursive_collapses_descendant_dirs() {
        let (mut controller, _) = hierarchy_controller("C:/test-root");
        let expanded = controller.visible_lines();

        let (collapsed, cursor_line) =
            fold_applied(controller.fold(&expanded, 0, FoldOp::CloseRecursive));

        assert_eq!(cursor_line, Some(0));
        assert_eq!(collapsed.len(), 2);
        assert_eq!(
            controller.context.collapsed_dirs,
            [EntryId(1), EntryId(2)].into()
        );
    }

    #[test]
    fn reveal_entry_returns_the_correct_line_when_already_visible() {
        let (mut controller, _) = hierarchy_controller("C:/test-root");

        assert_eq!(
            controller.reveal_entry(EntryId(3)),
            RevealResult::AlreadyVisible { line: 2 }
        );
    }

    #[test]
    fn open_recursive_expands_descendant_dirs() {
        let (mut controller, _) = hierarchy_controller("C:/test-root");
        controller.collapse_all_dirs();
        let collapsed = controller.visible_lines();

        let (opened, cursor_line) =
            fold_applied(controller.fold(&collapsed, 0, FoldOp::OpenRecursive));

        assert_eq!(cursor_line, Some(0));
        assert!(opened.iter().any(|line| line.text.ends_with("leaf.txt")));
        assert!(controller.context.collapsed_dirs.is_empty());
    }

    #[test]
    fn close_all_moves_cursor_to_top_level_ancestor() {
        let (mut controller, _) = hierarchy_controller("C:/test-root");
        let expanded = controller.visible_lines();

        let (collapsed, cursor_line) =
            fold_applied(controller.fold(&expanded, 2, FoldOp::CloseAll));

        assert_eq!(cursor_line, Some(0));
        assert_eq!(collapsed.len(), 2);
        assert_eq!(
            controller.context.collapsed_dirs,
            [EntryId(1), EntryId(2)].into()
        );
    }

    #[test]
    fn open_all_clears_and_keeps_cursor_entry() {
        let (mut controller, _) = hierarchy_controller("C:/test-root");
        let expanded = controller.visible_lines();
        let (nested_collapsed, _) = fold_applied(controller.fold(&expanded, 1, FoldOp::Close));

        let (opened, cursor_line) =
            fold_applied(controller.fold(&nested_collapsed, 1, FoldOp::OpenAll));

        assert_eq!(cursor_line, Some(1));
        assert_eq!(opened, expanded);
        assert!(controller.context.collapsed_dirs.is_empty());
    }

    #[test]
    fn fold_busy_when_not_idle() {
        let (mut controller, _) = hierarchy_controller("C:/test-root");
        let mut lines = controller.visible_lines();
        lines[4].text = lines[4].text.replace("top.txt", "renamed.txt").into();
        assert!(matches!(
            controller.on_commit(7, &lines),
            SaveFlowResult::ShowPlan { .. }
        ));

        assert_eq!(controller.fold(&lines, 0, FoldOp::Close), FoldResult::Busy);
    }

    #[test]
    fn fold_not_found_for_no_id_line() {
        let (mut controller, _) = hierarchy_controller("C:/test-root");

        assert_eq!(
            controller.fold(&lines(&["new.txt"]), 0, FoldOp::Close),
            FoldResult::NotFound
        );
    }

    #[test]
    fn reveal_entry_expands_one_collapsed_ancestor() {
        let (mut controller, _) = hierarchy_controller("C:/test-root");
        let expanded = controller.visible_lines();
        assert!(matches!(
            controller.toggle_collapse(&expanded, 0),
            ToggleCollapseResult::Toggled(_)
        ));

        let RevealResult::Revealed { lines, line } = controller.reveal_entry(EntryId(4)) else {
            panic!("collapsed child was not revealed");
        };

        assert_eq!(line, 3);
        assert_eq!(lines, controller.visible_lines());
        assert!(matches!(
            fyler_core::grammar::split_id_prefix(&lines[line].text),
            PrefixParse::WithId { id: EntryId(4), .. }
        ));
    }

    #[test]
    fn reveal_entry_expands_every_collapsed_ancestor() {
        let (mut controller, _) = hierarchy_controller("C:/test-root");
        controller.collapse_all_dirs();

        let RevealResult::Revealed { lines, line } = controller.reveal_entry(EntryId(3)) else {
            panic!("deeply collapsed entry was not revealed");
        };

        assert_eq!(line, 2);
        assert_eq!(lines, controller.visible_lines());
        assert!(!controller.context.collapsed_dirs.contains(&EntryId(1)));
        assert!(!controller.context.collapsed_dirs.contains(&EntryId(2)));
        assert!(matches!(
            fyler_core::grammar::split_id_prefix(&lines[line].text),
            PrefixParse::WithId { id: EntryId(3), .. }
        ));
    }

    #[test]
    fn reveal_entry_returns_not_found_without_changing_collapsed_state() {
        let (mut controller, _) = hierarchy_controller("C:/test-root");
        controller.collapse_all_dirs();
        let collapsed = controller.collapsed_dirs();

        assert_eq!(
            controller.reveal_entry(EntryId(999)),
            RevealResult::NotFound
        );
        assert_eq!(controller.collapsed_dirs(), collapsed);
    }

    #[test]
    fn reveal_entry_returns_busy_during_save_flow() {
        let (mut controller, _) = hierarchy_controller("C:/test-root");
        controller.collapse_all_dirs();
        let mut edited = controller.visible_lines();
        edited[1].text = edited[1].text.replace("top.txt", "renamed.txt").into();
        assert!(matches!(
            controller.on_commit(7, &edited),
            SaveFlowResult::ShowPlan { .. }
        ));
        let collapsed = controller.collapsed_dirs();

        assert_eq!(controller.reveal_entry(EntryId(3)), RevealResult::Busy);
        assert_eq!(controller.collapsed_dirs(), collapsed);
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
        lines[4].text = lines[4].text.replace("top.txt", "renamed.txt").into();
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
        collapsed[0].text = collapsed[0].text.replace("a/", "renamed/").into();

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

    #[test]
    fn new_controller_forces_unloaded_directories_into_collapsed_context() {
        let root = tempdir().unwrap();
        let mut baseline = BaselineTree::new(root.path());
        baseline.insert(BaselineEntry {
            id: EntryId(7),
            path: TreePath::parse("lazy"),
            kind: EntryKind::Dir,
        });
        baseline.mark_unloaded(TreePath::parse("lazy"));

        let controller = SaveController::new(
            root.path().to_path_buf(),
            IdAllocator::new(),
            baseline,
            Arc::new(RecordingEngine::default()),
        );

        assert_eq!(controller.collapsed_dirs(), [EntryId(7)].into());
    }
}
