//! eframeアプリ本体。毎フレーム、エンジンのsnapshotだけを描画する。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use eframe::egui;
use fyler_core::WindowGeometry;
use fyler_core::editor::{
    CmdlineState, EditorEngine, EditorEvent, EditorMessage, Key, KeyInput, Mode, Modifiers,
    PopupmenuState,
};
use fyler_core::feedback::{FeedbackKind, MAX_BODY_CHARS, validate_body};
use fyler_core::fileinfo::FileInfo;
use fyler_core::gitstatus::GitBadge;
use fyler_core::id::EntryId;
use fyler_core::keymap::{HelpEntry, KeySequence};
use fyler_core::options::StatusItem;
use fyler_core::pane::{PaneId, PaneLayout, SplitDirection};
use fyler_core::path::TreePath;
use fyler_core::plan::OperationPlan;
use fyler_core::report::{ApplyProgress, CommitReport};
use fyler_core::transfer::{DropEffect, ImportOp, ImportPlan, TransferOp, TransferPlan};
use fyler_core::validate::ValidateError;

use crate::confirm::{ConfirmChoice, ConfirmDetail};
use crate::{chrome, cmdline, confirm, icon, input, modeline, theme, tree_view};

const BUILTIN_FONT_NAME: &str = "fyler-builtin";
const INITIAL_WINDOW_SCALE: f32 = 0.7;
/// ファイルpickerで候補を確定したときの動作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerAction {
    /// 対象をツリー上へ表示し、カーソルを移動する。
    Jump,
    /// OSの既定アプリケーションで対象を開く。
    Open,
}

/// app側検索workerが返す、GUI表示に必要な情報だけを持つpicker結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerHit {
    pub path: TreePath,
    pub display: String,
    pub kind: fyler_core::tree::EntryKind,
}

/// ツリー行クリックの種別(app層へ伝える語彙。エンジン非依存)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeRowClickKind {
    /// 単純click(right-clickによる行選択もこれとして扱う)。
    Single,
    /// double-click(directory展開/折りたたみ、file/symlink open)。
    Double,
    /// Shift押下中のclick(anchorからlinewise選択)。
    Shift,
}

/// ツリーのcontext menu項目。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeContextItem {
    /// Open / Enter directory。
    Open,
    OpenWith,
    /// buffer上で名前編集を開始するだけ(実FSは変更しない)。
    Rename,
    /// bufferから行を除去するだけ(実FSは変更しない)。
    MarkForDeletion,
    CopyPath,
    OpenTerminal,
}

/// GUIからapp層へ返すユーザー操作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuiAction {
    Confirm(ConfirmChoice),
    Editor {
        pane_id: PaneId,
        event: EditorEvent,
    },
    LoaderCancel,
    PickerSelect {
        pane_id: PaneId,
        path: TreePath,
        action: PickerAction,
    },
    PickerQuery {
        pane_id: PaneId,
        query: String,
    },
    PickerClosed {
        pane_id: PaneId,
    },
    FeedbackSubmit {
        kind: FeedbackKind,
        body: String,
    },
    FeedbackClosed,
    FilesDropped {
        pane_id: PaneId,
        line: Option<usize>,
        paths: Vec<PathBuf>,
        effect: DropEffect,
    },
    /// ユーザーがpaneのツリー領域(行または空白部分)をクリックしてfocusを
    /// 要求した。pane_runtimeが`active`/`last_active`を更新する。
    RequestPaneFocus {
        pane_id: PaneId,
    },
    /// ツリー行のクリック(single/double/shift)をapp層へ伝える。
    TreeRowClicked {
        pane_id: PaneId,
        line: usize,
        kind: TreeRowClickKind,
    },
    /// ツリーのcontext menu項目実行を要求する。
    TreeContextAction {
        pane_id: PaneId,
        line: usize,
        item: TreeContextItem,
    },
}

/// GUIへ通知するフィードバック送信結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackResultKind {
    Accepted,
    Invalid,
    RateLimited,
    ServerError,
    Network,
    Timeout,
}

/// app層からGUI起動時に渡す表示設定。
#[derive(Debug, Clone, PartialEq)]
pub struct GuiOptions {
    /// 保存確認ダイアログの操作一覧詳細度。
    pub confirm_detail: ConfirmDetail,
    /// ユーザーが明示した日本語fallbackフォントの絶対パス。
    pub font_path: Option<PathBuf>,
    /// ヘルプダイアログへ表示する、エンジン非依存の操作一覧。
    pub help_entries: Vec<HelpEntry>,
    /// ドックfocus操作へ割り当てられた解決済みキーシーケンス。
    pub dock_focus_bindings: Vec<KeySequence>,
    /// 左ドックへ表示する設定済みブックマーク。
    pub bookmarks: Vec<(String, PathBuf)>,
    /// 左ドックへ表示する最近使ったルート。
    pub recent_roots: Vec<PathBuf>,
    pub drives: Vec<PathBuf>,
    /// ステータスラインの左クラスタ表示項目(順序どおり)。
    pub statusline_left: Vec<StatusItem>,
    /// ステータスラインの右クラスタ表示項目(順序どおり)。
    pub statusline_right: Vec<StatusItem>,
}

struct AppOptions {
    confirm_detail: ConfirmDetail,
    help_entries: Vec<HelpEntry>,
    dock_focus_bindings: Vec<KeySequence>,
    bookmarks: Vec<(String, PathBuf)>,
    recent_roots: Vec<PathBuf>,
    drives: Vec<PathBuf>,
    statusline_left: Vec<StatusItem>,
    statusline_right: Vec<StatusItem>,
}

/// app層からGUIへ渡す描画指示。
#[derive(Clone)]
pub enum GuiEvent {
    /// paneをGUIの描画状態へ追加する。layout反映より先に送る。
    AddPane {
        pane_id: PaneId,
        engine: Arc<dyn EditorEngine>,
        root: PathBuf,
    },
    RemovePane(PaneId),
    LayoutChanged {
        layout: PaneLayout,
        active: PaneId,
    },
    Editor {
        pane_id: PaneId,
        event: EditorEvent,
    },
    /// app層で表示ルートが切り替わったことをモードラインへ反映する。
    RootChanged {
        pane_id: PaneId,
        root: PathBuf,
    },
    /// baselineのエントリIDに対応するGit装飾を全件差し替える。
    GitBadges {
        pane_id: PaneId,
        branch: Option<String>,
        badges: HashMap<EntryId, GitBadge>,
    },
    /// 読み取り不能なディレクトリの行末装飾を全件差し替える。
    IncompleteDirs {
        pane_id: PaneId,
        dirs: HashSet<EntryId>,
    },
    /// paneのroot到達性と部分scan状態をmodelineへ反映する。
    PaneHealth {
        pane_id: PaneId,
        offline: bool,
        unreadable: usize,
    },
    /// paneのnavigation historyのback/forward可用性をtoolbarへ反映する。
    HistoryState {
        pane_id: PaneId,
        can_go_back: bool,
        can_go_forward: bool,
    },
    /// 表示中のエントリIDに対応する表示用メタデータを全件差し替える。
    FileInfos {
        pane_id: PaneId,
        infos: HashMap<EntryId, FileInfo>,
    },
    /// 現在折りたたまれているディレクトリのID集合を差し替える。
    /// 展開/折りたたみアイコンの判定に使う(空ディレクトリの展開も正しく描く)。
    CollapsedDirs {
        pane_id: PaneId,
        dirs: HashSet<EntryId>,
    },
    /// 指定paneのファイルpickerを候補待ち状態で即座に開く。
    ShowFilePicker {
        pane_id: PaneId,
    },
    /// 検索workerが返した最新のpicker結果。
    PickerResults {
        pane_id: PaneId,
        query: String,
        results: Vec<PickerHit>,
        indexed_count: usize,
        indexing: bool,
    },
    /// 匿名フィードバック入力モーダルを開く。
    ShowFeedback,
    /// フィードバックworkerの完了結果。
    FeedbackResult {
        outcome: FeedbackResultKind,
        message: &'static str,
    },
    /// 指定された表示用パスをクリップボードへコピーする。
    CopyPath(String),
    /// open-with候補を表示する。
    ShowOpenWith {
        file_name: String,
        choices: Vec<String>,
    },
    /// 保存planと実行前に確認すべき警告を表示する。
    ShowPlan {
        plan: OperationPlan,
        warnings: Vec<String>,
        /// 承認時に既存実体をごみ箱へ退避する移動先。plan順。
        overwrites: Vec<TreePath>,
    },
    /// undo確認ダイアログを表示する。行はapp層で整形済み。
    ShowUndoPlan {
        lines: Vec<String>,
    },
    /// apply開始時に操作総数を設定して進捗ダイアログを表示する。
    ShowApplyProgress {
        /// 承認済みplanに含まれる操作総数。
        total: usize,
    },
    /// root scanまたは再帰directory loadの進捗ダイアログを表示する。
    ShowLoaderProgress {
        title: String,
        path: PathBuf,
    },
    /// loaderが発見した累計entry数を表示へ反映する。
    LoaderProgress {
        entries: usize,
    },
    /// loaderのキャンセル要求を受理済みとして操作を無効化する。
    LoaderCancelRequested,
    /// apply workerから届いた操作単位の進捗を表示へ反映する。
    ApplyProgress(ApplyProgress),
    /// undo workerから届いた操作単位の進捗を表示へ反映する。
    UndoProgress(ApplyProgress<String>),
    ShowTransferPlan {
        plan: TransferPlan,
        target: PaneId,
        overwrites: Vec<PathBuf>,
    },
    TransferProgress(ApplyProgress<TransferOp>),
    /// キャンセル要求を受理済みとして進捗ダイアログの操作を無効化する。
    ApplyCancelRequested,
    ShowReport(CommitReport),
    /// undo結果ダイアログを表示する。行はapp層で整形済み。
    ShowUndoReport {
        lines: Vec<String>,
        any_failed: bool,
    },
    ShowTransferReport(CommitReport<TransferOp>),
    /// clipboard・inbound drop取り込みplanと実行前に確認すべき上書き警告を表示する。
    ShowImportPlan {
        pane_id: PaneId,
        plan: ImportPlan,
        overwrites: Vec<PathBuf>,
    },
    /// import applyワーカーから届いた操作単位の進捗を表示へ反映する。
    ImportProgress(ApplyProgress<ImportOp>),
    ShowImportReport {
        report: CommitReport<ImportOp>,
        effect: DropEffect,
    },
    ShowValidationErrors(Vec<ValidateError>),
    /// 起動時復旧ダイアログを表示する。行はapp層で整形済み。
    ShowUndoRecovery {
        descriptions: Vec<String>,
    },
    FatalError(String),
    CloseDialog,
}

