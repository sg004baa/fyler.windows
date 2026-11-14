//! eframeアプリ本体。毎フレーム、エンジンのsnapshotだけを描画する。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use eframe::egui;
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
use fyler_core::window::{
    NativeCaptionLayout, NativeWindowCaption, NativeWindowCaptionFactory, PhysicalRect,
};
use fyler_core::{StartupWindow, WindowGeometry};
#[cfg(windows)]
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

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
    /// 管理者権限(elevated)でfileを開く(UAC確認はOS側)。
    OpenAsAdmin,
    /// buffer上で名前編集を開始するだけ(実FSは変更しない)。
    Rename,
    /// bufferから行を除去するだけ(実FSは変更しない)。
    MarkForDeletion,
    /// OSクリップボードへcopyとして載せる(実FSはPaste確認後)。
    ClipboardCopy,
    /// OSクリップボードへcutとして載せる(実FSはPaste確認後)。
    ClipboardCut,
    /// OSクリップボードの内容をこの場所へ貼り付ける(確認後に実FS反映)。
    ClipboardPaste,
    /// .zipアーカイブをその場に展開する(確認後)。
    Extract,
    /// エントリへの.lnkショートカットを同じ場所に作成する。
    CreateShortcut,
    CopyPath,
    OpenTerminal,
    /// paneのツリーを再読込する。
    Refresh,
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
    /// GUI window内で開始したtree行dragがwindow境界を離れた。app層がOLE
    /// drag(Explorer等の外部Shellターゲットへのdrag-and-drop)へ移行する。
    TreeDragOut {
        pane_id: PaneId,
        line: usize,
    },
    /// GUI window内で完結したtree行drag(pane間)。`copy`はCtrl押下時true。
    TreeDragDrop {
        source_pane: PaneId,
        source_line: usize,
        target_pane: PaneId,
        target_line: Option<usize>,
        copy: bool,
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
    ReloadOptions(Box<GuiOptions>),
    Editor {
        pane_id: PaneId,
        event: EditorEvent,
    },
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
    /// 汎用の承認確認ダイアログを表示する(shortcut作成・zip展開の確認)。
    /// 行はapp層で整形済み。承認は`GuiAction::Confirm(Approve)`で返る。
    ShowActionConfirm {
        /// ダイアログ見出し(英語)。
        title: String,
        /// 承認ボタンのラベル(英語、"(y)"付き)。
        approve_label: String,
        lines: Vec<String>,
    },
    /// extract applyワーカーから届いた操作単位の進捗を表示へ反映する。
    /// ラベルはapp層で整形済み。
    ExtractProgress(ApplyProgress<String>),
    /// 汎用の結果ダイアログを表示する(zip展開結果)。行はapp層で整形済み。
    /// `NG`で始まる行はエラー色で描画する。
    ShowActionReport {
        title: String,
        lines: Vec<String>,
        any_failed: bool,
    },
    /// OLE drag-out完了後、Move報告でsourceが残存している場合の後始末
    /// (ごみ箱退避)確認ダイアログを表示する。
    ShowDragCleanupConfirm {
        paths: Vec<PathBuf>,
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
    ActionConfirm {
        title: String,
        approve_label: String,
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
    ActionReport {
        title: String,
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
    Help {
        query: String,
        searching: bool,
    },
    /// ツリーの右click context menu(GUI local。app層の往復を待たず即表示する)。
    TreeContext {
        pane_id: PaneId,
        line: usize,
        /// 表示位置(右clickのscreen座標)。
        pos: egui::Pos2,
        has_id: bool,
        is_dir: bool,
        /// 行が.zipアーカイブのファイルか(Extract gating用)。
        is_zip: bool,
        /// 表示中のpaneのengineが健全か(crashed=false)。
        engine_ok: bool,
        /// 表示中のpaneのrootがoffline(到達不能)か。
        offline: bool,
    },
    /// OLE drag-out完了後、Move報告でsourceが残存している場合の後始末確認。
    DragCleanupConfirm {
        paths: Vec<PathBuf>,
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

/// GUI window内で開始したtree行drag(pane間dragを含む)のセッション。
///
/// fylerのwindow**内**でのdragはOLEを使わずegui内で完結する。windowの境界を
/// 離れた時点でのみ`GuiAction::TreeDragOut`を送りOLE dragへ移行する
/// (親セッション設計確定事項)。
///
/// v1は単一行のみを対象とする。visual選択中の複数行dragへの拡張は、
/// 選択範囲を実entry行の一覧へ解決する既存ヘルパーがこの層に露出しておらず
/// (app層の`resolve_selection`はTreePath解決までを担い、GUI層はsnapshotの
/// 生selectionしか持たない)、境界越え検知と二重化して1turnで安全に実装する
/// 判断ができなかったため見送った。
#[derive(Debug, Clone, Copy)]
struct TreeDragSession {
    pane_id: PaneId,
    line: usize,
}

/// 1フレームでの内部tree drag描画結果を集約する。
#[derive(Default)]
struct DragCollect {
    /// このフレームで最初に開始された行drag(pane_id込み)。
    started: Option<(PaneId, tree_view::RowDragStart)>,
    /// internal drag中、全paneのdrop先候補(pane_id, tree領域, drop対象行)。
    rects: Vec<(PaneId, egui::Rect, Option<usize>)>,
}

/// internal tree drag中、pointerがwindow境界(viewport)を離れたかを判定する
/// 純ロジック。座標不明(`None`)は離脱と誤判定しない。
fn pointer_left_window(pointer: Option<egui::Pos2>, viewport: egui::Rect) -> bool {
    match pointer {
        Some(pos) => !viewport.contains(pos),
        None => false,
    }
}

/// internal tree drag中、pointer位置からdrop先pane・行を解決する純ロジック。
/// 同一paneへのdrop(`source_pane`と一致)は「何もしない」ため`None`を返す。
fn resolve_drag_drop_target(
    pointer: egui::Pos2,
    source_pane: PaneId,
    rects: &[(PaneId, egui::Rect, Option<usize>)],
) -> Option<(PaneId, Option<usize>)> {
    let (pane_id, _, line) = rects.iter().find(|(_, rect, _)| rect.contains(pointer))?;
    (*pane_id != source_pane).then_some((*pane_id, *line))
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
    /// 起動直後、保存位置と実際の配置(`ViewportInfo::outer_rect`)を最大1回だけ
    /// 比較して`ViewportCommand::OuterPosition`で補正するための状態。
    /// `None`は「補正の必要がない、または判定・補正済み」を表す。
    position_correction: Option<PositionCorrection>,
    /// GUI window内で進行中のtree行drag(pane間dragを含む)。
    /// windowの境界を離れたら`GuiAction::TreeDragOut`を送って`None`へ戻す。
    tree_drag: Option<TreeDragSession>,
    pending_font: Option<Option<PathBuf>>,
    viewport_close_requested: bool,
    viewport_maximize_requested: bool,
    viewport_minimize_requested: bool,
    native_caption: Option<Box<dyn NativeWindowCaption>>,
    native_caption_layout: Option<NativeCaptionLayout>,
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
    center_line: Option<usize>,
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
        native_caption: Option<Box<dyn NativeWindowCaption>>,
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
            position_correction: None,
            tree_drag: None,
            pending_font: None,
            viewport_close_requested: false,
            viewport_maximize_requested: false,
            viewport_minimize_requested: false,
            native_caption,
            native_caption_layout: None,
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
                            center_line: None,
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
                GuiEvent::ReloadOptions(options) => {
                    let GuiOptions {
                        confirm_detail,
                        font_path,
                        help_entries,
                        dock_focus_bindings,
                        bookmarks,
                        recent_roots: _,
                        drives,
                        statusline_left,
                        statusline_right,
                    } = *options;
                    self.confirm_detail = confirm_detail;
                    self.help_entries = help_entries;
                    self.dock_focus_bindings = dock_focus_bindings;
                    self.bookmarks = bookmarks;
                    self.drives = drives;
                    self.statusline_left = statusline_left;
                    self.statusline_right = statusline_right;
                    self.pending_font = Some(font_path);
                }
                GuiEvent::Editor { pane_id, event } => match event {
                    EditorEvent::SnapshotUpdated => {
                        if let Some(pane) = self.panes.get(&pane_id) {
                            pane.engine.acknowledge_snapshot_update();
                        }
                    }
                    EditorEvent::ActivateLine { .. } => {}
                    EditorEvent::OpenConfigRequested | EditorEvent::ReloadConfigRequested => {}
                    EditorEvent::WindowCloseRequested => self.viewport_close_requested = true,
                    EditorEvent::WindowMaximizeRequested => self.viewport_maximize_requested = true,
                    EditorEvent::WindowMinimizeRequested => self.viewport_minimize_requested = true,
                    EditorEvent::CenterCursor { line } => {
                        if let Some(pane) = self.panes.get_mut(&pane_id) {
                            pane.center_line = Some(line);
                        }
                    }
                    EditorEvent::OpenWith { .. } => {}
                    EditorEvent::YankPath { .. } => {}
                    EditorEvent::NavigateInto { .. } => {}
                    EditorEvent::OpenTerminal { .. } => {}
                    EditorEvent::NavigateParent => {}
                    EditorEvent::HistoryBack => {}
                    EditorEvent::HistoryForward => {}
                    EditorEvent::RefreshRequested => {}
                    EditorEvent::DirSizeRequested { .. } => {}
                    EditorEvent::ChangeDirectory { .. } => {}
                    EditorEvent::ChangeSort { .. } => {}
                    EditorEvent::ToggleHidden => {}
                    EditorEvent::ToggleDockFocus => self.navigation_dock.toggle_focus(),
                    EditorEvent::Fold { .. } => {}
                    EditorEvent::JumpBookmark { .. } => {}
                    EditorEvent::OpenFilePicker => {}
                    EditorEvent::FeedbackRequested => {}
                    EditorEvent::ShowHelp => {
                        self.dialog = Some(DialogState::Help {
                            query: String::new(),
                            searching: false,
                        })
                    }
                    EditorEvent::PaneAction(_) => {}
                    EditorEvent::TransferRequested { .. } => {}
                    EditorEvent::ClipboardCopyRequested { .. } => {}
                    EditorEvent::ClipboardCutRequested { .. } => {}
                    EditorEvent::ClipboardPasteRequested { .. } => {}
                    EditorEvent::OpenAsAdmin { .. } => {}
                    EditorEvent::CreateShortcut { .. } => {}
                    EditorEvent::ExtractArchive { .. } => {}
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
                GuiEvent::ShowActionConfirm {
                    title,
                    approve_label,
                    lines,
                } => {
                    self.dialog = Some(DialogState::ActionConfirm {
                        title,
                        approve_label,
                        lines,
                    });
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
                GuiEvent::ExtractProgress(progress) => {
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
                GuiEvent::ShowActionReport {
                    title,
                    lines,
                    any_failed,
                } => {
                    self.dialog = Some(DialogState::ActionReport {
                        title,
                        lines,
                        any_failed,
                    });
                }
                GuiEvent::ShowTransferReport(report) => {
                    self.dialog = Some(DialogState::TransferReport(report));
                }
                GuiEvent::ShowImportReport { report, effect } => {
                    self.dialog = Some(DialogState::ImportReport { report, effect });
                }
                GuiEvent::ShowDragCleanupConfirm { paths } => {
                    self.dialog = Some(DialogState::DragCleanupConfirm { paths });
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
        if let Some(font) = self.pending_font.take() {
            if let Some(path) = font.as_deref() {
                if let Err(error) = fs::read(path) {
                    self.message = Some(EditorMessage {
                        kind: fyler_core::editor::MessageKind::Error,
                        text: format!("Failed to load font {}: {error}", path.display()),
                    });
                }
            }
            install_fallback_font(ctx, font.as_deref());
        }
        if self.viewport_close_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            self.viewport_close_requested = false;
        }
        if self.viewport_maximize_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
            self.viewport_maximize_requested = false;
        }
        if self.viewport_minimize_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            self.viewport_minimize_requested = false;
        }
        if self.resize_to_monitor_on_first_frame
            && let Some((size, position)) =
                initial_window_geometry(ctx.input(|input| input.viewport().monitor_size))
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(position));
            self.resize_to_monitor_on_first_frame = false;
        }
        if let Some(state) = self.position_correction {
            let observed = ctx
                .input(|input| input.viewport().outer_rect)
                .map(|rect| rect.min);
            let (next, should_correct) = state.poll(observed);
            self.position_correction = next;
            if should_correct {
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(state.target));
            }
        }
        if let Some(caption) = self.native_caption.as_mut() {
            let toolbar_height = (theme::TOOLBAR_HEIGHT * ctx.pixels_per_point()).round() as u32;
            match caption.update(toolbar_height) {
                Ok(layout) => self.native_caption_layout = layout,
                Err(error) => {
                    self.fatal_error =
                        Some(format!("Failed to update native window caption: {error:#}"));
                    self.native_caption_layout = None;
                }
            }
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
        let mut drag_rect = None;
        let native_path = self.native_caption.is_some();
        ui.ctx().data_mut(|data| {
            data.insert_temp(
                egui::Id::new("fyler-native-caption"),
                (native_path, self.native_caption_layout),
            );
        });
        let ppp = ui.ctx().pixels_per_point();
        let caption_left = self
            .native_caption_layout
            .map(|layout| layout.buttons.x as f32 / ppp);
        let caption_cell_size = self.native_caption_layout.map_or_else(
            || egui::vec2(46.0, 32.0),
            |layout| {
                egui::vec2(
                    layout.buttons.width as f32 / (3.0 * ppp),
                    layout.buttons.height as f32 / ppp,
                )
            },
        );
        let render_toolbar_height = self
            .native_caption_layout
            .map_or(theme::TOOLBAR_HEIGHT, |layout| {
                theme::TOOLBAR_HEIGHT.max(layout.toolbar_height as f32 / ppp)
            })
            .max(caption_cell_size.y);
        let panel_frame = if native_path {
            egui::Frame::NONE
        } else {
            egui::Frame::NONE.fill(theme::CANVAS)
        };
        egui::Panel::top("fyler-toolbar")
            .frame(panel_frame)
            .exact_size(render_toolbar_height)
            .show(ui, |ui| {
                if !native_path || self.native_caption_layout.is_some() {
                    let (can_go_back, can_go_forward) = chrome_state
                        .as_ref()
                        .map(|(_, _, back, forward)| (*back, *forward))
                        .unwrap_or((false, false));
                    (chrome_action, drag_rect) = chrome::draw_toolbar(
                        ui,
                        can_go_back,
                        can_go_forward,
                        caption_cell_size,
                        caption_left,
                    );
                }
            });
        if let Some(caption) = self.native_caption.as_mut() {
            let region = drag_rect.and_then(|rect| physical_drag_region(rect, ppp));
            caption.set_drag_region(region);
        }
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
            let command_frame = egui::Frame::NONE
                .fill(theme::SURFACE)
                .inner_margin(egui::Margin::symmetric(8, 2));
            egui::Panel::bottom("global-command-area")
                .frame(command_frame)
                .show(ui, |ui| {
                    if let Some(state) = &self.popupmenu {
                        cmdline::draw_popupmenu(ui, state);
                    }
                    if let Some(state) = &self.cmdline {
                        cmdline::draw_cmdline(ui, state);
                    }
                });
        } else if let Some(message) = &self.message {
            egui::Panel::top("global-message-area")
                .frame(panel_frame)
                .exact_size(34.0)
                .show(ui, |ui| cmdline::draw_message(ui, message));
        }

        let layout = self.layout.clone();
        let active = self.active;
        let fatal_error = self.fatal_error.clone();

        let dragging_files = ui.ctx().input(|i| !i.raw.hovered_files.is_empty());
        let internal_drag_active = self.tree_drag.is_some();
        let mut drag_collect = DragCollect::default();
        let (ime, navigation_clicked, tree_click, drop_target_line) = egui::CentralPanel::default()
            .frame(panel_frame)
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
                        internal_drag_active,
                        &mut drag_collect,
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
                        is_zip: row.is_zip,
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

        // GUI window内のtree drag(pane間drag含む)。境界を離れたらOLE dragへ、
        // window内でbutton releaseしたら別paneの場合のみTreeDragDropを送る。
        if self.dialog.is_none() {
            if let Some((pane_id, started)) = drag_collect.started
                && self.tree_drag.is_none()
                && started.has_id
            {
                self.tree_drag = Some(TreeDragSession {
                    pane_id,
                    line: started.line,
                });
            }
            if let Some(session) = self.tree_drag {
                let pointer = ui.ctx().input(|i| i.pointer.latest_pos());
                let viewport = ui.ctx().input(|i| i.viewport_rect());
                let button_down = ui.ctx().input(|i| i.pointer.primary_down());
                if !button_down {
                    self.tree_drag = None;
                    if let Some(pos) = pointer
                        && let Some((target_pane, target_line)) =
                            resolve_drag_drop_target(pos, session.pane_id, &drag_collect.rects)
                        && self
                            .action_tx
                            .send(GuiAction::TreeDragDrop {
                                source_pane: session.pane_id,
                                source_line: session.line,
                                target_pane,
                                target_line,
                                copy: ui.ctx().input(|i| i.modifiers.ctrl),
                            })
                            .is_err()
                    {
                        self.fatal_error = Some("Failed to send tree drag drop to app".to_owned());
                    }
                } else if pointer_left_window(pointer, viewport) {
                    self.tree_drag = None;
                    if self
                        .action_tx
                        .send(GuiAction::TreeDragOut {
                            pane_id: session.pane_id,
                            line: session.line,
                        })
                        .is_err()
                    {
                        self.fatal_error = Some("Failed to send tree drag-out to app".to_owned());
                    }
                }
            }
        } else {
            self.tree_drag = None;
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
            Some(DialogState::ActionConfirm {
                title,
                approve_label,
                lines,
            }) => {
                confirm_choice = confirm::draw_action_confirm(ui, title, approve_label, lines);
            }
            Some(DialogState::Report(report)) => {
                dismiss_report = confirm::draw_report(ui, report);
            }
            Some(DialogState::UndoReport { lines, any_failed }) => {
                dismiss_report = confirm::draw_undo_report(ui, lines, *any_failed);
            }
            Some(DialogState::ActionReport {
                title,
                lines,
                any_failed,
            }) => {
                dismiss_report = confirm::draw_action_report(ui, title, lines, *any_failed);
            }
            Some(DialogState::TransferReport(report)) => {
                dismiss_report = confirm::draw_transfer_report(ui, report);
            }
            Some(DialogState::ImportReport { report, effect }) => {
                dismiss_report = confirm::draw_import_report(ui, report, *effect);
            }
            Some(DialogState::DragCleanupConfirm { paths }) => {
                confirm_choice = confirm::draw_drag_cleanup_confirm(ui, paths);
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
            Some(DialogState::Help { query, searching }) => {
                dismiss_errors = draw_help(ui, &self.help_entries, query, searching).0;
            }
            Some(DialogState::ValidationErrors(errors)) => {
                let dismiss_from_keyboard = ui.ctx().input(|input| {
                    input.key_pressed(egui::Key::Enter) || input.key_pressed(egui::Key::Escape)
                });
                dismiss_errors = caption_modal(ui.ctx(), egui::Id::new("save-validation-errors"))
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
                is_zip,
                engine_ok,
                offline,
            }) => {
                tree_context_result = Some((
                    *pane_id,
                    *line,
                    draw_tree_context_menu(
                        ui, *pos, *has_id, *is_dir, *is_zip, *engine_ok, *offline,
                    ),
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
        if let Some(layout) = self.native_caption_layout {
            paint_native_caption(ui.ctx(), layout);
        }
    }
    #[cfg(windows)]
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }
}

fn should_forward_input(dialog_absent: bool, fatal_absent: bool, engine_healthy: bool) -> bool {
    dialog_absent && fatal_absent && engine_healthy
}

fn physical_drag_region(rect: egui::Rect, pixels_per_point: f32) -> Option<PhysicalRect> {
    if !rect.is_positive()
        || !pixels_per_point.is_finite()
        || pixels_per_point <= 0.0
        || ![rect.left(), rect.top(), rect.right(), rect.bottom()]
            .into_iter()
            .all(f32::is_finite)
    {
        return None;
    }
    let scale = f64::from(pixels_per_point);
    let left = (f64::from(rect.left()) * scale).ceil();
    let top = (f64::from(rect.top()) * scale).ceil();
    let right = (f64::from(rect.right()) * scale).floor();
    let bottom = (f64::from(rect.bottom()) * scale).floor();
    if left < f64::from(i32::MIN)
        || top < f64::from(i32::MIN)
        || right > f64::from(i32::MAX)
        || bottom > f64::from(i32::MAX)
        || right - left > f64::from(i32::MAX)
        || bottom - top > f64::from(i32::MAX)
        || right <= left
        || bottom <= top
    {
        return None;
    }
    Some(PhysicalRect {
        x: left as i32,
        y: top as i32,
        width: (right - left) as i32,
        height: (bottom - top) as i32,
    })
}
fn paint_native_caption(ctx: &egui::Context, layout: NativeCaptionLayout) {
    let ppp = ctx.pixels_per_point();
    let viewport = ctx.content_rect();
    if !ppp.is_finite() || ppp <= 0.0 || !viewport.is_finite() {
        return;
    }
    let caption = layout.buttons;
    if caption.width <= 0 || caption.height <= 0 || layout.toolbar_height == 0 {
        return;
    }
    let origin = egui::pos2(viewport.left(), viewport.top());
    let painter = ctx
        .layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("native-caption"),
        ))
        .with_clip_rect(viewport);
    let caption_right = i64::from(caption.x) + i64::from(caption.width);
    let px = |physical: i64| physical as f32 / ppp;
    let strip = egui::Rect::from_min_max(
        origin + egui::vec2(px(i64::from(caption.x)), 0.0),
        origin + egui::vec2(px(caption_right), px(i64::from(layout.toolbar_height))),
    )
    .intersect(viewport);
    if !strip.is_positive() || !strip.is_finite() {
        return;
    }
    let painter = painter.with_clip_rect(strip);
    painter.rect_filled(strip, 0.0, theme::SURFACE);
    let edge = |index: i64| i64::from(caption.x) + i64::from(caption.width) * index / 3;
    let snap = |point: egui::Pos2| {
        egui::pos2((point.x * ppp).round() / ppp, (point.y * ppp).round() / ppp)
    };
    let maximized = ctx.input(|input| input.viewport().maximized.unwrap_or(false));
    let stroke = egui::Stroke::new(1.0 / ppp, egui::Color32::WHITE);
    for index in 0..3 {
        let left = edge(index);
        let right = edge(index + 1);
        if right <= left {
            continue;
        }
        let full_cell = egui::Rect::from_min_max(
            origin + egui::vec2(px(left), 0.0),
            origin + egui::vec2(px(right), px(i64::from(layout.toolbar_height))),
        );
        let cell = full_cell.intersect(viewport);
        if !cell.is_positive() {
            continue;
        }
        let hot = layout.hovered_button.is_some_and(|hovered| {
            i64::from(hovered.x) == left
                && i64::from(hovered.width) == right - left
                && hovered.y == 0
                && i64::from(hovered.height) == i64::from(layout.toolbar_height)
        });
        if hot {
            painter.rect_filled(
                cell,
                0.0,
                if index == 2 {
                    theme::CAPTION_CLOSE_HOVER
                } else {
                    theme::TOOLBAR_HOVER
                },
            );
        }
        let center = snap(full_cell.center());
        match index {
            0 => {
                painter.line_segment(
                    [
                        snap(center - egui::vec2(5.0, 0.0)),
                        snap(center + egui::vec2(5.0, 0.0)),
                    ],
                    stroke,
                );
            }
            1 => paint_caption_maximize_glyph(&painter, center, stroke, maximized),
            _ => {
                let rect = egui::Rect::from_center_size(center, egui::vec2(10.0, 10.0));
                painter.line_segment([snap(rect.left_top()), snap(rect.right_bottom())], stroke);
                painter.line_segment([snap(rect.right_top()), snap(rect.left_bottom())], stroke);
            }
        }
    }
}

fn paint_caption_maximize_glyph(
    painter: &egui::Painter,
    center: egui::Pos2,
    stroke: egui::Stroke,
    maximized: bool,
) {
    if maximized {
        for offset in [egui::vec2(-1.5, -1.5), egui::vec2(1.5, 1.5)] {
            let square = egui::Rect::from_center_size(center + offset, egui::vec2(7.0, 7.0));
            painter.rect_stroke(square, 0.0, stroke, egui::StrokeKind::Inside);
        }
    } else {
        let square = egui::Rect::from_center_size(center, egui::vec2(9.0, 9.0));
        painter.rect_stroke(square, 0.0, stroke, egui::StrokeKind::Inside);
    }
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
    internal_drag_active: bool,
    drag_collect: &mut DragCollect,
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
        internal_drag_active,
        &mut drop_target_line,
        drag_collect,
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
    internal_drag_active: bool,
    drop_target_line: &mut Option<usize>,
    drag_collect: &mut DragCollect,
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
                            pane.center_line.take(),
                            *id,
                            *id == active,
                            *id == active && drag_active,
                            internal_drag_active,
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
            if let Some(started) = output.drag_started {
                drag_collect.started.get_or_insert((*id, started));
            }
            if internal_drag_active {
                drag_collect
                    .rects
                    .push((*id, output.tree_rect, output.drop_target_line));
            }
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
                internal_drag_active,
                drop_target_line,
                drag_collect,
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
                internal_drag_active,
                drop_target_line,
                drag_collect,
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

/// 候補選択に使うキーを読み取り、同一フレーム内でイベントキューから消費する。
///
/// pickerモーダルは検索クエリの`TextEdit`と候補リストを同時に表示するため、
/// ここで消費しておかないと同じ矢印キー/Enter/Escapeが`TextEdit`にも渡ってしまい、
/// カーソル移動・フォーカス解除が候補選択と二重に発生する。この関数は`TextEdit`を
/// 描画するより前に呼ぶこと(egui はフレーム内で先に消費した側が勝つ)。
fn read_picker_keys(context: &egui::Context) -> PickerKeys {
    context.input_mut(|input| {
        // `||`は短絡評価されるため、両方の`consume_key`呼び出しを必ず走らせて
        // 片方だけが未消費のままイベントキューに残ることを防ぐ。
        let arrow_up = input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp);
        let ctrl_p = input.consume_key(egui::Modifiers::CTRL, egui::Key::P);
        let previous = arrow_up || ctrl_p;
        let arrow_down = input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown);
        let ctrl_n = input.consume_key(egui::Modifiers::CTRL, egui::Key::N);
        let next = arrow_down || ctrl_n;
        PickerKeys {
            escape: input.consume_key(egui::Modifiers::NONE, egui::Key::Escape),
            previous,
            next,
            enter: input.consume_key(egui::Modifiers::NONE, egui::Key::Enter),
            ctrl_enter: input.consume_key(egui::Modifiers::CTRL, egui::Key::Enter),
        }
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

pub(crate) fn caption_modal(ctx: &egui::Context, id: egui::Id) -> egui::Modal {
    let caption = ctx.data(|data| {
        data.get_temp::<(bool, Option<NativeCaptionLayout>)>(egui::Id::new("fyler-native-caption"))
    });
    let Some((true, layout)) = caption else {
        return egui::Modal::new(id);
    };
    let Some(layout) = layout else {
        return egui::Modal::new(id).backdrop_color(egui::Color32::TRANSPARENT);
    };

    let content = ctx.content_rect();
    let scale = ctx.pixels_per_point();
    let strip_left = layout.buttons.x as f32 / scale;
    let strip_bottom = (content.top() + layout.toolbar_height as f32 / scale).min(content.bottom());
    let reserved = egui::Rect::from_min_max(
        egui::pos2(strip_left, content.top()),
        egui::pos2(content.right(), strip_bottom),
    )
    .intersect(content);
    if !reserved.is_positive() {
        return egui::Modal::new(id);
    }

    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, id));
    let dim = egui::Color32::from_black_alpha(100);
    for rect in [
        egui::Rect::from_min_max(content.min, egui::pos2(content.right(), reserved.top())),
        egui::Rect::from_min_max(egui::pos2(content.left(), reserved.bottom()), content.max),
        egui::Rect::from_min_max(
            egui::pos2(content.left(), reserved.top()),
            egui::pos2(reserved.left(), reserved.bottom()),
        ),
        egui::Rect::from_min_max(
            egui::pos2(reserved.right(), reserved.top()),
            egui::pos2(content.right(), reserved.bottom()),
        ),
    ] {
        if rect.is_positive() {
            painter.rect_filled(rect, 0.0, dim);
        }
    }
    egui::Modal::new(id).backdrop_color(egui::Color32::TRANSPARENT)
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
    caption_modal(ui.ctx(), egui::Id::new("fyler-file-picker")).show(ui.ctx(), |ui| {
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
    caption_modal(ui.ctx(), egui::Id::new("fyler-feedback")).show(ui.ctx(), |ui| {
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
fn help_matches<'a>(help_entries: &'a [HelpEntry], query: &str) -> Vec<&'a HelpEntry> {
    let needle = query.to_lowercase();
    help_entries
        .iter()
        .filter(|entry| {
            needle.is_empty()
                || entry.command.to_lowercase().contains(&needle)
                || entry.description.to_lowercase().contains(&needle)
        })
        .collect()
}

fn help_content_height(viewport_height: f32, frame: egui::Frame) -> f32 {
    (viewport_height * 0.7 - frame.total_margin().sum().y).max(0.0)
}

fn apply_help_keys(
    escape: bool,
    slash: bool,
    key_j: bool,
    key_k: bool,
    text_entry: bool,
    query: &mut String,
    searching: &mut bool,
) -> (bool, bool, f32) {
    let dismiss = escape && !*searching;
    if escape && *searching {
        *searching = false;
        query.clear();
    }
    let begin_search = slash && !text_entry;
    if begin_search {
        *searching = true;
    }
    let scroll_delta = if text_entry {
        0.0
    } else if key_j {
        -80.0
    } else if key_k {
        80.0
    } else {
        0.0
    };
    (dismiss, begin_search, scroll_delta)
}

fn draw_help(
    ui: &mut egui::Ui,
    help_entries: &[HelpEntry],
    query: &mut String,
    searching: &mut bool,
) -> (bool, egui::Rect) {
    let text_entry = *searching;
    let (escape, slash, key_j, key_k) = ui.ctx().input(|input| {
        (
            input.key_pressed(egui::Key::Escape),
            input.key_pressed(egui::Key::Slash),
            input.key_pressed(egui::Key::J),
            input.key_pressed(egui::Key::K),
        )
    });
    let (dismiss, begin_search, scroll_delta) =
        apply_help_keys(escape, slash, key_j, key_k, text_entry, query, searching);
    if escape || begin_search || scroll_delta != 0.0 {
        ui.ctx().input_mut(|input| {
            input.events.retain(|event| {
                !(matches!(
                    event,
                    egui::Event::Key {
                        key: egui::Key::Escape | egui::Key::Slash,
                        ..
                    }
                ) || (scroll_delta != 0.0
                    && matches!(
                        event,
                        egui::Event::Key {
                            key: egui::Key::J | egui::Key::K,
                            ..
                        }
                    )))
            });
        });
    }
    let viewport = ui.ctx().content_rect().size();
    let frame = egui::Frame::popup(ui.style());
    let content_height = help_content_height(viewport.y, frame);
    let outer_width = viewport.x * 0.3;
    let content_width = (outer_width - frame.total_margin().sum().x).max(0.0);
    let modal_response = caption_modal(ui.ctx(), egui::Id::new("fyler-help"))
        .frame(frame)
        .show(ui.ctx(), |ui| {
            ui.set_min_width(content_width);
            ui.set_max_width(content_width);
            ui.set_min_height(content_height);
            ui.set_max_height(content_height);
            let command_width = (content_width * 0.35).max(0.0);
            let description_width = (content_width - command_width - 8.0).max(0.0);
            ui.heading("Keyboard");
            ui.label("j/k scroll · / search · esc close");
            if *searching {
                let response = ui.add(
                    egui::TextEdit::singleline(query)
                        .hint_text("Filter commands")
                        .desired_width(content_width),
                );
                if begin_search {
                    response.request_focus();
                }
            }
            ui.separator();
            let matches = help_matches(help_entries, query);
            let scroll_height = ui.available_height().max(0.0);
            egui::ScrollArea::vertical()
                .id_salt("fyler-help-commands")
                .max_height(scroll_height)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    if scroll_delta != 0.0 {
                        ui.scroll_with_delta(egui::vec2(0.0, scroll_delta));
                    }
                    if matches.is_empty() {
                        ui.label("No matching commands");
                    } else {
                        egui::Grid::new("fyler-help-commands-grid")
                            .num_columns(2)
                            .spacing([8.0, 8.0])
                            .striped(true)
                            .show(ui, |ui| {
                                for entry in matches {
                                    ui.add_sized(
                                        [command_width, 0.0],
                                        egui::Label::new(
                                            egui::RichText::new(&entry.command)
                                                .monospace()
                                                .strong()
                                                .size(12.0)
                                                .color(theme::BLUE),
                                        )
                                        .wrap(),
                                    );
                                    ui.add_sized(
                                        [description_width, 0.0],
                                        egui::Label::new(
                                            egui::RichText::new(&entry.description)
                                                .monospace()
                                                .size(12.0)
                                                .color(theme::TEXT_SECONDARY),
                                        )
                                        .wrap(),
                                    );
                                    ui.end_row();
                                }
                            });
                    }
                });
        });
    (dismiss, modal_response.response.rect)
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

/// context menuに表示する項目(グループ順・グループ内表示順)。
/// グループ間はseparatorで区切って描画する。
const TREE_CONTEXT_GROUPS: [&[TreeContextItem]; 4] = [
    &[
        TreeContextItem::Open,
        TreeContextItem::OpenWith,
        TreeContextItem::OpenAsAdmin,
    ],
    &[
        TreeContextItem::ClipboardCopy,
        TreeContextItem::ClipboardCut,
        TreeContextItem::ClipboardPaste,
    ],
    &[
        TreeContextItem::Rename,
        TreeContextItem::MarkForDeletion,
        TreeContextItem::Extract,
        TreeContextItem::CreateShortcut,
    ],
    &[
        TreeContextItem::CopyPath,
        TreeContextItem::OpenTerminal,
        TreeContextItem::Refresh,
    ],
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
        TreeContextItem::OpenAsAdmin => "Open as administrator",
        TreeContextItem::Rename => "Rename",
        TreeContextItem::MarkForDeletion => "Mark for deletion",
        TreeContextItem::ClipboardCopy => "Copy",
        TreeContextItem::ClipboardCut => "Cut",
        TreeContextItem::ClipboardPaste => "Paste",
        TreeContextItem::Extract => "Extract here",
        TreeContextItem::CreateShortcut => "Create shortcut",
        TreeContextItem::CopyPath => "Copy path",
        TreeContextItem::OpenTerminal => "Open terminal here",
        TreeContextItem::Refresh => "Refresh",
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
    is_zip: bool,
    engine_ok: bool,
    offline: bool,
) -> bool {
    if !engine_ok {
        return false;
    }
    match item {
        TreeContextItem::Open | TreeContextItem::Rename | TreeContextItem::CopyPath => has_id,
        TreeContextItem::OpenWith | TreeContextItem::OpenAsAdmin => has_id && !is_dir && !offline,
        TreeContextItem::MarkForDeletion => true,
        TreeContextItem::ClipboardCopy | TreeContextItem::ClipboardCut => has_id && !offline,
        TreeContextItem::ClipboardPaste => !offline,
        TreeContextItem::Extract => has_id && !is_dir && !offline && is_zip,
        TreeContextItem::CreateShortcut => has_id && !offline,
        TreeContextItem::OpenTerminal => has_id && !offline,
        TreeContextItem::Refresh => true,
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
    is_zip: bool,
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
        for (group_index, group) in TREE_CONTEXT_GROUPS.iter().enumerate() {
            if group_index > 0 {
                ui.separator();
            }
            for &item in *group {
                let enabled =
                    context_item_enabled(item, has_id, is_dir, is_zip, engine_ok, offline);
                let label = context_item_label(item, is_dir);
                let response = ui.add_enabled(enabled, egui::Button::new(label));
                let response = if enabled {
                    response
                } else {
                    response.on_disabled_hover_text(context_item_disabled_reason(
                        item, engine_ok, has_id, is_dir, offline, is_zip,
                    ))
                };
                if response.clicked() {
                    chosen = Some(item);
                }
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

/// disableされた項目のtooltip文言。優先度: crashed > offline > item固有 > has_id。
fn context_item_disabled_reason(
    item: TreeContextItem,
    engine_ok: bool,
    has_id: bool,
    is_dir: bool,
    offline: bool,
    is_zip: bool,
) -> &'static str {
    if !engine_ok {
        return "Editor engine has stopped";
    }
    if offline {
        return "Location is offline";
    }
    match item {
        TreeContextItem::OpenWith if is_dir => return "Open with is for files only",
        TreeContextItem::OpenAsAdmin if is_dir => {
            return "Open as administrator is for files only";
        }
        TreeContextItem::Extract if is_dir || !is_zip => {
            return "Extract is for .zip archives only";
        }
        _ => {}
    }
    if !has_id {
        return "This entry has not been saved yet";
    }
    ""
}

fn current_window_geometry(context: &egui::Context) -> Option<WindowGeometry> {
    let scale = context.pixels_per_point();
    context.input(|input| window_geometry_from_viewport(input.viewport(), scale))
}

fn window_geometry_from_viewport(
    viewport: &egui::ViewportInfo,
    scale: f32,
) -> Option<WindowGeometry> {
    let inner = viewport.inner_rect?;
    let outer = viewport.outer_rect?;
    WindowGeometry::new(
        inner.width(),
        inner.height(),
        outer.min.x,
        outer.min.y,
        viewport.maximized.unwrap_or(false),
        scale,
    )
}

/// 起動直後の1回だけの位置補正状態機械。
///
/// DPIが異なるセカンダリモニタでは、window生成時の論理→物理px変換に
/// プライマリモニタの`scale`が使われてしまう(winit issue #2645: `with_position`
/// はwindow生成前に呼ばれるため、生成直後のwindowにはまだ「実際のモニタの
/// scale」が存在せずプライマリモニタのscaleで暫定変換される)。
/// `ViewportCommand::OuterPosition`はwindow生成後に送るコマンドなので、その時点の
/// 実際のscaleで変換され直され正しい位置へ補正できる。
///
/// 生成直後の数フレームで実際の配置(`outer_rect`)が保存位置とズレていれば
/// 1回だけ補正する。無限補正ループ防止のため、判定自体も`MAX_FRAMES`で
/// 打ち切り、補正は最大1回しか送らない。
#[derive(Debug, Clone, Copy, PartialEq)]
struct PositionCorrection {
    target: egui::Pos2,
    frames_remaining: u8,
}

const POSITION_CORRECTION_EPSILON: f32 = 1.0;
const POSITION_CORRECTION_MAX_FRAMES: u8 = 5;

impl PositionCorrection {
    fn new(target: egui::Pos2) -> Self {
        Self {
            target,
            frames_remaining: POSITION_CORRECTION_MAX_FRAMES,
        }
    }

    /// このフレームの判定結果を返す。戻り値の1つ目は次フレームへ持ち越す状態
    /// (`None`なら判定終了、以後このwindowでは二度と補正しない)、2つ目は
    /// `target`へ`OuterPosition`を送るべきかどうか。
    fn poll(self, observed: Option<egui::Pos2>) -> (Option<Self>, bool) {
        let Some(observed) = observed else {
            let frames_remaining = self.frames_remaining.saturating_sub(1);
            if frames_remaining == 0 {
                return (None, false);
            }
            return (
                Some(Self {
                    frames_remaining,
                    ..self
                }),
                false,
            );
        };
        let drifted = (observed.x - self.target.x).abs() > POSITION_CORRECTION_EPSILON
            || (observed.y - self.target.y).abs() > POSITION_CORRECTION_EPSILON;
        (None, drifted)
    }
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

fn window_icon() -> egui::IconData {
    let rgba = include_bytes!(concat!(env!("OUT_DIR"), "/window_icon.rgba")).to_vec();
    egui::IconData {
        rgba,
        width: env!("FYLER_WINDOW_ICON_WIDTH")
            .parse()
            .expect("build.rs emits a valid icon width"),
        height: env!("FYLER_WINDOW_ICON_HEIGHT")
            .parse()
            .expect("build.rs emits a valid icon height"),
    }
}

fn native_options(window: Option<StartupWindow>) -> eframe::NativeOptions {
    let viewport = window
        .map_or_else(egui::ViewportBuilder::default, |window| {
            let builder = egui::ViewportBuilder::default()
                .with_inner_size([window.geometry.inner_width, window.geometry.inner_height])
                .with_maximized(window.geometry.maximized);
            if window.apply_position {
                builder.with_position([window.geometry.outer_x, window.geometry.outer_y])
            } else {
                builder
            }
        })
        .with_decorations(!cfg!(windows))
        .with_transparent(cfg!(windows))
        .with_min_inner_size([720.0, 480.0])
        .with_title("fyler")
        .with_icon(window_icon());
    let options = eframe::NativeOptions {
        viewport,
        // Geometry is persisted in session.toml so it shares the same normal-shutdown contract
        // as pane state and does not depend on eframe's optional persistence feature.
        persist_window: false,
        ..Default::default()
    };
    #[cfg(windows)]
    let options = {
        let mut options = options;
        let mut setup = eframe::egui_wgpu::WgpuSetupCreateNew::without_display_handle();
        setup.instance_descriptor.backends = eframe::wgpu::Backends::DX12;
        setup
            .instance_descriptor
            .backend_options
            .dx12
            .presentation_system = eframe::wgpu::Dx12SwapchainKind::DxgiFromVisual;
        options.wgpu_options.wgpu_setup = eframe::egui_wgpu::WgpuSetup::CreateNew(setup);
        options
    };
    options
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
    initial_window: Option<StartupWindow>,
    window_geometry: Arc<Mutex<Option<WindowGeometry>>>,
    native_caption_factory: Option<NativeWindowCaptionFactory>,
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
            // オフスクリーンガードで位置を捨てた場合(`apply_position: false`)は、
            // 補正の基準になる保存位置がそもそも信頼できないため状態機械を作らない。
            let position_correction =
                initial_window
                    .filter(|window| window.apply_position)
                    .map(|window| {
                        PositionCorrection::new(egui::pos2(
                            window.geometry.outer_x,
                            window.geometry.outer_y,
                        ))
                    });
            #[cfg(windows)]
            let native_caption = {
                let factory = native_caption_factory.ok_or_else(|| {
                    anyhow::anyhow!("Windows GUI requires native caption integration")
                })?;
                let hwnd = match creation_context.window_handle()?.as_raw() {
                    RawWindowHandle::Win32(handle) => handle.hwnd.get(),
                    _ => {
                        return Err(anyhow::anyhow!(
                            "Windows GUI did not provide a Win32 window handle"
                        )
                        .into());
                    }
                };
                let repaint_context = creation_context.egui_ctx.clone();
                let surface = theme::SURFACE;
                Some(factory(
                    hwnd,
                    Arc::new(move || repaint_context.request_repaint()),
                    [surface.r(), surface.g(), surface.b()],
                )?)
            };
            #[cfg(not(windows))]
            let native_caption = {
                let _ = native_caption_factory;
                None
            };
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
                native_caption,
            )
            .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { error.into() })?;
            app.resize_to_monitor_on_first_frame = resize_to_monitor_on_first_frame;
            app.position_correction = position_correction;
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

    #[test]
    fn physical_drag_region_tracks_scale_and_rejects_invalid_geometry() {
        let rect = egui::Rect::from_min_max(egui::pos2(50.25, 4.0), egui::pos2(200.0, 40.0));
        assert_eq!(
            physical_drag_region(rect, 1.0),
            Some(PhysicalRect {
                x: 51,
                y: 4,
                width: 149,
                height: 36
            })
        );
        assert_eq!(
            physical_drag_region(rect, 2.0),
            Some(PhysicalRect {
                x: 101,
                y: 8,
                width: 299,
                height: 72
            })
        );
        let drag = physical_drag_region(rect, 2.0).unwrap();
        assert!(drag.x + drag.width <= (rect.right() * 2.0) as i32);
        assert_eq!(physical_drag_region(rect, f32::INFINITY), None);
        assert_eq!(physical_drag_region(rect, 0.0), None);
        assert_eq!(physical_drag_region(egui::Rect::NOTHING, 1.0), None);
        assert_eq!(
            physical_drag_region(
                egui::Rect::from_min_max(egui::pos2(0.01, 0.01), egui::pos2(0.1, 0.1)),
                1.0
            ),
            None
        );
    }

    #[test]
    fn native_caption_paints_opaque_full_height_hover_and_centered_glyphs() {
        let context = egui::Context::default();
        context.set_pixels_per_point(1.25);
        let layout = NativeCaptionLayout {
            buttons: PhysicalRect {
                x: 70,
                y: 0,
                width: 31,
                height: 43,
            },
            hovered_button: Some(PhysicalRect {
                x: 90,
                y: 0,
                width: 11,
                height: 43,
            }),
            toolbar_height: 43,
        };
        let output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(110.0, 50.0),
                )),
                ..Default::default()
            },
            |_| paint_native_caption(&context, layout),
        );
        let surface = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Rect(rect) if rect.fill == theme::SURFACE => Some(rect.rect),
                _ => None,
            })
            .expect("caption strip has an opaque surface");
        assert_eq!(
            surface,
            egui::Rect::from_min_max(egui::pos2(56.0, 0.0), egui::pos2(80.8, 34.4))
        );
        let close_hover = output
            .shapes
            .iter()
            .find_map(|shape| match &shape.shape {
                egui::Shape::Rect(rect) if rect.fill == theme::CAPTION_CLOSE_HOVER => {
                    Some(rect.rect)
                }
                _ => None,
            })
            .expect("close hover is painted red");
        assert_eq!(
            close_hover,
            egui::Rect::from_min_max(egui::pos2(72.0, 0.0), egui::pos2(80.8, 34.4))
        );
        let min_hover = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(110.0, 50.0),
                )),
                ..Default::default()
            },
            |_| {
                paint_native_caption(
                    &context,
                    NativeCaptionLayout {
                        hovered_button: Some(PhysicalRect {
                            x: 70,
                            y: 0,
                            width: 10,
                            height: 43,
                        }),
                        ..layout
                    },
                )
            },
        );
        assert!(min_hover.shapes.iter().any(|shape| matches!(&shape.shape,
            egui::Shape::Rect(rect) if rect.fill == theme::TOOLBAR_HOVER
                && rect.rect == egui::Rect::from_min_max(egui::pos2(56.0, 0.0), egui::pos2(64.0, 34.4)))));
        assert_eq!(
            theme::CAPTION_CLOSE_HOVER,
            egui::Color32::from_rgb(232, 17, 35)
        );
        assert_eq!(theme::TOOLBAR_HOVER, egui::Color32::from_rgb(31, 31, 34));
        let lines: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::LineSegment { points, stroke }
                    if stroke.color == egui::Color32::WHITE =>
                {
                    Some(*points)
                }
                _ => None,
            })
            .collect();
        for center in [60.0, 76.0] {
            assert!(
                lines.iter().any(|points| {
                    ((points[0].x + points[1].x) * 0.5 - center).abs() <= 1.0
                        && ((points[0].y + points[1].y) * 0.5 - 17.2).abs() <= 1.0
                }),
                "minimize and close glyphs are centered in their unclipped cells"
            );
        }
        assert!(
            output.shapes.iter().any(|shape| matches!(&shape.shape,
            egui::Shape::Rect(rect) if rect.stroke.color == egui::Color32::WHITE
                && (rect.rect.center().x - 68.0).abs() <= 1.0
                && (rect.rect.center().y - 17.2).abs() <= 1.0)),
            "maximize uses a centered outlined square"
        );
        assert!(
            lines
                .iter()
                .any(|points| (points[0].x - points[1].x).abs() > 5.0),
            "caption glyphs use crisp white strokes"
        );
        for (maximized, expected_squares) in [(false, 1), (true, 2)] {
            let state_output = context.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(110.0, 50.0),
                    )),
                    ..Default::default()
                },
                |ctx| {
                    paint_caption_maximize_glyph(
                        &ctx.layer_painter(egui::LayerId::new(
                            egui::Order::Foreground,
                            egui::Id::new("caption-maximize-test"),
                        )),
                        egui::pos2(68.0, 17.2),
                        egui::Stroke::new(1.0, egui::Color32::WHITE),
                        maximized,
                    );
                },
            );
            assert_eq!(
                state_output
                    .shapes
                    .iter()
                    .filter(|shape| matches!(&shape.shape,
                        egui::Shape::Rect(rect) if rect.stroke.color == egui::Color32::WHITE))
                    .count(),
                expected_squares,
                "maximized and restored windows have distinct caption glyphs"
            );
        }
    }

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

    struct FakeNativeCaption {
        layout: Option<NativeCaptionLayout>,
        drag_region: Arc<Mutex<Option<PhysicalRect>>>,
    }

    impl NativeWindowCaption for FakeNativeCaption {
        fn update(&mut self, _toolbar_height: u32) -> anyhow::Result<Option<NativeCaptionLayout>> {
            Ok(self.layout)
        }

        fn set_drag_region(&mut self, region: Option<PhysicalRect>) {
            *self.drag_region.lock().unwrap() = region;
        }
    }

    #[test]
    fn native_caption_headless_ui_keeps_opaque_caption_and_drag_separate() {
        for (width, ppp, caption_width) in [
            (720.0, 1.0, 100.0),
            (720.0, 1.5, 560.0),
            (1344.0, 2.0, 100.0),
        ] {
            let context = egui::Context::default();
            context.set_pixels_per_point(ppp);
            theme::install(&context);
            let (mut app, _, _) = empty_test_app();
            let caption_x = (width - caption_width) * ppp;
            let drag_region = Arc::new(Mutex::new(None));
            app.native_caption = Some(Box::new(FakeNativeCaption {
                layout: Some(NativeCaptionLayout {
                    buttons: PhysicalRect {
                        x: caption_x as i32,
                        y: 0,
                        width: (caption_width * ppp) as i32,
                        height: (theme::TOOLBAR_HEIGHT * ppp) as i32,
                    },
                    hovered_button: Some(PhysicalRect {
                        x: caption_x as i32 + (caption_width * ppp * 2.0 / 3.0) as i32,
                        y: 0,
                        width: (caption_width * ppp) as i32 - (caption_width * ppp) as i32 * 2 / 3,
                        height: (theme::TOOLBAR_HEIGHT * ppp) as i32,
                    }),
                    toolbar_height: (theme::TOOLBAR_HEIGHT * ppp) as u32,
                }),
                drag_region: Arc::clone(&drag_region),
            }));
            app.dialog = Some(DialogState::Help {
                query: String::new(),
                searching: false,
            });
            let mut frame = eframe::Frame::_new_kittest();
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(width, 480.0),
                )),
                ..Default::default()
            };
            let mut output = egui::FullOutput::default();
            for _ in 0..2 {
                output = context.run_ui(input.clone(), |ui| {
                    eframe::App::logic(&mut app, ui.ctx(), &mut frame);
                    eframe::App::ui(&mut app, ui, &mut frame);
                });
            }
            let drag = drag_region.lock().unwrap();
            if ppp == 1.5 {
                assert!(
                    drag.is_none(),
                    "native-sized controls do not fit this stress viewport"
                );
                assert!(
                    output.shapes.iter().all(|shape| !matches!(
                        &shape.shape,
                        egui::Shape::Text(text)
                            if ["←", "→", "↑", "↻"].contains(&text.galley.job.text.as_str())
                    )),
                    "controls too wide for the stress viewport must not paint navigation glyphs"
                );
            } else {
                let drag = drag.expect("toolbar should expose drag area");
                assert!(drag.x + drag.width <= caption_x as i32);
                assert_eq!(drag.y, 0);
                assert_eq!(drag.height, (theme::TOOLBAR_HEIGHT * ppp) as i32);
                let refresh_glyph_right = output
                    .shapes
                    .iter()
                    .find_map(|shape| match &shape.shape {
                        egui::Shape::Text(text) if text.galley.job.text == "↻" => {
                            Some((text.pos.x + text.galley.size().x) * ppp)
                        }
                        _ => None,
                    })
                    .expect("refresh navigation control must be painted");
                assert!(drag.x >= refresh_glyph_right.ceil() as i32);
            }
            let reserved = egui::Rect::from_min_size(
                egui::pos2(width - caption_width, 0.0),
                egui::vec2(caption_width, theme::TOOLBAR_HEIGHT),
            );
            let caption_surface = output.shapes.iter().find_map(|shape| match &shape.shape {
                egui::Shape::Rect(rect)
                    if rect.fill == theme::SURFACE
                        && rect.rect.contains(reserved.center())
                        && rect.rect.height() >= theme::TOOLBAR_HEIGHT =>
                {
                    Some(rect.rect)
                }
                _ => None,
            });
            assert!(
                caption_surface.is_some(),
                "reserved caption strip is opaque surface"
            );
            assert!(output.shapes.iter().any(|shape| matches!(&shape.shape,
                egui::Shape::Rect(rect) if rect.fill == theme::CAPTION_CLOSE_HOVER
                    && rect.rect.intersect(reserved).is_positive())));
            let dim_backdrops = output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::Rect(rect)
                        if rect.fill == egui::Color32::from_black_alpha(100) =>
                    {
                        Some(rect.rect)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert!(
                !dim_backdrops.is_empty(),
                "Help still dims non-caption content"
            );
            assert!(
                dim_backdrops
                    .iter()
                    .all(|rect| !rect.intersect(reserved).is_positive()),
                "Help backdrop must not paint any part of the OS-owned caption strip"
            );
            assert!(
                dim_backdrops
                    .iter()
                    .enumerate()
                    .all(|(index, rect)| dim_backdrops[index + 1..]
                        .iter()
                        .all(|other| !rect.intersect(*other).is_positive())),
                "Help backdrop pieces must not overlap and darken pixels twice"
            );
            let dim_count_at = |point| {
                dim_backdrops
                    .iter()
                    .filter(|rect| rect.contains(point))
                    .count()
            };
            assert_eq!(
                dim_count_at(egui::pos2(
                    (width - caption_width) * 0.5,
                    theme::TOOLBAR_HEIGHT * 0.5
                )),
                1,
                "content beside the reserved strip is dimmed exactly once"
            );
            assert_eq!(
                dim_count_at(egui::pos2(width - 20.0, theme::TOOLBAR_HEIGHT + 10.0)),
                1,
                "content below the reserved strip is dimmed exactly once"
            );
            assert_eq!(
                dim_count_at(egui::pos2(width - 20.0, theme::TOOLBAR_HEIGHT * 0.5)),
                0,
                "the reserved strip remains undimmed"
            );
        }

        let context = egui::Context::default();
        let (mut app, _, _) = empty_test_app();
        let drag_region = Arc::new(Mutex::new(None));
        app.native_caption = Some(Box::new(FakeNativeCaption {
            layout: None,
            drag_region,
        }));
        app.dialog = Some(DialogState::Help {
            query: String::new(),
            searching: false,
        });
        let mut frame = eframe::Frame::_new_kittest();
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(720.0, 480.0),
            )),
            ..Default::default()
        };
        let output = context.run_ui(input, |ui| {
            eframe::App::logic(&mut app, ui.ctx(), &mut frame);
            eframe::App::ui(&mut app, ui, &mut frame);
        });
        assert!(
            !output.shapes.iter().any(|shape| {
                matches!(&shape.shape, egui::Shape::Text(text)
                    if ["←", "→", "↑", "↻"].contains(&text.galley.job.text.as_str()))
            }),
            "startup without a caption layout must not invent OS caption controls"
        );
        assert!(
            !output.shapes.iter().any(|shape| {
                matches!(&shape.shape, egui::Shape::Rect(rect)
                if rect.fill == egui::Color32::from_black_alpha(100)
                    && rect.rect == egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(720.0, 480.0)
                    ))
            }),
            "Help modal must not paint its backdrop over OS-owned caption controls"
        );
        assert!(
            !output.shapes.iter().any(|shape| {
                matches!(&shape.shape, egui::Shape::Rect(rect)
                    if rect.fill == theme::SURFACE
                        && rect.rect.top() < theme::TOOLBAR_HEIGHT)
            }),
            "startup without native metrics must not paint an unknown-width toolbar surface"
        );
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
                position_correction: None,
                tree_drag: None,
                pending_font: None,
                viewport_close_requested: false,
                viewport_maximize_requested: false,
                viewport_minimize_requested: false,
                native_caption: None,
                native_caption_layout: None,
            },
            event_tx,
            action_rx,
        )
    }

    #[test]
    fn help_modal_geometry_stays_fixed_across_results_search_and_viewports() {
        for size in [egui::vec2(720.0, 480.0), egui::vec2(1344.0, 756.0)] {
            for (count, searching) in [
                (0, false),
                (1, false),
                (20, false),
                (0, true),
                (1, true),
                (20, true),
            ] {
                let context = egui::Context::default();
                let entries = (0..count)
                    .map(|index| HelpEntry {
                        command: format!("Command{index}"),
                        description: format!("Description {index}"),
                    })
                    .collect::<Vec<_>>();
                let mut query = if count == 0 {
                    "no-match".to_owned()
                } else {
                    String::new()
                };
                let mut searching = searching;
                let mut rect = egui::Rect::NOTHING;
                for _ in 0..3 {
                    let input = egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                        ..Default::default()
                    };
                    let _ = context.run_ui(input, |ui| {
                        rect = draw_help(ui, &entries, &mut query, &mut searching).1;
                    });
                }
                assert!((rect.width() - size.x * 0.3).abs() < 1.0);
                assert!((rect.height() - size.y * 0.7).abs() < 1.0);
            }
        }
    }

    #[test]
    fn application_panels_reach_viewport_edges_with_message_and_cmdline() {
        for size in [egui::vec2(720.0, 480.0), egui::vec2(1344.0, 756.0)] {
            for state in ["normal", "message", "cmdline"] {
                let context = egui::Context::default();
                theme::install(&context);
                let (mut app, event_tx, _) = empty_test_app();
                let pane = PaneId::new(1);
                event_tx
                    .send(GuiEvent::AddPane {
                        pane_id: pane,
                        engine: recording_engine(),
                        root: PathBuf::from("root"),
                    })
                    .unwrap();
                app.receive_events();
                app.active = Some(pane);
                app.layout = Some(PaneLayout::Leaf(pane));
                app.navigation_dock.open = false;
                match state {
                    "message" => {
                        app.message = Some(EditorMessage {
                            kind: fyler_core::editor::MessageKind::Info,
                            text: "visible message".to_owned(),
                        });
                    }
                    "cmdline" => {
                        app.cmdline = Some(CmdlineState {
                            prompt: ':',
                            content: "visible command".to_owned(),
                            cursor: 15,
                        });
                    }
                    _ => {}
                }
                let mut frame = eframe::Frame::_new_kittest();
                let input = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                    ..Default::default()
                };
                let mut output = egui::FullOutput::default();
                for _ in 0..3 {
                    output = context.run_ui(input.clone(), |ui| {
                        eframe::App::ui(&mut app, ui, &mut frame);
                    });
                }
                let pane_rect = output
                    .shapes
                    .iter()
                    .filter_map(|clipped| match &clipped.shape {
                        egui::Shape::Rect(rect)
                            if rect.stroke.width == 1.0
                                && rect.stroke.color == theme::BORDER_SUBTLE =>
                        {
                            Some(rect.rect)
                        }
                        _ => None,
                    })
                    .find(|rect| (rect.width() - size.x).abs() < 1.0)
                    .expect("pane border must span the production pane rectangle");
                let pane_top = theme::TOOLBAR_HEIGHT + if state == "message" { 34.0 } else { 0.0 };
                assert!(pane_rect.left().abs() < 1.0, "{pane_rect:?}");
                assert!((pane_rect.right() - size.x).abs() < 1.0, "{pane_rect:?}");
                assert!((pane_rect.top() - pane_top).abs() < 1.0, "{pane_rect:?}");
                assert!(
                    output.shapes.iter().any(|clipped| match &clipped.shape {
                        egui::Shape::Rect(rect) => {
                            rect.fill == theme::SURFACE
                                && rect.rect.left().abs() < 1.0
                                && (rect.rect.right() - size.x).abs() < 1.0
                                && rect.rect.top().abs() < 1.0
                                && (rect.rect.height() - theme::TOOLBAR_HEIGHT).abs() < 1.0
                        }
                        _ => false,
                    }),
                    "toolbar background must reach the top and side viewport edges"
                );
                if state == "message" {
                    assert!(
                        output.shapes.iter().any(|clipped| match &clipped.shape {
                            egui::Shape::Rect(rect) => {
                                rect.fill == theme::SURFACE
                                    && rect.rect.left().abs() < 1.0
                                    && (rect.rect.right() - size.x).abs() < 1.0
                                    && (rect.rect.top() - theme::TOOLBAR_HEIGHT).abs() < 1.0
                                    && (rect.rect.height() - 34.0).abs() < 1.0
                            }
                            _ => false,
                        }),
                        "message background must fill its edge-to-edge panel"
                    );
                }

                let command_rect = if state == "cmdline" {
                    Some(
                        output
                            .shapes
                            .iter()
                            .find_map(|clipped| match &clipped.shape {
                                egui::Shape::Rect(rect)
                                    if rect.fill == theme::SURFACE
                                        && (rect.rect.width() - size.x).abs() < 1.0
                                        && (rect.rect.bottom() - size.y).abs() < 1.0 =>
                                {
                                    Some(rect.rect)
                                }
                                _ => None,
                            })
                            .expect("command band background must reach the bottom viewport edge"),
                    )
                } else {
                    None
                };
                let pane_bottom = command_rect.map_or(size.y, |rect| rect.top());
                assert!(
                    (pane_rect.bottom() - pane_bottom).abs() < 1.0,
                    "{pane_rect:?} vs {command_rect:?}"
                );

                let modeline_rect = egui::Rect::from_min_max(
                    egui::pos2(
                        pane_rect.left() + 1.0,
                        pane_rect.bottom() - 1.0 - theme::STATUSBAR_HEIGHT,
                    ),
                    egui::pos2(pane_rect.right() - 1.0, pane_rect.bottom() - 1.0),
                );
                assert!(
                    output.shapes.iter().any(|clipped| match &clipped.shape {
                        egui::Shape::Rect(rect) =>
                            rect.fill == theme::SURFACE && rect.rect == modeline_rect,
                        _ => false,
                    }),
                    "modeline must retain its inset surface: {modeline_rect:?}"
                );
                let viewport = app.panes[&pane]
                    .tree_viewport
                    .expect("production app UI must render the pane tree");
                let expected_tree_height = pane_rect.height() - 2.0 - theme::STATUSBAR_HEIGHT;
                assert!(
                    (viewport.height - expected_tree_height).abs() < 1.0,
                    "{state} at {size:?}: tree height {} should be {expected_tree_height}",
                    viewport.height
                );
            }
        }
    }

    #[test]
    fn help_filter_is_case_insensitive_and_height_reserves_modal_chrome() {
        let entries = vec![
            HelpEntry {
                command: "OpenFile".to_owned(),
                description: "Open a file".to_owned(),
            },
            HelpEntry {
                command: "ClosePane".to_owned(),
                description: "Close pane".to_owned(),
            },
        ];
        assert_eq!(help_matches(&entries, "OPEN").len(), 1);
        assert_eq!(help_matches(&entries, "close").len(), 1);
        assert_eq!(help_matches(&entries, "missing").len(), 0);
    }
    #[test]
    fn help_keys_focus_search_preserve_text_keys_and_exit_before_closing() {
        let mut query = String::new();
        let mut searching = false;
        assert_eq!(
            apply_help_keys(false, true, false, false, false, &mut query, &mut searching),
            (false, true, 0.0)
        );
        assert!(searching);
        query = "jk".to_owned();
        assert_eq!(
            apply_help_keys(false, false, true, true, true, &mut query, &mut searching),
            (false, false, 0.0)
        );
        assert_eq!(query, "jk");
        assert_eq!(
            apply_help_keys(true, false, false, false, true, &mut query, &mut searching),
            (false, false, 0.0)
        );
        assert!(!searching);
        assert!(query.is_empty());
        assert_eq!(
            apply_help_keys(true, false, false, false, false, &mut query, &mut searching),
            (true, false, 0.0)
        );
        assert_eq!(
            apply_help_keys(false, false, true, false, false, &mut query, &mut searching),
            (false, false, -80.0)
        );
    }

    #[test]
    fn reload_options_replaces_display_settings_and_keeps_live_recent_roots() {
        let (mut app, tx, _) = empty_test_app();
        app.recent_roots = vec![PathBuf::from("live-root")];
        let help = HelpEntry {
            command: "new-command".to_owned(),
            description: "new description".to_owned(),
        };
        let font_path = PathBuf::from("custom-font.ttf");
        tx.send(GuiEvent::ReloadOptions(Box::new(GuiOptions {
            confirm_detail: ConfirmDetail::Summary,
            font_path: Some(font_path.clone()),
            help_entries: vec![help.clone()],
            dock_focus_bindings: Vec::new(),
            bookmarks: vec![("bookmark".to_owned(), PathBuf::from("bookmarked"))],
            recent_roots: vec![PathBuf::from("stale-config-root")],
            drives: vec![PathBuf::from("drive")],
            statusline_left: vec![StatusItem::Mode],
            statusline_right: Vec::new(),
        })))
        .unwrap();
        app.receive_events();

        assert_eq!(app.confirm_detail, ConfirmDetail::Summary);
        assert_eq!(app.help_entries, vec![help]);
        assert_eq!(
            app.bookmarks,
            [("bookmark".to_owned(), PathBuf::from("bookmarked"))]
        );
        assert_eq!(app.recent_roots, [PathBuf::from("live-root")]);
        assert_eq!(app.drives, [PathBuf::from("drive")]);
        assert_eq!(app.statusline_left, [StatusItem::Mode]);
        assert!(app.statusline_right.is_empty());
        assert_eq!(app.pending_font, Some(Some(font_path)));
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
                        center_line: None,
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
                        center_line: None,
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
            position_correction: None,
            tree_drag: None,
            pending_font: None,
            viewport_close_requested: false,
            viewport_maximize_requested: false,
            viewport_minimize_requested: false,
            native_caption: None,
            native_caption_layout: None,
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
            TreeContextItem::OpenAsAdmin,
            TreeContextItem::OpenTerminal,
            TreeContextItem::ClipboardCopy,
            TreeContextItem::ClipboardCut,
            TreeContextItem::Extract,
            TreeContextItem::CreateShortcut,
        ] {
            assert!(!context_item_enabled(item, false, false, true, true, false));
        }
        // Mark for deletionはIDなし(unsaved)行でも有効。
        assert!(context_item_enabled(
            TreeContextItem::MarkForDeletion,
            false,
            false,
            false,
            true,
            false
        ));
    }

    #[test]
    fn context_item_enabled_disables_everything_when_engine_crashed() {
        for group in TREE_CONTEXT_GROUPS {
            for &item in group {
                assert!(!context_item_enabled(item, true, false, true, false, false));
            }
        }
    }

    #[test]
    fn context_item_enabled_open_with_is_files_only_and_requires_online() {
        assert!(context_item_enabled(
            TreeContextItem::OpenWith,
            true,
            false,
            false,
            true,
            false
        ));
        assert!(!context_item_enabled(
            TreeContextItem::OpenWith,
            true,
            true,
            false,
            true,
            false
        ));
        assert!(!context_item_enabled(
            TreeContextItem::OpenWith,
            true,
            false,
            false,
            true,
            true
        ));
    }

    #[test]
    fn context_item_enabled_open_as_admin_is_files_only_and_requires_online() {
        assert!(context_item_enabled(
            TreeContextItem::OpenAsAdmin,
            true,
            false,
            false,
            true,
            false
        ));
        assert!(!context_item_enabled(
            TreeContextItem::OpenAsAdmin,
            true,
            true,
            false,
            true,
            false
        ));
        assert!(!context_item_enabled(
            TreeContextItem::OpenAsAdmin,
            true,
            false,
            false,
            true,
            true
        ));
    }

    #[test]
    fn context_item_enabled_extract_requires_zip_file() {
        // zip fileなら有効。
        assert!(context_item_enabled(
            TreeContextItem::Extract,
            true,
            false,
            true,
            true,
            false
        ));
        // 非zip fileでは無効。
        assert!(!context_item_enabled(
            TreeContextItem::Extract,
            true,
            false,
            false,
            true,
            false
        ));
        // directoryでは無効(is_zipが立っていても)。
        assert!(!context_item_enabled(
            TreeContextItem::Extract,
            true,
            true,
            true,
            true,
            false
        ));
    }

    #[test]
    fn context_item_enabled_create_shortcut_allows_directories_but_needs_saved_entry() {
        assert!(context_item_enabled(
            TreeContextItem::CreateShortcut,
            true,
            true,
            false,
            true,
            false
        ));
        assert!(!context_item_enabled(
            TreeContextItem::CreateShortcut,
            false,
            true,
            false,
            true,
            false
        ));
    }

    #[test]
    fn context_item_enabled_clipboard_paste_ignores_id_but_requires_online() {
        assert!(context_item_enabled(
            TreeContextItem::ClipboardPaste,
            false,
            false,
            false,
            true,
            false
        ));
        assert!(!context_item_enabled(
            TreeContextItem::ClipboardPaste,
            false,
            false,
            false,
            true,
            true
        ));
    }

    #[test]
    fn context_item_enabled_refresh_only_requires_healthy_engine() {
        assert!(context_item_enabled(
            TreeContextItem::Refresh,
            false,
            true,
            false,
            true,
            true
        ));
        assert!(!context_item_enabled(
            TreeContextItem::Refresh,
            true,
            false,
            false,
            false,
            false
        ));
    }

    #[test]
    fn context_item_enabled_open_terminal_requires_online_but_allows_directories() {
        assert!(context_item_enabled(
            TreeContextItem::OpenTerminal,
            true,
            true,
            false,
            true,
            false
        ));
        assert!(!context_item_enabled(
            TreeContextItem::OpenTerminal,
            true,
            true,
            false,
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
            false,
            true,
            true
        ));
        assert!(context_item_enabled(
            TreeContextItem::CopyPath,
            true,
            false,
            false,
            true,
            true
        ));
    }

    #[test]
    fn context_item_disabled_reason_prefers_engine_then_offline_then_item_specific() {
        assert_eq!(
            context_item_disabled_reason(TreeContextItem::Extract, false, true, false, true, false),
            "Editor engine has stopped"
        );
        assert_eq!(
            context_item_disabled_reason(TreeContextItem::Extract, true, true, false, true, false),
            "Location is offline"
        );
        assert_eq!(
            context_item_disabled_reason(TreeContextItem::Extract, true, true, false, false, false),
            "Extract is for .zip archives only"
        );
        assert_eq!(
            context_item_disabled_reason(TreeContextItem::Extract, true, true, true, false, true),
            "Extract is for .zip archives only"
        );
        assert_eq!(
            context_item_disabled_reason(TreeContextItem::OpenWith, true, true, true, false, false),
            "Open with is for files only"
        );
        assert_eq!(
            context_item_disabled_reason(
                TreeContextItem::OpenAsAdmin,
                true,
                true,
                true,
                false,
                false
            ),
            "Open as administrator is for files only"
        );
        assert_eq!(
            context_item_disabled_reason(TreeContextItem::Open, true, false, false, false, false),
            "This entry has not been saved yet"
        );
    }

    #[test]
    fn context_item_label_reflects_directory_vs_file_for_open() {
        assert_eq!(
            context_item_label(TreeContextItem::Open, true),
            "Enter directory"
        );
        assert_eq!(context_item_label(TreeContextItem::Open, false), "Open");
        assert_eq!(
            context_item_label(TreeContextItem::OpenAsAdmin, false),
            "Open as administrator"
        );
        assert_eq!(
            context_item_label(TreeContextItem::ClipboardCopy, false),
            "Copy"
        );
        assert_eq!(
            context_item_label(TreeContextItem::ClipboardCut, false),
            "Cut"
        );
        assert_eq!(
            context_item_label(TreeContextItem::ClipboardPaste, false),
            "Paste"
        );
        assert_eq!(
            context_item_label(TreeContextItem::Extract, false),
            "Extract here"
        );
        assert_eq!(
            context_item_label(TreeContextItem::CreateShortcut, true),
            "Create shortcut"
        );
        assert_eq!(
            context_item_label(TreeContextItem::Refresh, true),
            "Refresh"
        );
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
        let binding = fyler_core::keymap::parse_key_sequence("xe", None).unwrap();
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

        let binding = fyler_core::keymap::parse_key_sequence("xe", None).unwrap();
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

    fn arrow_key_event(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    /// `read_picker_keys`は候補選択に使うキーをイベントキューから消費し、
    /// 同一フレームで後から描画される`TextEdit`にArrowUp/ArrowDown等が
    /// 届かないことを検証する(issue #38: 矢印キーが入力欄と候補選択の
    /// 両方に効いてしまう問題)。
    #[test]
    fn read_picker_keys_consumes_navigation_keys_from_the_event_queue() {
        let ctx = egui::Context::default();
        ctx.begin_pass(egui::RawInput {
            events: vec![
                arrow_key_event(egui::Key::ArrowDown, egui::Modifiers::NONE),
                arrow_key_event(egui::Key::ArrowUp, egui::Modifiers::NONE),
                arrow_key_event(egui::Key::P, egui::Modifiers::CTRL),
                arrow_key_event(egui::Key::N, egui::Modifiers::CTRL),
                arrow_key_event(egui::Key::Enter, egui::Modifiers::NONE),
                arrow_key_event(egui::Key::Enter, egui::Modifiers::CTRL),
                arrow_key_event(egui::Key::Escape, egui::Modifiers::NONE),
            ],
            ..Default::default()
        });

        let keys = read_picker_keys(&ctx);
        assert!(keys.next, "ArrowDown/Ctrl+N should register as `next`");
        assert!(
            keys.previous,
            "ArrowUp/Ctrl+P should register as `previous`"
        );
        assert!(keys.enter, "plain Enter should register as `enter`");
        assert!(
            keys.ctrl_enter,
            "Ctrl+Enter should register as `ctrl_enter`"
        );
        assert!(keys.escape, "Escape should register as `escape`");

        // 消費済みなので、この後に同じフレーム内で`TextEdit`が読んでも
        // 何も残っていない = カーソル移動やフォーカス解除が発生しない。
        ctx.input(|input| {
            assert!(
                input.events.is_empty(),
                "picker keys must be consumed, but {:?} remained",
                input.events
            );
        });

        let _ = ctx.end_pass();
    }

    /// query入力用の文字イベントは消費対象外であり、`TextEdit`まで届いて
    /// 従来どおりクエリ更新に使われることを確認する。
    #[test]
    fn read_picker_keys_does_not_consume_unrelated_text_input() {
        let ctx = egui::Context::default();
        ctx.begin_pass(egui::RawInput {
            events: vec![
                egui::Event::Text("a".to_owned()),
                arrow_key_event(egui::Key::ArrowDown, egui::Modifiers::NONE),
            ],
            ..Default::default()
        });

        let keys = read_picker_keys(&ctx);
        assert!(keys.next);
        ctx.input(|input| {
            assert_eq!(input.events, vec![egui::Event::Text("a".to_owned())]);
        });

        let _ = ctx.end_pass();
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

        let geometry = WindowGeometry::new(1200.0, 700.0, 30.0, 40.0, true, 1.5).unwrap();
        let options = native_options(Some(StartupWindow {
            geometry,
            apply_position: true,
        }));
        assert_eq!(options.viewport.inner_size, Some(egui::vec2(1200.0, 700.0)));
        assert_eq!(options.viewport.position, Some(egui::pos2(30.0, 40.0)));
        assert_eq!(options.viewport.maximized, Some(true));
        assert!(!options.persist_window);
    }

    #[test]
    fn native_options_omits_position_when_offscreen_guard_disables_it() {
        // オフスクリーンガードで`apply_position: false`になった場合、サイズと
        // maximizedだけ適用し、位置(`outer_x`/`outer_y`)はwith_positionへ渡さない。
        let geometry = WindowGeometry::new(1200.0, 700.0, 30.0, 40.0, true, 1.5).unwrap();
        let options = native_options(Some(StartupWindow {
            geometry,
            apply_position: false,
        }));
        assert_eq!(options.viewport.inner_size, Some(egui::vec2(1200.0, 700.0)));
        assert_eq!(options.viewport.position, None);
        assert_eq!(options.viewport.maximized, Some(true));
    }

    #[test]
    fn native_options_embed_a_non_empty_window_icon() {
        let icon = window_icon();
        assert!(icon.width > 0 && icon.height > 0);
        assert_eq!(
            icon.rgba.len(),
            icon.width as usize * icon.height as usize * 4
        );

        let options = native_options(None);
        assert!(options.viewport.icon.is_some());
        assert_eq!(options.viewport.decorations, Some(!cfg!(windows)));
        assert_eq!(options.viewport.transparent, Some(cfg!(windows)));
    }

    #[cfg(windows)]
    #[test]
    fn windows_native_options_use_transparent_dx12_composition_swapchain() {
        let options = native_options(None);
        assert_eq!(options.renderer, eframe::Renderer::Wgpu);
        let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = options.wgpu_options.wgpu_setup else {
            panic!("Windows native path must explicitly configure a new DX12 instance");
        };
        assert_eq!(
            setup.instance_descriptor.backends,
            eframe::wgpu::Backends::DX12
        );
        assert_eq!(
            setup
                .instance_descriptor
                .backend_options
                .dx12
                .presentation_system,
            eframe::wgpu::Dx12SwapchainKind::DxgiFromVisual
        );
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
            window_geometry_from_viewport(&viewport, 1.5),
            WindowGeometry::new(1000.0, 600.0, 30.0, 40.0, false, 1.5)
        );
        assert!(window_geometry_from_viewport(&egui::ViewportInfo::default(), 1.5).is_none());
    }

    #[test]
    fn position_correction_fires_at_most_once_when_position_drifts() {
        let target = egui::pos2(30.0, 40.0);
        let state = PositionCorrection::new(target);

        // outer_rectがまだ届いていないフレームは判定を持ち越す。
        let (state, corrected) = state.poll(None);
        assert!(!corrected);
        let state = state.expect("still waiting for outer_rect");

        // 保存位置から大きくズレた配置を観測したら、1回だけ補正すべきと判定する。
        let (state, corrected) = state.poll(Some(egui::pos2(500.0, 500.0)));
        assert!(corrected);
        assert!(state.is_none(), "state must be consumed after a decision");
    }

    #[test]
    fn position_correction_does_not_fire_when_position_matches() {
        let target = egui::pos2(30.0, 40.0);
        let state = PositionCorrection::new(target);
        let (state, corrected) = state.poll(Some(egui::pos2(30.4, 39.6)));
        assert!(
            !corrected,
            "sub-epsilon drift must not trigger a correction"
        );
        assert!(state.is_none());
    }

    #[test]
    fn position_correction_gives_up_after_max_frames_without_outer_rect() {
        let mut state = Some(PositionCorrection::new(egui::pos2(0.0, 0.0)));
        for _ in 0..POSITION_CORRECTION_MAX_FRAMES {
            let (next, corrected) = state.unwrap().poll(None);
            assert!(!corrected);
            state = next;
        }
        assert!(
            state.is_none(),
            "correction judging must not wait forever if outer_rect never appears"
        );
    }

    #[test]
    fn pointer_left_window_detects_exit_but_not_unknown_position() {
        let viewport = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 600.0));
        assert!(!pointer_left_window(Some(egui::pos2(10.0, 10.0)), viewport));
        assert!(pointer_left_window(Some(egui::pos2(-5.0, 10.0)), viewport));
        assert!(pointer_left_window(Some(egui::pos2(900.0, 10.0)), viewport));
        // 座標不明(latest_posがNone)は離脱と誤判定しない。
        assert!(!pointer_left_window(None, viewport));
    }

    #[test]
    fn resolve_drag_drop_target_ignores_source_pane_and_picks_hit_pane() {
        let source = PaneId::new(1);
        let target = PaneId::new(2);
        let rects = vec![
            (
                source,
                egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(100.0, 100.0)),
                Some(3),
            ),
            (
                target,
                egui::Rect::from_min_size(egui::pos2(100.0, 0.0), egui::vec2(100.0, 100.0)),
                Some(7),
            ),
        ];
        // 同一paneへのdropは何もしない(None)。
        assert_eq!(
            resolve_drag_drop_target(egui::pos2(50.0, 50.0), source, &rects),
            None
        );
        // 別paneへのdropはそのpaneとdrop対象行を返す。
        assert_eq!(
            resolve_drag_drop_target(egui::pos2(150.0, 50.0), source, &rects),
            Some((target, Some(7)))
        );
        // どのpane矩形にも入っていなければNone。
        assert_eq!(
            resolve_drag_drop_target(egui::pos2(500.0, 500.0), source, &rects),
            None
        );
    }
}