#[derive(Debug, Clone)]
enum DialogState {
    Plan {
        plan: OperationPlan,
        warnings: Vec<String>,
        overwrites: Vec<TreePath>,
    },
    UndoPlan {
        lines: Vec<String>,
    },
    TransferPlan {
        plan: TransferPlan,
        target: PaneId,
        overwrites: Vec<PathBuf>,
    },
    ImportPlan {
        plan: ImportPlan,
        overwrites: Vec<PathBuf>,
    },
    Progress {
        completed: usize,
        total: usize,
        /// これから実行する操作の表示ラベル。
        current: Option<String>,
        cancel_requested: bool,
    },
    LoaderProgress {
        title: String,
        path: PathBuf,
        entries: usize,
        cancel_requested: bool,
    },
    Report(CommitReport),
    UndoReport {
        lines: Vec<String>,
        any_failed: bool,
    },
    TransferReport(CommitReport<TransferOp>),
    ImportReport {
        report: CommitReport<ImportOp>,
        effect: DropEffect,
    },
    ValidationErrors(Vec<ValidateError>),
    OpenWith {
        file_name: String,
        choices: Vec<String>,
        selected: usize,
    },
    UndoRecovery {
        descriptions: Vec<String>,
    },
    FilePicker {
        pane_id: PaneId,
        query: String,
        selected: usize,
        results: Vec<PickerHit>,
        indexed_count: usize,
        indexing: bool,
    },
    Feedback {
        kind: FeedbackKind,
        body: String,
        stage: FeedbackStage,
    },
    Help,
    /// ツリーの右click context menu(GUI local。app層の往復を待たず即表示する)。
    TreeContext {
        pane_id: PaneId,
        line: usize,
        /// 表示位置(右clickのscreen座標)。
        pos: egui::Pos2,
        has_id: bool,
        is_dir: bool,
        /// 表示中のpaneのengineが健全か(crashed=false)。
        engine_ok: bool,
        /// 表示中のpaneのrootがoffline(到達不能)か。
        offline: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FeedbackStage {
    Input,
    Confirm,
    Sending,
    Done(&'static str),
    Failed(FeedbackResultKind, &'static str),
}

#[derive(Debug, Default)]
struct NavigationDockState {
    open: bool,
    focused: bool,
    selected: usize,
    pending_binding: Vec<KeyInput>,
}

impl NavigationDockState {
    #[cfg(test)]
    fn visible() -> Self {
        Self {
            open: true,
            ..Self::default()
        }
    }

    /// ドックfocusのトグル。閉→開+focus / 開+focus無→focus / 開+focus有→閉(非表示)。
    fn toggle_focus(&mut self) {
        self.pending_binding.clear();
        if !self.open {
            self.open = true;
            self.focused = true;
            self.selected = 0;
        } else if !self.focused {
            self.focused = true;
            self.selected = 0;
        } else {
            self.open = false;
            self.focused = false;
        }
    }
}

/// fylerのGUIアプリケーション。
///
/// 描画契約:
/// - 毎フレーム [`EditorEngine::snapshot`] を1回だけ取得し、そのsnapshotのみで
///   描画する(lines/cursor/modeを別々のタイミングで読まない。整合性のため)
/// - RPC完了を同期待ちしない。入力は [`EditorEngine::send`] へ投げるだけ
pub struct FylerApp {
    panes: BTreeMap<PaneId, PaneViewState>,
    layout: Option<PaneLayout>,
    active: Option<PaneId>,
    event_rx: mpsc::Receiver<GuiEvent>,
    event_dequeued: Arc<dyn Fn() + Send + Sync>,
    cmdline: Option<CmdlineState>,
    popupmenu: Option<PopupmenuState>,
    message: Option<EditorMessage>,
    pending_copy: Option<String>,
    fatal_error: Option<String>,
    dialog: Option<DialogState>,
    action_tx: mpsc::Sender<GuiAction>,
    picker_needs_focus: bool,
    feedback_needs_focus: bool,
    confirm_detail: ConfirmDetail,
    help_entries: Vec<HelpEntry>,
    dock_focus_bindings: Vec<KeySequence>,
    navigation_dock: NavigationDockState,
    bookmarks: Vec<(String, PathBuf)>,
    recent_roots: Vec<PathBuf>,
    drives: Vec<PathBuf>,
    statusline_left: Vec<StatusItem>,
    statusline_right: Vec<StatusItem>,
    /// native window geometryが未保存の初回起動だけ、monitor比率でサイズを設定する。
    resize_to_monitor_on_first_frame: bool,
    window_geometry: Arc<Mutex<Option<WindowGeometry>>>,
}

struct PaneViewState {
    engine: Arc<dyn EditorEngine>,
    root: PathBuf,
    git_badges: HashMap<EntryId, GitBadge>,
    branch: Option<String>,
    incomplete_dirs: HashSet<EntryId>,
    offline: bool,
    unreadable: usize,
    file_infos: HashMap<EntryId, FileInfo>,
    collapsed_dirs: HashSet<EntryId>,
    engine_error: Option<String>,
    tree_viewport: Option<tree_view::TreeViewport>,
    can_go_back: bool,
    can_go_forward: bool,
}

impl FylerApp {
    fn new(
        gui_events: mpsc::Receiver<GuiEvent>,
        action_tx: mpsc::Sender<GuiAction>,
        options: AppOptions,
        repaint_context: egui::Context,
        event_dequeued: Arc<dyn Fn() + Send + Sync>,
    ) -> anyhow::Result<Self> {
        let AppOptions {
            confirm_detail,
            help_entries,
            dock_focus_bindings,
            bookmarks,
            recent_roots,
            drives,
            statusline_left,
            statusline_right,
        } = options;
        let (event_tx, event_rx) = mpsc::channel();
        thread::Builder::new()
            .name("fyler-editor-events".to_owned())
            // recv/forward/repaintだけの非再帰ループなので既定2MiB stackは不要。
            .stack_size(256 * 1024)
            .spawn(move || {
                while let Ok(first) = gui_events.recv() {
                    if event_tx.send(first).is_err() {
                        return;
                    }
                    while let Ok(event) = gui_events.try_recv() {
                        if event_tx.send(event).is_err() {
                            return;
                        }
                    }
                    // 到着済みbatchを順序どおり転送した後、再描画要求は1回にまとめる。
                    repaint_context.request_repaint();
                }
            })
            .map_err(|error| anyhow::anyhow!("Failed to start editor event monitor: {error}"))?;

        Ok(Self {
            panes: BTreeMap::new(),
            layout: None,
            active: None,
            event_rx,
            event_dequeued,
            cmdline: None,
            popupmenu: None,
            message: None,
            pending_copy: None,
            fatal_error: None,
            dialog: None,
            action_tx,
            picker_needs_focus: false,
            feedback_needs_focus: false,
            confirm_detail,
            help_entries,
            dock_focus_bindings,
            navigation_dock: NavigationDockState::default(),
            bookmarks,
            recent_roots,
            drives,
            statusline_left,
            statusline_right,
            resize_to_monitor_on_first_frame: false,
            window_geometry: Arc::new(Mutex::new(None)),
        })
    }

    fn receive_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            (self.event_dequeued)();
            match event {
                GuiEvent::AddPane {
                    pane_id,
                    engine,
                    root,
                } => {
                    self.panes.insert(
                        pane_id,
                        PaneViewState {
                            engine,
                            root,
                            git_badges: HashMap::new(),
                            branch: None,
                            incomplete_dirs: HashSet::new(),
                            offline: false,
                            unreadable: 0,
                            file_infos: HashMap::new(),
                            collapsed_dirs: HashSet::new(),
                            engine_error: None,
                            tree_viewport: None,
                            can_go_back: false,
                            can_go_forward: false,
                        },
                    );
                }
                GuiEvent::RemovePane(pane_id) => {
                    self.panes.remove(&pane_id);
                    if matches!(
                        &self.dialog,
                        Some(DialogState::FilePicker { pane_id: owner, .. })
                            | Some(DialogState::TreeContext { pane_id: owner, .. })
                                if *owner == pane_id
                    ) {
                        self.dialog = None;
                    }
                }
                GuiEvent::LayoutChanged { layout, active } => {
                    self.layout = Some(layout);
                    self.active = Some(active);
                }
                GuiEvent::Editor { pane_id, event } => match event {
                    EditorEvent::SnapshotUpdated => {
                        if let Some(pane) = self.panes.get(&pane_id) {
                            pane.engine.acknowledge_snapshot_update();
                        }
                    }
                    EditorEvent::ActivateLine { .. } => {}
                    EditorEvent::OpenWith { .. } => {}
                    EditorEvent::YankPath { .. } => {}
                    EditorEvent::NavigateInto { .. } => {}
                    EditorEvent::OpenTerminal { .. } => {}
                    EditorEvent::NavigateParent => {}
                    EditorEvent::HistoryBack => {}
                    EditorEvent::HistoryForward => {}
                    EditorEvent::RefreshRequested => {}
                    EditorEvent::ChangeDirectory { .. } => {}
                    EditorEvent::ChangeSort { .. } => {}
                    EditorEvent::ToggleHidden => {}
                    EditorEvent::ToggleDockFocus => self.navigation_dock.toggle_focus(),
                    EditorEvent::Fold { .. } => {}
                    EditorEvent::JumpBookmark { .. } => {}
                    EditorEvent::OpenFilePicker => {}
                    EditorEvent::FeedbackRequested => {}
                    EditorEvent::ShowHelp => self.dialog = Some(DialogState::Help),
                    EditorEvent::PaneAction(_) => {}
                    EditorEvent::TransferRequested { .. } => {}
                    EditorEvent::ClipboardCopyRequested { .. } => {}
                    EditorEvent::ClipboardCutRequested { .. } => {}
                    EditorEvent::ClipboardPasteRequested { .. } => {}
                    EditorEvent::CommitRequested { .. } => {}
                    EditorEvent::UndoRequested => {}
                    EditorEvent::CmdlineShow(state) if self.active == Some(pane_id) => {
                        self.cmdline = Some(state);
                    }
                    EditorEvent::CmdlineShow(_) => {}
                    EditorEvent::CmdlineHide if self.active == Some(pane_id) => {
                        self.cmdline = None;
                        self.popupmenu = None;
                    }
                    EditorEvent::CmdlineHide => {}
                    EditorEvent::PopupmenuShow(state) if self.active == Some(pane_id) => {
                        self.popupmenu = Some(state);
                    }
                    EditorEvent::PopupmenuShow(_) => {}
                    EditorEvent::PopupmenuSelect { selected } if self.active == Some(pane_id) => {
                        if let Some(state) = &mut self.popupmenu {
                            state.selected = selected;
                        }
                    }
                    EditorEvent::PopupmenuSelect { .. } => {}
                    EditorEvent::PopupmenuHide if self.active == Some(pane_id) => {
                        self.popupmenu = None;
                    }
                    EditorEvent::PopupmenuHide => {}
                    EditorEvent::Message(message) => self.message = Some(message),
                    EditorEvent::EngineCrashed { reason } => {
                        if matches!(
                            &self.dialog,
                            Some(DialogState::FilePicker { pane_id: owner, .. })
                                | Some(DialogState::TreeContext { pane_id: owner, .. })
                                    if *owner == pane_id
                        ) {
                            self.dialog = None;
                        }
                        if let Some(pane) = self.panes.get_mut(&pane_id) {
                            pane.engine_error = Some(format!("Editor engine stopped: {reason}"));
                        }
                    }
                },
                GuiEvent::RootChanged { pane_id, root } => {
                    self.navigation_dock.selected = 0;
                    if let Some(pane) = self.panes.get_mut(&pane_id) {
                        pane.root = root.clone();
                    }
                    self.recent_roots.retain(|recent| recent != &root);
                    self.recent_roots.insert(0, root);
                    self.recent_roots.truncate(10);
                }
                GuiEvent::GitBadges {
                    pane_id,
                    branch,
                    badges,
                } => {
                    if let Some(pane) = self.panes.get_mut(&pane_id) {
                        pane.branch = branch;
                        pane.git_badges = badges;
                    }
                }
                GuiEvent::IncompleteDirs { pane_id, dirs } => {
                    if let Some(pane) = self.panes.get_mut(&pane_id) {
                        pane.incomplete_dirs = dirs;
                    }
                }
                GuiEvent::PaneHealth {
                    pane_id,
                    offline,
                    unreadable,
                } => {
                    if let Some(pane) = self.panes.get_mut(&pane_id) {
                        pane.offline = offline;
                        pane.unreadable = unreadable;
                    }
                }
                GuiEvent::HistoryState {
                    pane_id,
                    can_go_back,
                    can_go_forward,
                } => {
                    if let Some(pane) = self.panes.get_mut(&pane_id) {
                        pane.can_go_back = can_go_back;
                        pane.can_go_forward = can_go_forward;
                    }
                }
                GuiEvent::FileInfos { pane_id, infos } => {
                    if let Some(pane) = self.panes.get_mut(&pane_id) {
                        pane.file_infos = infos;
                    }
                }
                GuiEvent::CollapsedDirs { pane_id, dirs } => {
                    if let Some(pane) = self.panes.get_mut(&pane_id) {
                        pane.collapsed_dirs = dirs;
                    }
                }
                GuiEvent::ShowFilePicker { pane_id } => {
                    self.dialog = Some(DialogState::FilePicker {
                        pane_id,
                        query: String::new(),
                        selected: 0,
                        results: Vec::new(),
                        indexed_count: 0,
                        indexing: true,
                    });
                    self.picker_needs_focus = true;
                    let _ = self
                        .action_tx
                        .send(picker_query_action(pane_id, String::new()));
                }
                GuiEvent::PickerResults {
                    pane_id,
                    query: _,
                    results,
                    indexed_count,
                    indexing,
                } => {
                    if let Some(DialogState::FilePicker {
                        pane_id: owner,
                        selected,
                        results: current,
                        indexed_count: current_count,
                        indexing: current_indexing,
                        ..
                    }) = &mut self.dialog
                        && *owner == pane_id
                    {
                        *current = results;
                        *current_count = indexed_count;
                        *current_indexing = indexing;
                        *selected = (*selected).min(current.len().saturating_sub(1));
                    }
                }
                GuiEvent::ShowFeedback => {
                    self.dialog = Some(DialogState::Feedback {
                        kind: FeedbackKind::Impression,
                        body: String::new(),
                        stage: FeedbackStage::Input,
                    });
                    self.feedback_needs_focus = true;
                }
                GuiEvent::FeedbackResult { outcome, message } => {
                    if let Some(DialogState::Feedback { stage, .. }) = &mut self.dialog
                        && *stage == FeedbackStage::Sending
                    {
                        *stage = if outcome == FeedbackResultKind::Accepted {
                            FeedbackStage::Done(message)
                        } else {
                            FeedbackStage::Failed(outcome, message)
                        };
                    }
                }
                GuiEvent::CopyPath(path) => self.pending_copy = Some(path),
                GuiEvent::ShowOpenWith { file_name, choices } => {
                    self.dialog = Some(DialogState::OpenWith {
                        file_name,
                        choices,
                        selected: 0,
                    });
                }
                GuiEvent::ShowPlan {
                    plan,
                    warnings,
                    overwrites,
                } => {
                    self.dialog = Some(DialogState::Plan {
                        plan,
                        warnings,
                        overwrites,
                    });
                }
                GuiEvent::ShowUndoPlan { lines } => {
                    self.dialog = Some(DialogState::UndoPlan { lines });
                }
                GuiEvent::ShowTransferPlan {
                    plan,
                    target,
                    overwrites,
                } => {
                    self.dialog = Some(DialogState::TransferPlan {
                        plan,
                        target,
                        overwrites,
                    });
                }
                GuiEvent::ShowImportPlan {
                    pane_id: _,
                    plan,
                    overwrites,
                } => {
                    self.dialog = Some(DialogState::ImportPlan { plan, overwrites });
                }
                GuiEvent::ShowApplyProgress { total } => {
                    self.dialog = Some(DialogState::Progress {
                        completed: 0,
                        total,
                        current: None,
                        cancel_requested: false,
                    });
                }
                GuiEvent::ShowLoaderProgress { title, path } => {
                    self.dialog = Some(DialogState::LoaderProgress {
                        title,
                        path,
                        entries: 0,
                        cancel_requested: false,
                    });
                }
                GuiEvent::LoaderProgress { entries } => {
                    if let Some(DialogState::LoaderProgress {
                        entries: current, ..
                    }) = &mut self.dialog
                    {
                        *current = entries;
                    }
                }
                GuiEvent::LoaderCancelRequested => {
                    if let Some(DialogState::LoaderProgress {
                        cancel_requested, ..
                    }) = &mut self.dialog
                    {
                        *cancel_requested = true;
                    }
                }
                GuiEvent::ApplyProgress(progress) => {
                    if let Some(DialogState::Progress {
                        completed,
                        total,
                        current,
                        ..
                    }) = &mut self.dialog
                    {
                        *completed = progress.completed;
                        *total = progress.total;
                        *current = progress.current.as_ref().map(confirm::operation_label);
                    }
                }
                GuiEvent::UndoProgress(progress) => {
                    if let Some(DialogState::Progress {
                        completed,
                        total,
                        current,
                        ..
                    }) = &mut self.dialog
                    {
                        *completed = progress.completed;
                        *total = progress.total;
                        *current = progress.current;
                    }
                }
                GuiEvent::TransferProgress(progress) => {
                    if let Some(DialogState::Progress {
                        completed,
                        total,
                        current,
                        ..
                    }) = &mut self.dialog
                    {
                        *completed = progress.completed;
                        *total = progress.total;
                        *current = progress
                            .current
                            .as_ref()
                            .map(|operation| confirm::transfer_operation_label(operation, None));
                    }
                }
                GuiEvent::ImportProgress(progress) => {
                    if let Some(DialogState::Progress {
                        completed,
                        total,
                        current,
                        ..
                    }) = &mut self.dialog
                    {
                        *completed = progress.completed;
                        *total = progress.total;
                        *current = progress
                            .current
                            .as_ref()
                            .map(|operation| confirm::import_operation_label(operation, None));
                    }
                }
                GuiEvent::ApplyCancelRequested => {
                    if let Some(DialogState::Progress {
                        cancel_requested, ..
                    }) = &mut self.dialog
                    {
                        *cancel_requested = true;
                    }
                }
                GuiEvent::ShowReport(report) => {
                    self.dialog = Some(DialogState::Report(report));
                }
                GuiEvent::ShowUndoReport { lines, any_failed } => {
                    self.dialog = Some(DialogState::UndoReport { lines, any_failed });
                }
                GuiEvent::ShowTransferReport(report) => {
                    self.dialog = Some(DialogState::TransferReport(report));
                }
                GuiEvent::ShowImportReport { report, effect } => {
                    self.dialog = Some(DialogState::ImportReport { report, effect });
                }
                GuiEvent::ShowValidationErrors(errors) => {
                    self.dialog = Some(DialogState::ValidationErrors(errors));
                }
                GuiEvent::ShowUndoRecovery { descriptions } => {
                    self.dialog = Some(DialogState::UndoRecovery { descriptions });
                }
                GuiEvent::FatalError(error) => {
                    self.fatal_error = Some(error);
                }
                GuiEvent::CloseDialog => self.dialog = None,
            }
        }
    }
}

impl eframe::App for FylerApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.receive_events();
        if self.resize_to_monitor_on_first_frame
            && let Some((size, position)) =
                initial_window_geometry(ctx.input(|input| input.viewport().monitor_size))
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(position));
            self.resize_to_monitor_on_first_frame = false;
        }
        if let Some(geometry) = current_window_geometry(ctx)
            && let Ok(mut current) = self.window_geometry.lock()
        {
            *current = Some(geometry);
        }
        if let Some(path) = self.pending_copy.take() {
            ctx.copy_text(path.clone());
            self.message = Some(EditorMessage {
                kind: fyler_core::editor::MessageKind::Info,
                text: format!("Copied: {path}"),
            });
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // paneごとにフレーム冒頭のsnapshotを1回だけ取得し、入力と描画で共有する。
        let snapshots = self
            .panes
            .iter()
            .map(|(id, pane)| (*id, pane.engine.snapshot()))
            .collect::<BTreeMap<_, _>>();

        let chrome_state = self.active.and_then(|pane_id| {
            let pane = self.panes.get(&pane_id)?;
            Some((
                pane_id,
                pane.root.clone(),
                pane.can_go_back,
                pane.can_go_forward,
            ))
        });
        let mut chrome_action = None;
        egui::Panel::top("fyler-toolbar")
            .exact_size(theme::TOOLBAR_HEIGHT)
            .show(ui, |ui| {
                let (can_go_back, can_go_forward) = chrome_state
                    .as_ref()
                    .map(|(_, _, back, forward)| (*back, *forward))
                    .unwrap_or((false, false));
                chrome_action = chrome::draw_toolbar(ui, can_go_back, can_go_forward);
            });
        let navigation_entries = chrome_state
            .as_ref()
            .map(|(_, root, ..)| {
                chrome::navigation_entries(root, &self.bookmarks, &self.recent_roots, &self.drives)
            })
            .unwrap_or_default();
        if self.dialog.is_none()
            && let (Some((pane_id, ..)), Some(action)) = (chrome_state.as_ref(), chrome_action)
        {
            let event = match action {
                chrome::ChromeAction::NavigateParent => EditorEvent::NavigateParent,
                chrome::ChromeAction::HistoryBack => EditorEvent::HistoryBack,
                chrome::ChromeAction::HistoryForward => EditorEvent::HistoryForward,
                chrome::ChromeAction::Refresh => EditorEvent::RefreshRequested,
            };
            if self
                .action_tx
                .send(GuiAction::Editor {
                    pane_id: *pane_id,
                    event,
                })
                .is_err()
            {
                self.fatal_error = Some("Failed to send toolbar action to app".to_owned());
            }
        }
        let navigation_focused = self.navigation_dock.focused;
        if !navigation_focused
            && should_forward_input(
                self.dialog.is_none(),
                self.fatal_error.is_none(),
                self.active
                    .and_then(|active| self.panes.get(&active))
                    .is_some_and(|pane| pane.engine_error.is_none()),
            )
            && let Some(active) = self.active
            && let (Some(pane), Some(snapshot)) =
                (self.panes.get_mut(&active), snapshots.get(&active))
            && let Err(error) = input::forward_input(ui.ctx(), pane.engine.as_ref(), &snapshot.mode)
        {
            pane.engine_error = Some(format!("Failed to send input to editor engine: {error}"));
        }

        if self.cmdline.is_some() || self.popupmenu.is_some() {
            egui::Panel::bottom("global-command-area").show(ui, |ui| {
                if let Some(state) = &self.popupmenu {
                    cmdline::draw_popupmenu(ui, state);
                }
                if let Some(state) = &self.cmdline {
                    cmdline::draw_cmdline(ui, state);
                }
            });
        } else if let Some(message) = &self.message {
            egui::Panel::top("global-message-area")
                .exact_size(34.0)
                .show(ui, |ui| cmdline::draw_message(ui, message));
        }

        let layout = self.layout.clone();
        let active = self.active;
        let fatal_error = self.fatal_error.clone();

        let dragging_files = ui.ctx().input(|i| !i.raw.hovered_files.is_empty());
        let (ime, navigation_clicked, tree_click, drop_target_line) = egui::CentralPanel::default()
            .show(ui, |ui| {
                if let Some(error) = fatal_error {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                    (None, None, None, None)
                } else if let (Some(layout), Some(active)) = (layout.as_ref(), active) {
                    draw_layout(
                        ui,
                        layout,
                        active,
                        &mut self.panes,
                        &snapshots,
                        &navigation_entries,
                        self.navigation_dock.open,
                        self.navigation_dock.focused,
                        self.navigation_dock.selected,
                        &self.statusline_left,
                        &self.statusline_right,
                        dragging_files,
                    )
                } else {
                    (None, None, None, None)
                }
            })
            .inner;
        let mut navigation_target = None;
        if self.dialog.is_none() {
            if let Some(index) = navigation_clicked {
                self.navigation_dock.focused = true;
                self.navigation_dock.selected = index;
                navigation_target = navigation_entries
                    .get(index)
                    .map(|entry| entry.path.clone());
            }
            if self.navigation_dock.focused {
                navigation_target = handle_navigation_keys(
                    &mut self.navigation_dock,
                    input::normalized_keys(ui.ctx()),
                    &self.dock_focus_bindings,
                    &navigation_entries,
                )
                .or(navigation_target);
            }
        } else {
            self.navigation_dock.pending_binding.clear();
        }
        if self.dialog.is_none()
            && let (Some(active), Some(target)) = (active, navigation_target)
            && self
                .action_tx
                .send(GuiAction::Editor {
                    pane_id: active,
                    event: EditorEvent::ChangeDirectory {
                        query: Some(target.display().to_string()),
                    },
                })
                .is_err()
        {
            self.fatal_error = Some("Failed to send navigation action to app".to_owned());
        }
        if self.dialog.is_none()
            && let Some(TreeClickEvent { pane_id, click }) = tree_click
        {
            match click {
                TreeClickEventKind::Row(row) if row.kind == tree_view::RowClickKind::Secondary => {
                    let engine_ok = self
                        .panes
                        .get(&pane_id)
                        .is_some_and(|pane| pane.engine_error.is_none());
                    let offline = self.panes.get(&pane_id).is_some_and(|pane| pane.offline);
                    self.dialog = Some(DialogState::TreeContext {
                        pane_id,
                        line: row.line,
                        pos: row.pos,
                        has_id: row.has_id,
                        is_dir: row.is_dir,
                        engine_ok,
                        offline,
                    });
                    if self
                        .action_tx
                        .send(GuiAction::TreeRowClicked {
                            pane_id,
                            line: row.line,
                            kind: TreeRowClickKind::Single,
                        })
                        .is_err()
                    {
                        self.fatal_error = Some("Failed to send tree click to app".to_owned());
                    }
                }
                TreeClickEventKind::Row(row) => {
                    let kind = match row.kind {
                        tree_view::RowClickKind::Single => TreeRowClickKind::Single,
                        tree_view::RowClickKind::Double => TreeRowClickKind::Double,
                        tree_view::RowClickKind::Shift => TreeRowClickKind::Shift,
                        // 上のガード付きアームで処理済み。到達しない。
                        tree_view::RowClickKind::Secondary => TreeRowClickKind::Single,
                    };
                    if self
                        .action_tx
                        .send(GuiAction::TreeRowClicked {
                            pane_id,
                            line: row.line,
                            kind,
                        })
                        .is_err()
                    {
                        self.fatal_error = Some("Failed to send tree click to app".to_owned());
                    }
                }
                TreeClickEventKind::Blank => {
                    if self
                        .action_tx
                        .send(GuiAction::RequestPaneFocus { pane_id })
                        .is_err()
                    {
                        self.fatal_error =
                            Some("Failed to send pane focus request to app".to_owned());
                    }
                }
            }
        }
        if self.dialog.is_none()
            && let Some(ime) = ime
        {
            ui.ctx().output_mut(|platform_output| {
                platform_output.ime = Some(egui::output::IMEOutput {
                    rect: ime.tree_rect,
                    cursor_rect: ime.cursor_rect,
                    should_interrupt_composition: false,
                });
            });
        }
        let dropped_paths: Vec<PathBuf> = ui.ctx().input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .filter_map(|file| file.path.clone())
                .collect()
        });
        if !dropped_paths.is_empty()
            && let Some(active) = self.active
        {
            let effect = if ui.ctx().input(|input| input.modifiers.shift) {
                DropEffect::Move
            } else {
                DropEffect::Copy
            };
            if self
                .action_tx
                .send(GuiAction::FilesDropped {
                    pane_id: active,
                    line: drop_target_line,
                    paths: dropped_paths,
                    effect,
                })
                .is_err()
            {
                self.fatal_error = Some("Failed to send dropped files to app".to_owned());
            }
        }

        let mut confirm_choice = None;
        let mut cancel_apply = false;
        let mut cancel_loader = false;
        let mut dismiss_errors = false;
        let mut dismiss_report = false;
        let mut open_with_choice = None;
        let mut picker_result = None;
        let mut picker_owner = None;
        let mut feedback_result = None;
        let mut tree_context_result = None;
        match &mut self.dialog {
            Some(DialogState::Plan {
                plan,
                warnings,
                overwrites,
            }) => {
                confirm_choice =
                    confirm::draw_plan(ui, plan, overwrites, warnings, self.confirm_detail);
            }
            Some(DialogState::TransferPlan {
                plan,
                target,
                overwrites,
            }) => {
                confirm_choice = confirm::draw_transfer_plan(ui, plan, *target, overwrites);
            }
            Some(DialogState::ImportPlan {
                plan, overwrites, ..
            }) => {
                confirm_choice = confirm::draw_import_plan(ui, plan, overwrites);
            }
            Some(DialogState::UndoPlan { lines }) => {
                confirm_choice = confirm::draw_undo_plan(ui, lines);
            }
            Some(DialogState::Report(report)) => {
                dismiss_report = confirm::draw_report(ui, report);
            }
            Some(DialogState::UndoReport { lines, any_failed }) => {
                dismiss_report = confirm::draw_undo_report(ui, lines, *any_failed);
            }
            Some(DialogState::TransferReport(report)) => {
                dismiss_report = confirm::draw_transfer_report(ui, report);
            }
            Some(DialogState::ImportReport { report, effect }) => {
                dismiss_report = confirm::draw_import_report(ui, report, *effect);
            }
            Some(DialogState::UndoRecovery { descriptions }) => {
                confirm_choice = confirm::draw_undo_recovery(ui, descriptions);
            }
            Some(DialogState::Progress {
                completed,
                total,
                current,
                cancel_requested,
            }) => {
                cancel_apply = confirm::draw_apply_progress(
                    ui,
                    *completed,
                    *total,
                    current.as_deref(),
                    *cancel_requested,
                );
            }
            Some(DialogState::LoaderProgress {
                title,
                path,
                entries,
                cancel_requested,
            }) => {
                cancel_loader =
                    confirm::draw_loader_progress(ui, title, path, *entries, *cancel_requested);
            }
            Some(DialogState::Help) => {
                dismiss_errors = draw_help(ui, &self.help_entries);
            }
            Some(DialogState::ValidationErrors(errors)) => {
                let dismiss_from_keyboard = ui.ctx().input(|input| {
                    input.key_pressed(egui::Key::Enter) || input.key_pressed(egui::Key::Escape)
                });
                dismiss_errors = egui::Modal::new(egui::Id::new("save-validation-errors"))
                    .show(ui.ctx(), |ui| {
                        ui.heading("Cannot save");
                        ui.add_space(8.0);
                        confirm::draw_validation_errors(ui, errors);
                        ui.add_space(12.0);
                        ui.button("Dismiss (Enter / Esc)").clicked() || dismiss_from_keyboard
                    })
                    .inner;
            }
            Some(DialogState::OpenWith {
                file_name,
                choices,
                selected,
            }) => {
                let (choice, next_selected) =
                    confirm::draw_open_with(ui, file_name, choices, *selected);
                if let Some(next_selected) = next_selected {
                    *selected = next_selected;
                }
                open_with_choice = choice;
            }
            Some(DialogState::FilePicker {
                pane_id,
                query,
                selected,
                results,
                indexed_count,
                indexing,
            }) => {
                picker_owner = Some(*pane_id);
                let (closed, query_action) = draw_file_picker(
                    ui,
                    *pane_id,
                    query,
                    selected,
                    results,
                    *indexed_count,
                    *indexing,
                    &mut self.picker_needs_focus,
                );
                picker_result = closed;
                if let Some(action) = query_action
                    && self.action_tx.send(action).is_err()
                {
                    self.fatal_error = Some("Failed to send picker query to app".to_owned());
                }
            }
            Some(DialogState::Feedback { kind, body, stage }) => {
                feedback_result =
                    draw_feedback(ui, kind, body, stage, &mut self.feedback_needs_focus);
            }
            Some(DialogState::TreeContext {
                pane_id,
                line,
                pos,
                has_id,
                is_dir,
                engine_ok,
                offline,
            }) => {
                tree_context_result = Some((
                    *pane_id,
                    *line,
                    draw_tree_context_menu(ui, *pos, *has_id, *is_dir, *engine_ok, *offline),
                ));
            }
            None => {}
        }

        if dismiss_errors {
            self.dialog = None;
        }
        if dismiss_report {
            self.dialog = None;
        }
        if let Some(choice) = confirm_choice
            && self.action_tx.send(GuiAction::Confirm(choice)).is_err()
        {
            self.fatal_error = Some("Failed to send confirmation result to app".to_owned());
        }
        if let Some(choice) = open_with_choice {
            self.dialog = None;
            if self.action_tx.send(GuiAction::Confirm(choice)).is_err() {
                self.fatal_error = Some("Failed to send open-with result to app".to_owned());
            }
        }
        if cancel_apply
            && self
                .action_tx
                .send(GuiAction::Confirm(ConfirmChoice::Cancel))
                .is_err()
        {
            self.fatal_error = Some("Failed to send cancel request to app".to_owned());
        }
        if cancel_loader && self.action_tx.send(GuiAction::LoaderCancel).is_err() {
            self.fatal_error = Some("Failed to send loader cancel request to app".to_owned());
        }
        if let Some(result) = picker_result {
            self.dialog = None;
            let action = picker_completion_action(picker_owner.unwrap_or(PaneId::new(0)), result);
            if self.action_tx.send(action).is_err() {
                self.fatal_error = Some("Failed to send picker result to app".to_owned());
            }
        }
        if let Some(result) = feedback_result {
            match result {
                FeedbackUiResult::Close => {
                    self.dialog = None;
                    if self.action_tx.send(GuiAction::FeedbackClosed).is_err() {
                        self.fatal_error =
                            Some("Failed to close feedback dialog in app".to_owned());
                    }
                }
                FeedbackUiResult::Submit { kind, body } => {
                    if self
                        .action_tx
                        .send(GuiAction::FeedbackSubmit { kind, body })
                        .is_err()
                    {
                        self.fatal_error =
                            Some("Failed to send feedback request to app".to_owned());
                    }
                }
            }
        }
        if let Some((pane_id, line, outcome)) = tree_context_result {
            match outcome {
                TreeContextOutcome::Chosen(item) => {
                    self.dialog = None;
                    if self
                        .action_tx
                        .send(GuiAction::TreeContextAction {
                            pane_id,
                            line,
                            item,
                        })
                        .is_err()
                    {
                        self.fatal_error =
                            Some("Failed to send context menu action to app".to_owned());
                    }
                }
                TreeContextOutcome::Dismissed => {
                    self.dialog = None;
                }
                TreeContextOutcome::Pending => {}
            }
        }
    }
}

fn should_forward_input(dialog_absent: bool, fatal_absent: bool, engine_healthy: bool) -> bool {
    dialog_absent && fatal_absent && engine_healthy
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DockBindingMatch {
    Activated,
    Pending,
    Unmatched,
}

fn advance_dock_binding(
    pending: &mut Vec<KeyInput>,
    key: KeyInput,
    bindings: &[KeySequence],
) -> DockBindingMatch {
    pending.push(key);
    if bindings
        .iter()
        .any(|binding| binding.0.as_slice() == pending.as_slice())
    {
        pending.clear();
        return DockBindingMatch::Activated;
    }
    if bindings
        .iter()
        .any(|binding| binding.0.starts_with(pending))
    {
        return DockBindingMatch::Pending;
    }

    pending.clear();
    pending.push(key);
    if bindings
        .iter()
        .any(|binding| binding.0.as_slice() == pending.as_slice())
    {
        pending.clear();
        DockBindingMatch::Activated
    } else if bindings
        .iter()
        .any(|binding| binding.0.starts_with(pending))
    {
        DockBindingMatch::Pending
    } else {
        pending.clear();
        DockBindingMatch::Unmatched
    }
}

fn handle_navigation_keys(
    state: &mut NavigationDockState,
    keys: impl IntoIterator<Item = KeyInput>,
    bindings: &[KeySequence],
    entries: &[chrome::NavigationEntry],
) -> Option<PathBuf> {
    let mut target = None;
    for key in keys {
        let starts_binding = bindings
            .iter()
            .any(|binding| binding.0.first() == Some(&key));
        if state.pending_binding.is_empty()
            && !starts_binding
            && handle_navigation_movement(state, key, entries, &mut target)
        {
            continue;
        }
        match advance_dock_binding(&mut state.pending_binding, key, bindings) {
            DockBindingMatch::Activated => state.toggle_focus(),
            DockBindingMatch::Pending => {}
            DockBindingMatch::Unmatched => {
                let _ = handle_navigation_movement(state, key, entries, &mut target);
            }
        }
    }
    target
}

fn handle_navigation_movement(
    state: &mut NavigationDockState,
    key: KeyInput,
    entries: &[chrome::NavigationEntry],
    target: &mut Option<PathBuf>,
) -> bool {
    if key.mods != Modifiers::default() {
        return false;
    }
    if key.key == Key::Esc {
        state.focused = false;
        state.pending_binding.clear();
        return true;
    }
    if entries.is_empty() {
        return false;
    }
    match key.key {
        Key::Char('j') | Key::Down => {
            state.selected = (state.selected + 1).min(entries.len() - 1);
            true
        }
        Key::Char('k') | Key::Up => {
            state.selected = state.selected.saturating_sub(1);
            true
        }
        Key::Enter => {
            state.selected = state.selected.min(entries.len() - 1);
            *target = Some(entries[state.selected].path.clone());
            state.focused = false;
            state.pending_binding.clear();
            true
        }
        _ => false,
    }
}

struct ImeGeometry {
    tree_rect: egui::Rect,
    cursor_rect: egui::Rect,
}

/// このフレームでツリー上に起きたクリックの詳細(pane_id込み)。
struct TreeClickEvent {
    pane_id: PaneId,
    click: TreeClickEventKind,
}

enum TreeClickEventKind {
    Row(tree_view::RowClick),
    Blank,
}

#[allow(clippy::too_many_arguments)]
fn draw_layout(
    ui: &mut egui::Ui,
    layout: &PaneLayout,
    active: PaneId,
    panes: &mut BTreeMap<PaneId, PaneViewState>,
    snapshots: &BTreeMap<PaneId, Arc<fyler_core::editor::EditorSnapshot>>,
    navigation_entries: &[chrome::NavigationEntry],
    navigation_open: bool,
    navigation_focused: bool,
    navigation_selected: usize,
    statusline_left: &[StatusItem],
    statusline_right: &[StatusItem],
    drag_active: bool,
) -> (
    Option<ImeGeometry>,
    Option<usize>,
    Option<TreeClickEvent>,
    Option<usize>,
) {
    let rect = ui.available_rect_before_wrap();
    ui.allocate_rect(rect, egui::Sense::hover());
    let (content_rect, navigation_clicked) = if navigation_open {
        let rail_width = chrome::NAV_RAIL_WIDTH.min(rect.width());
        let rail_rect = egui::Rect::from_min_max(
            rect.min,
            egui::pos2(rect.left() + rail_width, rect.bottom()),
        );
        let content_rect =
            egui::Rect::from_min_max(egui::pos2(rail_rect.right(), rect.top()), rect.max);
        let clicked = ui
            .scope_builder(egui::UiBuilder::new().max_rect(rail_rect), |ui| {
                chrome::draw_navigation_rail(
                    ui,
                    navigation_entries,
                    navigation_focused,
                    navigation_selected,
                )
            })
            .inner;
        (content_rect, clicked)
    } else {
        (rect, None)
    };
    let mut drop_target_line = None;
    let (ime, tree_click) = draw_layout_in_rect(
        ui,
        content_rect,
        layout,
        active,
        panes,
        snapshots,
        statusline_left,
        statusline_right,
        drag_active,
        &mut drop_target_line,
    );
    (ime, navigation_clicked, tree_click, drop_target_line)
}

#[allow(clippy::too_many_arguments)]
fn draw_layout_in_rect(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    layout: &PaneLayout,
    active: PaneId,
    panes: &mut BTreeMap<PaneId, PaneViewState>,
    snapshots: &BTreeMap<PaneId, Arc<fyler_core::editor::EditorSnapshot>>,
    statusline_left: &[StatusItem],
    statusline_right: &[StatusItem],
    drag_active: bool,
    drop_target_line: &mut Option<usize>,
) -> (Option<ImeGeometry>, Option<TreeClickEvent>) {
    match layout {
        PaneLayout::Leaf(id) => {
            let Some(pane) = panes.get_mut(id) else {
                return (None, None);
            };
            let Some(snapshot) = snapshots.get(id) else {
                return (None, None);
            };
            ui.painter().rect_filled(rect, 0.0, theme::CANVAS);
            ui.painter().rect_stroke(
                rect,
                0.0,
                egui::Stroke::new(1.0, theme::BORDER_SUBTLE),
                egui::StrokeKind::Inside,
            );

            let inner = rect.shrink(1.0);
            let modeline_height = theme::STATUSBAR_HEIGHT;
            let tree_rect = egui::Rect::from_min_max(
                inner.min,
                egui::pos2(
                    inner.max.x,
                    (inner.max.y - modeline_height).max(inner.min.y),
                ),
            );
            let modeline_rect =
                egui::Rect::from_min_max(egui::pos2(inner.min.x, tree_rect.max.y), inner.max);
            let output = ui
                .scope_builder(egui::UiBuilder::new().max_rect(tree_rect), |ui| {
                    if let Some(error) = &pane.engine_error {
                        ui.colored_label(ui.visuals().error_fg_color, error);
                        None
                    } else {
                        Some(tree_view::draw(
                            ui,
                            snapshot,
                            &pane.git_badges,
                            &pane.incomplete_dirs,
                            &pane.collapsed_dirs,
                            &pane.file_infos,
                            pane.tree_viewport,
                            *id,
                            *id == active,
                            *id == active && drag_active,
                        ))
                    }
                })
                .inner;
            ui.scope_builder(egui::UiBuilder::new().max_rect(modeline_rect), |ui| {
                modeline::draw(
                    ui,
                    snapshot,
                    &pane.root,
                    pane.branch.as_deref(),
                    &pane.file_infos,
                    statusline_left,
                    statusline_right,
                    pane.offline,
                    pane.unreadable,
                    pane.engine_error.is_some(),
                );
            });
            if *id != active {
                ui.painter()
                    .rect_filled(rect, 0.0, theme::inactive_pane_veil());
            }
            let Some(output) = output else {
                return (None, None);
            };
            pane.tree_viewport = Some(output.viewport);
            if *id == active {
                *drop_target_line = output.drop_target_line;
            }
            let click = if let Some(row) = output.click {
                Some(TreeClickEvent {
                    pane_id: *id,
                    click: TreeClickEventKind::Row(row),
                })
            } else if output.blank_clicked {
                Some(TreeClickEvent {
                    pane_id: *id,
                    click: TreeClickEventKind::Blank,
                })
            } else {
                None
            };
            let ime = if *id == active
                && matches!(snapshot.mode, Mode::Insert | Mode::Replace | Mode::Cmdline)
            {
                output.cursor_rect.map(|cursor_rect| ImeGeometry {
                    tree_rect: output.tree_rect,
                    cursor_rect,
                })
            } else {
                None
            };
            (ime, click)
        }
        PaneLayout::Split {
            direction,
            ratio,
            first,
            second,
        } => {
            let gap = 3.0;
            let ratio = ratio.clamp(0.0, 1.0);
            let (first_rect, second_rect) = match direction {
                SplitDirection::Horizontal => {
                    let middle = rect.top() + rect.height() * ratio;
                    (
                        egui::Rect::from_min_max(rect.min, egui::pos2(rect.max.x, middle - gap)),
                        egui::Rect::from_min_max(egui::pos2(rect.min.x, middle + gap), rect.max),
                    )
                }
                SplitDirection::Vertical => {
                    let middle = rect.left() + rect.width() * ratio;
                    (
                        egui::Rect::from_min_max(rect.min, egui::pos2(middle - gap, rect.max.y)),
                        egui::Rect::from_min_max(egui::pos2(middle + gap, rect.min.y), rect.max),
                    )
                }
            };
            let (first_ime, first_click) = draw_layout_in_rect(
                ui,
                first_rect,
                first,
                active,
                panes,
                snapshots,
                statusline_left,
                statusline_right,
                drag_active,
                drop_target_line,
            );
            let (second_ime, second_click) = draw_layout_in_rect(
                ui,
                second_rect,
                second,
                active,
                panes,
                snapshots,
                statusline_left,
                statusline_right,
                drag_active,
                drop_target_line,
            );
            (first_ime.or(second_ime), first_click.or(second_click))
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PickerKeys {
    escape: bool,
    previous: bool,
    next: bool,
    enter: bool,
    ctrl_enter: bool,
}

fn read_picker_keys(context: &egui::Context) -> PickerKeys {
    context.input(|input| PickerKeys {
        escape: input.key_pressed(egui::Key::Escape),
        previous: input.key_pressed(egui::Key::ArrowUp)
            || (input.modifiers.ctrl && input.key_pressed(egui::Key::P)),
        next: input.key_pressed(egui::Key::ArrowDown)
            || (input.modifiers.ctrl && input.key_pressed(egui::Key::N)),
        enter: !input.modifiers.ctrl && input.key_pressed(egui::Key::Enter),
        ctrl_enter: input.modifiers.ctrl && input.key_pressed(egui::Key::Enter),
    })
}

/// キー操作を選択indexへ反映し、閉じる場合は確定動作を返す。
///
/// 外側の`Some`はpickerを閉じること、内側の`Some`は候補を確定したことを表す。
fn apply_picker_keys(
    keys: PickerKeys,
    pane_id: PaneId,
    results: &[PickerHit],
    selected: &mut usize,
) -> Option<Option<GuiAction>> {
    if keys.escape {
        return Some(None);
    }
    if results.is_empty() {
        *selected = 0;
        return None;
    }
    if keys.previous {
        *selected = selected.saturating_sub(1);
    }
    if keys.next {
        *selected = (*selected + 1).min(results.len() - 1);
    }
    *selected = (*selected).min(results.len() - 1);

    let action = if keys.ctrl_enter {
        Some(PickerAction::Open)
    } else if keys.enter {
        Some(PickerAction::Jump)
    } else {
        None
    }?;
    let candidate = results.get(*selected)?;
    Some(Some(GuiAction::PickerSelect {
        pane_id,
        path: candidate.path.clone(),
        action,
    }))
}

fn picker_query_action(pane_id: PaneId, query: String) -> GuiAction {
    GuiAction::PickerQuery { pane_id, query }
}

fn picker_completion_action(pane_id: PaneId, selection: Option<GuiAction>) -> GuiAction {
    selection.unwrap_or(GuiAction::PickerClosed { pane_id })
}

#[allow(clippy::too_many_arguments)] // pickerの表示状態をDialogStateと同じ粒度で明示する。
fn draw_file_picker(
    ui: &mut egui::Ui,
    pane_id: PaneId,
    query: &mut String,
    selected: &mut usize,
    results: &[PickerHit],
    indexed_count: usize,
    indexing: bool,
    needs_focus: &mut bool,
) -> (Option<Option<GuiAction>>, Option<GuiAction>) {
    let keys = read_picker_keys(ui.ctx());
    let mut clicked_selection = None;
    let mut query_changed = false;
    egui::Modal::new(egui::Id::new("fyler-file-picker")).show(ui.ctx(), |ui| {
        ui.set_min_width(560.0);
        ui.heading("Find file");
        ui.add_space(6.0);
        let response = ui.add(
            egui::TextEdit::singleline(query)
                .hint_text("Type to filter…")
                .desired_width(f32::INFINITY),
        );
        if *needs_focus {
            response.request_focus();
            *needs_focus = false;
        }
        query_changed = response.changed();
        if query_changed {
            // 入力が変わったら選択を先頭へ戻す。workerの新結果を待たず、この
            // フレームの描画から先頭ハイライトにする。
            *selected = 0;
        }

        ui.add_space(6.0);
        if indexing {
            ui.weak(format!("Indexing… {indexed_count} entries"));
            ui.add_space(4.0);
        }
        egui::ScrollArea::vertical()
            .id_salt("fyler-file-picker-results")
            .max_height(360.0)
            .show(ui, |ui| {
                for (position, candidate) in results.iter().enumerate() {
                    let suffix = if candidate.kind == fyler_core::tree::EntryKind::Dir {
                        "/"
                    } else {
                        ""
                    };
                    let response = ui.selectable_label(
                        position == *selected,
                        format!("{}{suffix}", candidate.display),
                    );
                    if response.clicked() {
                        clicked_selection = Some(position);
                    }
                    if position == *selected {
                        response.scroll_to_me(Some(egui::Align::Center));
                    }
                }
            });
        ui.add_space(6.0);
        ui.weak("↑/↓ or Ctrl-p/Ctrl-n: select   Enter: jump   Ctrl-Enter: open   Esc: close");
    });
    if let Some(position) = clicked_selection {
        *selected = position;
    }
    let query_action = query_changed.then(|| picker_query_action(pane_id, query.clone()));
    (
        apply_picker_keys(keys, pane_id, results, selected),
        query_action,
    )
}

enum FeedbackUiResult {
    Close,
    Submit { kind: FeedbackKind, body: String },
}

fn draw_feedback(
    ui: &mut egui::Ui,
    kind: &mut FeedbackKind,
    body: &mut String,
    stage: &mut FeedbackStage,
    needs_focus: &mut bool,
) -> Option<FeedbackUiResult> {
    let escape = ui.ctx().input(|input| input.key_pressed(egui::Key::Escape));
    if escape {
        return Some(FeedbackUiResult::Close);
    }

    let mut result = None;
    egui::Modal::new(egui::Id::new("fyler-feedback")).show(ui.ctx(), |ui| {
        ui.set_min_width(560.0);
        ui.heading("Anonymous feedback");
        ui.add_space(8.0);
        match *stage {
            FeedbackStage::Input => {
                ui.label("Type");
                ui.horizontal(|ui| {
                    for (number, value) in [
                        ("1", FeedbackKind::Impression),
                        ("2", FeedbackKind::Request),
                        ("3", FeedbackKind::Bug),
                    ] {
                        if ui
                            .selectable_label(
                                *kind == value,
                                format!("{number}: {}", value.display_name()),
                            )
                            .clicked()
                        {
                            *kind = value;
                        }
                    }
                });
                ui.add_space(8.0);
                let response = ui.add(
                    egui::TextEdit::multiline(body)
                        .hint_text("Enter your comment, request, or bug report")
                        .desired_rows(10)
                        .desired_width(f32::INFINITY),
                );
                if *needs_focus {
                    response.request_focus();
                    *needs_focus = false;
                }
                if response.has_focus() {
                    ui.ctx().output_mut(|output| {
                        output.ime = Some(egui::output::IMEOutput {
                            rect: response.rect,
                            cursor_rect: response.rect,
                            should_interrupt_composition: false,
                        });
                    });
                }
                // 本文入力中の数字はテキストとして扱う。ショートカットは非focus時のみ。
                if !response.has_focus() {
                    ui.ctx().input(|input| {
                        if input.key_pressed(egui::Key::Num1) {
                            *kind = FeedbackKind::Impression;
                        } else if input.key_pressed(egui::Key::Num2) {
                            *kind = FeedbackKind::Request;
                        } else if input.key_pressed(egui::Key::Num3) {
                            *kind = FeedbackKind::Bug;
                        }
                    });
                }
                let count = body.chars().count();
                if count > MAX_BODY_CHARS {
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        format!("{count} / {MAX_BODY_CHARS} characters"),
                    );
                } else {
                    ui.weak(format!("{count} / {MAX_BODY_CHARS} characters"));
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Close (Esc)").clicked() {
                        result = Some(FeedbackUiResult::Close);
                    }
                    let valid = validate_body(body).is_ok();
                    if ui.add_enabled(valid, egui::Button::new("Review")).clicked() {
                        *stage = FeedbackStage::Confirm;
                    }
                });
            }
            FeedbackStage::Confirm => {
                ui.label("Content to send");
                ui.separator();
                ui.label(format!("Type: {}", kind.display_name()));
                ui.label(format!("fyler version: {}", env!("CARGO_PKG_VERSION")));
                ui.label(format!("OS: {}", std::env::consts::OS));
                ui.label(format!("arch: {}", std::env::consts::ARCH));
                ui.add_space(6.0);
                egui::ScrollArea::vertical()
                    .id_salt("fyler-feedback-preview")
                    .max_height(240.0)
                    .show(ui, |ui| {
                        ui.label(body.as_str());
                    });
                ui.add_space(8.0);
                ui.label("Anonymous feedback cannot receive an individual reply.");
                ui.label("Your IP address may be processed in transit to operate the service.");
                ui.hyperlink_to(
                    "See docs/PRIVACY.md in the repository for details",
                    "https://github.com/sg004baa/fyler.windows/blob/main/docs/PRIVACY.md",
                );
                ui.hyperlink_to(
                    "GitHub Issues",
                    "https://github.com/sg004baa/fyler.windows/issues",
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Back").clicked() {
                        *stage = FeedbackStage::Input;
                        *needs_focus = true;
                    }
                    if ui.button("Send").clicked() {
                        *stage = FeedbackStage::Sending;
                        result = Some(FeedbackUiResult::Submit {
                            kind: *kind,
                            body: body.clone(),
                        });
                    }
                });
            }
            FeedbackStage::Sending => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Sending…");
                });
                ui.add_space(8.0);
                if ui.button("Cancel").clicked() {
                    result = Some(FeedbackUiResult::Close);
                }
            }
            FeedbackStage::Done(message) => {
                ui.label(message);
                ui.add_space(8.0);
                if ui.button("Close").clicked() {
                    result = Some(FeedbackUiResult::Close);
                }
            }
            FeedbackStage::Failed(_outcome, message) => {
                ui.colored_label(ui.visuals().error_fg_color, message);
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Back to input").clicked() {
                        *stage = FeedbackStage::Input;
                        *needs_focus = true;
                    }
                    if ui.button("Close").clicked() {
                        result = Some(FeedbackUiResult::Close);
                    }
                });
            }
        }
    });
    result
}
fn draw_help(ui: &mut egui::Ui, help_entries: &[HelpEntry]) -> bool {
    let dismiss_from_keyboard = ui.ctx().input(|input| {
        input.key_pressed(egui::Key::Escape)
            || input
                .events
                .iter()
                .any(|event| matches!(event, egui::Event::Text(text) if text == "?"))
    });

    egui::Modal::new(egui::Id::new("fyler-help")).show(ui.ctx(), |ui| {
        ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
            ui.set_min_width(540.0);
            let (header_rect, _) = ui
                .allocate_exact_size(egui::vec2(ui.available_width(), 22.0), egui::Sense::hover());
            ui.painter().text(
                header_rect.left_center(),
                egui::Align2::LEFT_CENTER,
                "Keyboard",
                egui::TextStyle::Heading.resolve(ui.style()),
                theme::TEXT,
            );
            ui.painter().text(
                header_rect.right_center(),
                egui::Align2::RIGHT_CENTER,
                "? / esc",
                egui::FontId::monospace(11.0),
                theme::TEXT_MUTED,
            );
            ui.add_space(6.0);
            ui.separator();
            egui::ScrollArea::vertical()
                .max_height(520.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    egui::Grid::new("fyler-help-commands")
                        .num_columns(2)
                        .min_col_width(150.0)
                        .spacing([24.0, 8.0])
                        .striped(true)
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new("KEY")
                                    .monospace()
                                    .strong()
                                    .size(10.0)
                                    .color(theme::TEXT_MUTED),
                            );
                            ui.label(
                                egui::RichText::new("COMMAND")
                                    .monospace()
                                    .strong()
                                    .size(10.0)
                                    .color(theme::TEXT_MUTED),
                            );
                            ui.end_row();
                            for entry in help_entries {
                                ui.label(
                                    egui::RichText::new(&entry.command)
                                        .monospace()
                                        .strong()
                                        .size(12.0)
                                        .color(theme::BLUE),
                                );
                                ui.label(
                                    egui::RichText::new(&entry.description)
                                        .monospace()
                                        .size(12.0)
                                        .color(theme::TEXT_SECONDARY),
                                );
                                ui.end_row();
                            }
                        });
                });
        });
    });
    dismiss_from_keyboard
}

/// [`draw_tree_context_menu`] の描画結果。
enum TreeContextOutcome {
    /// まだ何も選ばれていない(閉じてもいない)。
    Pending,
    /// 項目がクリックされた。
    Chosen(TreeContextItem),
    /// outside click / Escapeで閉じられた(項目は選ばれていない)。
    Dismissed,
}

/// context menuに表示する項目(表示順)。
const TREE_CONTEXT_ITEMS: [TreeContextItem; 6] = [
    TreeContextItem::Open,
    TreeContextItem::OpenWith,
    TreeContextItem::Rename,
    TreeContextItem::MarkForDeletion,
    TreeContextItem::CopyPath,
    TreeContextItem::OpenTerminal,
];

fn context_item_label(item: TreeContextItem, is_dir: bool) -> &'static str {
    match item {
        TreeContextItem::Open => {
            if is_dir {
                "Enter directory"
            } else {
                "Open"
            }
        }
        TreeContextItem::OpenWith => "Open with...",
        TreeContextItem::Rename => "Rename",
        TreeContextItem::MarkForDeletion => "Mark for deletion",
        TreeContextItem::CopyPath => "Copy path",
        TreeContextItem::OpenTerminal => "Open terminal here",
    }
}

/// context menu項目の有効/無効判定(純ロジック。unit test対象)。
///
/// `has_id` / `is_dir` / `offline` はGUIがクリック時点で確定させた値、
/// `engine_ok` はpaneのengine健全性(crashed=false)。dirty・保存中などの
/// 権威判定はapp層の最終防衛(rejection helper)に委ねる(ここでは行わない)。
fn context_item_enabled(
    item: TreeContextItem,
    has_id: bool,
    is_dir: bool,
    engine_ok: bool,
    offline: bool,
) -> bool {
    if !engine_ok {
        return false;
    }
    match item {
        TreeContextItem::Open | TreeContextItem::Rename | TreeContextItem::CopyPath => has_id,
        TreeContextItem::OpenWith => has_id && !is_dir && !offline,
        TreeContextItem::MarkForDeletion => true,
        TreeContextItem::OpenTerminal => has_id && !offline,
    }
}

/// ツリー右clickのcontext menuを描画する。
///
/// gatingは呼び出し側がクリック時点で確定させた `has_id` / `is_dir` /
/// `engine_ok` / `offline`(GUIが確実に知っている状態)だけで行う。
/// dirty・保存中などの権威判定はapp層の最終防衛(rejection helper)に委ねる。
fn draw_tree_context_menu(
    ui: &mut egui::Ui,
    pos: egui::Pos2,
    has_id: bool,
    is_dir: bool,
    engine_ok: bool,
    offline: bool,
) -> TreeContextOutcome {
    let mut chosen = None;
    let popup = egui::Popup::new(
        egui::Id::new("fyler-tree-context-menu"),
        ui.ctx().clone(),
        pos,
        ui.layer_id(),
    )
    .kind(egui::PopupKind::Menu)
    .layout(egui::Layout::top_down_justified(egui::Align::Min))
    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
    .open(true)
    .show(|ui| {
        for item in TREE_CONTEXT_ITEMS {
            let enabled = context_item_enabled(item, has_id, is_dir, engine_ok, offline);
            let label = context_item_label(item, is_dir);
            let response = ui.add_enabled(enabled, egui::Button::new(label));
            let response = if enabled {
                response
            } else {
                response.on_disabled_hover_text(context_item_disabled_reason(
                    engine_ok, has_id, is_dir, offline,
                ))
            };
            if response.clicked() {
                chosen = Some(item);
            }
        }
    });

    match popup {
        None => TreeContextOutcome::Dismissed,
        Some(response) => {
            if let Some(item) = chosen {
                TreeContextOutcome::Chosen(item)
            } else if response.response.should_close() {
                TreeContextOutcome::Dismissed
            } else {
                TreeContextOutcome::Pending
            }
        }
    }
}

/// disableされた項目のtooltip文言。優先度: crashed > offline > is_dir(open-with専用) > has_id。
fn context_item_disabled_reason(
    engine_ok: bool,
    has_id: bool,
    is_dir: bool,
    offline: bool,
) -> &'static str {
    if !engine_ok {
        "Editor engine has stopped"
    } else if offline {
        "Location is offline"
    } else if is_dir {
        "Open with is for files only"
    } else if !has_id {
        "This entry has not been saved yet"
    } else {
        ""
    }
}

fn current_window_geometry(context: &egui::Context) -> Option<WindowGeometry> {
    context.input(|input| window_geometry_from_viewport(input.viewport()))
}

fn window_geometry_from_viewport(viewport: &egui::ViewportInfo) -> Option<WindowGeometry> {
    let inner = viewport.inner_rect?;
    let outer = viewport.outer_rect?;
    WindowGeometry::new(
        inner.width(),
        inner.height(),
        outer.min.x,
        outer.min.y,
        viewport.maximized.unwrap_or(false),
    )
}

fn initial_window_geometry(monitor_size: Option<egui::Vec2>) -> Option<(egui::Vec2, egui::Pos2)> {
    let monitor_size = monitor_size?;
    if !monitor_size.is_finite() || monitor_size.x <= 0.0 || monitor_size.y <= 0.0 {
        return None;
    }
    let size = monitor_size * INITIAL_WINDOW_SCALE;
    let margin = (monitor_size - size) * 0.5;
    Some((size, egui::pos2(margin.x, margin.y)))
}

fn native_options(window: Option<WindowGeometry>) -> eframe::NativeOptions {
    let viewport = window
        .map_or_else(egui::ViewportBuilder::default, |window| {
            egui::ViewportBuilder::default()
                .with_inner_size([window.inner_width, window.inner_height])
                .with_position([window.outer_x, window.outer_y])
                .with_maximized(window.maximized)
        })
        .with_decorations(false)
        .with_min_inner_size([720.0, 480.0])
        .with_title("fyler");
    eframe::NativeOptions {
        viewport,
        // Geometry is persisted in session.toml so it shares the same normal-shutdown contract
        // as pane state and does not depend on eframe's optional persistence feature.
        persist_window: false,
        ..Default::default()
    }
}

/// GUIを起動する(メインスレッドで呼ぶこと。eframeの制約)。
///
/// 実装契約(M1):
/// - `eframe::run_native` で [`FylerApp`] を起動する
/// - エンジンのイベント(`EditorEvent`)受信で `ctx.request_repaint()` を呼び、
///   ポーリングなしで再描画されるようにする
pub fn run(
    event_rx: mpsc::Receiver<GuiEvent>,
    action_tx: mpsc::Sender<GuiAction>,
    gui_options: GuiOptions,
    event_dequeued: Arc<dyn Fn() + Send + Sync>,
    initial_window: Option<WindowGeometry>,
    window_geometry: Arc<Mutex<Option<WindowGeometry>>>,
) -> anyhow::Result<()> {
    let native_options = native_options(initial_window);
    eframe::run_native(
        "fyler",
        native_options,
        Box::new(move |creation_context| {
            let GuiOptions {
                confirm_detail,
                font_path,
                help_entries,
                dock_focus_bindings,
                bookmarks,
                recent_roots,
                drives,
                statusline_left,
                statusline_right,
            } = gui_options;
            let resize_to_monitor_on_first_frame = initial_window.is_none();
            theme::install(&creation_context.egui_ctx);
            install_fallback_font(&creation_context.egui_ctx, font_path.as_deref());
            let mut app = FylerApp::new(
                event_rx,
                action_tx,
                AppOptions {
                    confirm_detail,
                    help_entries,
                    dock_focus_bindings,
                    bookmarks,
                    recent_roots,
                    drives,
                    statusline_left,
                    statusline_right,
                },
                creation_context.egui_ctx.clone(),
                event_dequeued,
            )
            .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { error.into() })?;
            app.resize_to_monitor_on_first_frame = resize_to_monitor_on_first_frame;
            app.window_geometry = window_geometry;
            Ok(Box::new(app))
        }),
    )
    .map_err(|error| anyhow::anyhow!("Failed to start GUI: {error}"))
}

/// 組み込みフォント(Moralerspace Argon HW)を常に登録する。
///
/// - テキストファミリ(Monospace / Proportional): `config.font`があれば
///   ユーザーフォントを先頭、組み込みフォントをその次(JP・アイコンのfallback)に
///   置く。なければ組み込みフォントが先頭。eguiの既定フォントはその後ろへ残す。
/// - アイコン専用ファミリ [`icon::font_family`] には組み込みフォントだけを入れ、
///   `config.font`に左右されずアイコンが同一に描画されるようにする。
///
/// ユーザーフォントの読み込みに失敗しても起動は止めず、組み込みフォントへfallbackする。
fn install_fallback_font(context: &egui::Context, configured: Option<&Path>) {
    const BUILTIN_FONT_BYTES: &[u8] =
        include_bytes!("../assets/fonts/MoralerspaceArgonHW-Regular.ttf");

    let mut definitions = egui::FontDefinitions::default();
    definitions.font_data.insert(
        BUILTIN_FONT_NAME.to_owned(),
        Arc::new(egui::FontData::from_static(BUILTIN_FONT_BYTES)),
    );

    // ユーザー指定フォントは実行時に読み込む(絶対パス。config読み込み時に検証済み)。
    // 読み込めなければ組み込みフォントだけで続行する。
    let user_font = configured
        .and_then(|path| fs::read(path).ok())
        .map(|bytes| {
            let name = "fyler-user".to_owned();
            definitions
                .font_data
                .insert(name.clone(), Arc::new(egui::FontData::from_owned(bytes)));
            name
        });

    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        let entry = definitions.families.entry(family).or_default();
        // 既定フォントの前へ、組み込み(+あればユーザー)フォントを差し込む。
        entry.insert(0, BUILTIN_FONT_NAME.to_owned());
        if let Some(name) = &user_font {
            entry.insert(0, name.clone());
        }
    }

    // アイコン専用ファミリは組み込みフォントだけ。config.fontの影響を受けない。
    definitions
        .families
        .insert(icon::font_family(), vec![BUILTIN_FONT_NAME.to_owned()]);

    context.set_fonts(definitions);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RecordingEngine(Arc<fyler_core::editor::EditorSnapshot>);

    impl EditorEngine for RecordingEngine {
        fn send(&self, _cmd: fyler_core::editor::EditorCommand) -> anyhow::Result<()> {
            Ok(())
        }

        fn snapshot(&self) -> Arc<fyler_core::editor::EditorSnapshot> {
            Arc::clone(&self.0)
        }
    }

    fn recording_engine() -> Arc<dyn EditorEngine> {
        Arc::new(RecordingEngine(Arc::new(
            fyler_core::editor::EditorSnapshot::empty(),
        )))
    }

    fn candidate(path: &str, kind: fyler_core::tree::EntryKind) -> PickerHit {
        PickerHit {
            path: TreePath::parse(path),
            kind,
            display: path.to_owned(),
        }
    }

    fn empty_test_app() -> (FylerApp, mpsc::Sender<GuiEvent>, mpsc::Receiver<GuiAction>) {
        let (event_tx, event_rx) = mpsc::channel();
        let (action_tx, action_rx) = mpsc::channel();
        (
            FylerApp {
                panes: BTreeMap::new(),
                layout: None,
                active: None,
                event_rx,
                event_dequeued: Arc::new(|| {}),
                cmdline: None,
                popupmenu: None,
                message: None,
                pending_copy: None,
                fatal_error: None,
                dialog: None,
                action_tx,
                picker_needs_focus: false,
                feedback_needs_focus: false,
                confirm_detail: ConfirmDetail::Full,
                help_entries: Vec::new(),
                dock_focus_bindings: Vec::new(),
                navigation_dock: NavigationDockState::visible(),
                bookmarks: Vec::new(),
                recent_roots: Vec::new(),
                drives: Vec::new(),
                statusline_left: Vec::new(),
                statusline_right: Vec::new(),
                resize_to_monitor_on_first_frame: false,
                window_geometry: Arc::new(Mutex::new(None)),
            },
            event_tx,
            action_rx,
        )
    }

    #[test]
    fn pane_tagged_events_update_only_their_own_view_state() {
        let first = PaneId::new(1);
        let second = PaneId::new(2);
        let (_event_tx, event_rx) = mpsc::channel();
        let (action_tx, _action_rx) = mpsc::channel();
        let mut app = FylerApp {
            panes: BTreeMap::from([
                (
                    first,
                    PaneViewState {
                        engine: recording_engine(),
                        root: PathBuf::from("first"),
                        git_badges: HashMap::new(),
                        branch: None,
                        incomplete_dirs: HashSet::new(),
                        offline: false,
                        unreadable: 0,
                        file_infos: HashMap::new(),
                        collapsed_dirs: HashSet::new(),
                        engine_error: None,
                        tree_viewport: None,
                        can_go_back: false,
                        can_go_forward: false,
                    },
                ),
                (
                    second,
                    PaneViewState {
                        engine: recording_engine(),
                        root: PathBuf::from("second"),
                        git_badges: HashMap::new(),
                        branch: None,
                        incomplete_dirs: HashSet::new(),
                        offline: false,
                        unreadable: 0,
                        file_infos: HashMap::new(),
                        collapsed_dirs: HashSet::new(),
                        engine_error: None,
                        tree_viewport: None,
                        can_go_back: false,
                        can_go_forward: false,
                    },
                ),
            ]),
            layout: Some(
                PaneLayout::leaf(first)
                    .split(first, SplitDirection::Vertical, second)
                    .unwrap(),
            ),
            active: Some(first),
            event_rx,
            event_dequeued: Arc::new(|| {}),
            cmdline: None,
            popupmenu: None,
            message: None,
            pending_copy: None,
            fatal_error: None,
            dialog: None,
            action_tx,
            picker_needs_focus: false,
            feedback_needs_focus: false,
            confirm_detail: ConfirmDetail::Full,
            help_entries: Vec::new(),
            dock_focus_bindings: Vec::new(),
            navigation_dock: NavigationDockState::visible(),
            bookmarks: Vec::new(),
            recent_roots: Vec::new(),
            drives: Vec::new(),
            statusline_left: Vec::new(),
            statusline_right: Vec::new(),
            resize_to_monitor_on_first_frame: false,
            window_geometry: Arc::new(Mutex::new(None)),
        };
        let (tx, rx) = mpsc::channel();
        app.event_rx = rx;
        tx.send(GuiEvent::RootChanged {
            pane_id: second,
            root: PathBuf::from("changed"),
        })
        .unwrap();
        tx.send(GuiEvent::GitBadges {
            pane_id: second,
            branch: Some("main".to_owned()),
            badges: HashMap::from([(EntryId(9), GitBadge::Modified)]),
        })
        .unwrap();
        tx.send(GuiEvent::IncompleteDirs {
            pane_id: second,
            dirs: HashSet::from([EntryId(10)]),
        })
        .unwrap();
        tx.send(GuiEvent::PaneHealth {
            pane_id: second,
            offline: true,
            unreadable: 2,
        })
        .unwrap();
        app.receive_events();

        assert_eq!(app.panes[&first].root, PathBuf::from("first"));
        assert!(app.panes[&first].git_badges.is_empty());
        assert_eq!(app.panes[&second].root, PathBuf::from("changed"));
        assert_eq!(app.recent_roots, [PathBuf::from("changed")]);
        assert_eq!(
            app.panes[&second].git_badges.get(&EntryId(9)),
            Some(&GitBadge::Modified)
        );
        assert!(app.panes[&second].incomplete_dirs.contains(&EntryId(10)));
        assert!(app.panes[&second].offline);
        assert_eq!(app.panes[&second].unreadable, 2);
        assert!(!app.panes[&first].offline);
    }

    #[test]
    fn context_item_enabled_requires_saved_entry_for_id_gated_items() {
        for item in [
            TreeContextItem::Open,
            TreeContextItem::Rename,
            TreeContextItem::CopyPath,
            TreeContextItem::OpenWith,
            TreeContextItem::OpenTerminal,
        ] {
            assert!(!context_item_enabled(item, false, false, true, false));
        }
        // Mark for deletionはIDなし(unsaved)行でも有効。
        assert!(context_item_enabled(
            TreeContextItem::MarkForDeletion,
            false,
            false,
            true,
            false
        ));
    }

    #[test]
    fn context_item_enabled_disables_everything_when_engine_crashed() {
        for item in TREE_CONTEXT_ITEMS {
            assert!(!context_item_enabled(item, true, false, false, false));
        }
    }

    #[test]
    fn context_item_enabled_open_with_is_files_only_and_requires_online() {
        assert!(context_item_enabled(
            TreeContextItem::OpenWith,
            true,
            false,
            true,
            false
        ));
        assert!(!context_item_enabled(
            TreeContextItem::OpenWith,
            true,
            true,
            true,
            false
        ));
        assert!(!context_item_enabled(
            TreeContextItem::OpenWith,
            true,
            false,
            true,
            true
        ));
    }

    #[test]
    fn context_item_enabled_open_terminal_requires_online_but_allows_directories() {
        assert!(context_item_enabled(
            TreeContextItem::OpenTerminal,
            true,
            true,
            true,
            false
        ));
        assert!(!context_item_enabled(
            TreeContextItem::OpenTerminal,
            true,
            true,
            true,
            true
        ));
    }

    #[test]
    fn context_item_enabled_rename_and_copy_path_ignore_offline() {
        assert!(context_item_enabled(
            TreeContextItem::Rename,
            true,
            false,
            true,
            true
        ));
        assert!(context_item_enabled(
            TreeContextItem::CopyPath,
            true,
            false,
            true,
            true
        ));
    }

    #[test]
    fn context_item_label_reflects_directory_vs_file_for_open() {
        assert_eq!(
            context_item_label(TreeContextItem::Open, true),
            "Enter directory"
        );
        assert_eq!(context_item_label(TreeContextItem::Open, false), "Open");
    }

    #[test]
    fn navigation_dock_uses_j_k_enter_and_returns_focus_with_configured_binding() {
        let entries = chrome::navigation_entries(
            Path::new("/current"),
            &[
                ("docs".to_owned(), PathBuf::from("/docs")),
                ("src".to_owned(), PathBuf::from("/src")),
            ],
            &[],
            &[],
        );
        let mut state = NavigationDockState {
            focused: true,
            ..NavigationDockState::visible()
        };
        let target = handle_navigation_keys(
            &mut state,
            [
                KeyInput {
                    key: Key::Char('j'),
                    mods: Modifiers::default(),
                },
                KeyInput {
                    key: Key::Enter,
                    mods: Modifiers::default(),
                },
            ],
            &[],
            &entries,
        );
        assert_eq!(state.selected, 1);
        assert_eq!(target, Some(PathBuf::from("/src")));
        assert!(!state.focused);

        state.focused = true;
        let binding = fyler_core::keymap::parse_key_sequence("x e", None).unwrap();
        assert_eq!(
            handle_navigation_keys(&mut state, binding.0.clone(), &[binding], &entries),
            None
        );
        assert!(!state.focused);
    }

    #[test]
    fn navigation_dock_escape_returns_focus_without_closing() {
        let entries = chrome::navigation_entries(
            Path::new("/current"),
            &[("src".to_owned(), PathBuf::from("/src"))],
            &[],
            &[],
        );
        let mut state = NavigationDockState {
            focused: true,
            ..NavigationDockState::visible()
        };
        let target = handle_navigation_keys(
            &mut state,
            [KeyInput {
                key: Key::Esc,
                mods: Modifiers::default(),
            }],
            &[],
            &entries,
        );

        assert_eq!(target, None);
        assert!(state.open);
        assert!(!state.focused);
    }

    #[test]
    fn configured_dock_binding_wins_over_navigation_and_recovers_after_mismatch() {
        let entries = chrome::navigation_entries(Path::new("/current"), &[], &[], &[]);
        let binding = fyler_core::keymap::parse_key_sequence("j", None).unwrap();
        let mut state = NavigationDockState {
            focused: true,
            ..NavigationDockState::visible()
        };
        handle_navigation_keys(
            &mut state,
            binding.0.clone(),
            std::slice::from_ref(&binding),
            &entries,
        );
        assert!(!state.focused);
        assert_eq!(state.selected, 0);

        let binding = fyler_core::keymap::parse_key_sequence("x e", None).unwrap();
        let mut state = NavigationDockState {
            focused: true,
            ..NavigationDockState::visible()
        };
        let keys = [
            KeyInput {
                key: Key::Char('q'),
                mods: Modifiers::default(),
            },
            KeyInput {
                key: Key::Char('x'),
                mods: Modifiers::default(),
            },
            KeyInput {
                key: Key::Char('e'),
                mods: Modifiers::default(),
            },
        ];
        handle_navigation_keys(&mut state, keys, &[binding], &entries);
        assert!(!state.focused);
        assert!(state.pending_binding.is_empty());
    }

    #[test]
    fn dock_focus_event_opens_and_focuses_a_closed_navigation_dock() {
        let (mut app, event_tx, _action_rx) = empty_test_app();
        app.navigation_dock = NavigationDockState::default();
        event_tx
            .send(GuiEvent::Editor {
                pane_id: PaneId::new(1),
                event: EditorEvent::ToggleDockFocus,
            })
            .unwrap();

        app.receive_events();

        assert!(app.navigation_dock.open);
        assert!(app.navigation_dock.focused);
        assert_eq!(app.navigation_dock.selected, 0);
    }

    #[test]
    fn dock_toggle_cycles_closed_focused_and_hidden() {
        let mut state = NavigationDockState::default();
        // closed → open + focus
        state.toggle_focus();
        assert!(state.open && state.focused);
        // open + focused → closed (hidden)
        state.toggle_focus();
        assert!(!state.open && !state.focused);
        // closed → open + focus
        state.toggle_focus();
        assert!(state.open && state.focused);

        // open + unfocused → focus
        state.focused = false;
        state.toggle_focus();
        assert!(state.open && state.focused);
    }

    #[test]
    fn picker_opens_immediately_and_accepts_progressive_results() {
        let pane = PaneId::new(3);
        let (mut app, event_tx, action_rx) = empty_test_app();
        event_tx
            .send(GuiEvent::ShowFilePicker { pane_id: pane })
            .unwrap();
        app.receive_events();
        assert!(matches!(
            app.dialog,
            Some(DialogState::FilePicker {
                indexing: true,
                indexed_count: 0,
                ..
            })
        ));
        assert_eq!(
            action_rx.try_recv().unwrap(),
            GuiAction::PickerQuery {
                pane_id: pane,
                query: String::new(),
            }
        );

        event_tx
            .send(GuiEvent::PickerResults {
                pane_id: pane,
                query: String::new(),
                results: vec![candidate("README.md", fyler_core::tree::EntryKind::File)],
                indexed_count: 42,
                indexing: true,
            })
            .unwrap();
        app.receive_events();
        assert!(matches!(
            app.dialog,
            Some(DialogState::FilePicker {
                ref results,
                indexed_count: 42,
                indexing: true,
                ..
            }) if results[0].display == "README.md"
        ));
    }

    #[test]
    fn picker_keys_close_move_jump_and_open() {
        let pane = PaneId::new(3);
        let results = vec![
            candidate("first", fyler_core::tree::EntryKind::File),
            candidate("second", fyler_core::tree::EntryKind::File),
        ];
        let mut selected = 0;

        assert_eq!(
            apply_picker_keys(
                PickerKeys {
                    next: true,
                    ..Default::default()
                },
                pane,
                &results,
                &mut selected,
            ),
            None
        );
        assert_eq!(selected, 1);
        assert_eq!(
            apply_picker_keys(
                PickerKeys {
                    previous: true,
                    ..Default::default()
                },
                pane,
                &results,
                &mut selected,
            ),
            None
        );
        assert_eq!(selected, 0);
        assert_eq!(
            apply_picker_keys(
                PickerKeys {
                    enter: true,
                    ..Default::default()
                },
                pane,
                &results,
                &mut selected,
            ),
            Some(Some(GuiAction::PickerSelect {
                pane_id: pane,
                path: TreePath::parse("first"),
                action: PickerAction::Jump,
            }))
        );
        assert_eq!(
            apply_picker_keys(
                PickerKeys {
                    ctrl_enter: true,
                    ..Default::default()
                },
                pane,
                &results,
                &mut selected,
            ),
            Some(Some(GuiAction::PickerSelect {
                pane_id: pane,
                path: TreePath::parse("first"),
                action: PickerAction::Open,
            }))
        );
        assert_eq!(
            apply_picker_keys(
                PickerKeys {
                    escape: true,
                    ..Default::default()
                },
                pane,
                &results,
                &mut selected,
            ),
            Some(None)
        );
        assert_eq!(
            picker_completion_action(pane, None),
            GuiAction::PickerClosed { pane_id: pane }
        );
        assert_eq!(
            picker_query_action(pane, "read".to_owned()),
            GuiAction::PickerQuery {
                pane_id: pane,
                query: "read".to_owned(),
            }
        );
    }

    #[test]
    fn picker_dialog_blocks_editor_input_forwarding() {
        assert!(should_forward_input(true, true, true));
        assert!(!should_forward_input(false, true, true));
    }

    #[test]
    fn picker_closes_when_its_pane_is_removed_or_crashes() {
        let pane = PaneId::new(7);
        for closing_event in [
            GuiEvent::RemovePane(pane),
            GuiEvent::Editor {
                pane_id: pane,
                event: EditorEvent::EngineCrashed {
                    reason: "test".to_owned(),
                },
            },
        ] {
            let (mut app, event_tx, _action_rx) = empty_test_app();
            event_tx
                .send(GuiEvent::AddPane {
                    pane_id: pane,
                    engine: recording_engine(),
                    root: PathBuf::from("root"),
                })
                .unwrap();
            event_tx
                .send(GuiEvent::ShowFilePicker { pane_id: pane })
                .unwrap();
            app.receive_events();
            assert!(matches!(app.dialog, Some(DialogState::FilePicker { .. })));

            event_tx.send(closing_event).unwrap();
            app.receive_events();

            assert!(app.dialog.is_none());
        }
    }

    #[test]
    fn feedback_result_is_only_accepted_while_sending() {
        let (mut app, event_tx, _action_rx) = empty_test_app();
        event_tx.send(GuiEvent::ShowFeedback).unwrap();
        event_tx
            .send(GuiEvent::FeedbackResult {
                outcome: FeedbackResultKind::Accepted,
                message: "accepted",
            })
            .unwrap();
        app.receive_events();
        assert!(matches!(
            app.dialog,
            Some(DialogState::Feedback {
                stage: FeedbackStage::Input,
                ..
            })
        ));

        if let Some(DialogState::Feedback { stage, .. }) = &mut app.dialog {
            *stage = FeedbackStage::Sending;
        }
        event_tx
            .send(GuiEvent::FeedbackResult {
                outcome: FeedbackResultKind::Accepted,
                message: "accepted",
            })
            .unwrap();
        app.receive_events();
        assert!(matches!(
            app.dialog,
            Some(DialogState::Feedback {
                stage: FeedbackStage::Done("accepted"),
                ..
            })
        ));

        app.dialog = None;
        event_tx
            .send(GuiEvent::FeedbackResult {
                outcome: FeedbackResultKind::ServerError,
                message: "failed",
            })
            .unwrap();
        app.receive_events();
        assert!(app.dialog.is_none());
    }
    #[test]
    fn window_geometry_roundtrips_through_native_options() {
        let (size, position) = initial_window_geometry(Some(egui::vec2(1920.0, 1080.0))).unwrap();
        assert_eq!(size, egui::vec2(1344.0, 756.0));
        assert_eq!(position, egui::pos2(288.0, 162.0));
        assert!(initial_window_geometry(None).is_none());
        assert!(initial_window_geometry(Some(egui::Vec2::ZERO)).is_none());

        let geometry = WindowGeometry::new(1200.0, 700.0, 30.0, 40.0, true).unwrap();
        let options = native_options(Some(geometry));
        assert_eq!(options.viewport.inner_size, Some(egui::vec2(1200.0, 700.0)));
        assert_eq!(options.viewport.position, Some(egui::pos2(30.0, 40.0)));
        assert_eq!(options.viewport.maximized, Some(true));
        assert!(!options.persist_window);
    }

    #[test]
    fn viewport_geometry_capture_requires_size_and_position() {
        let viewport = egui::ViewportInfo {
            inner_rect: Some(egui::Rect::from_min_size(
                egui::pos2(35.0, 65.0),
                egui::vec2(1000.0, 600.0),
            )),
            outer_rect: Some(egui::Rect::from_min_size(
                egui::pos2(30.0, 40.0),
                egui::vec2(1010.0, 630.0),
            )),
            maximized: Some(false),
            ..Default::default()
        };
        assert_eq!(
            window_geometry_from_viewport(&viewport),
            WindowGeometry::new(1000.0, 600.0, 30.0, 40.0, false)
        );
        assert!(window_geometry_from_viewport(&egui::ViewportInfo::default()).is_none());
    }
}
