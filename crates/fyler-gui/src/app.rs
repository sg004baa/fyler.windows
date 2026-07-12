//! eframeアプリ本体。毎フレーム、エンジンのsnapshotだけを描画する。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::thread;

use eframe::egui;
use fyler_core::editor::{
    CmdlineState, EditorEngine, EditorEvent, EditorMessage, Mode, PopupmenuState,
};
use fyler_core::feedback::{FeedbackKind, MAX_BODY_CHARS, validate_body};
use fyler_core::fileinfo::FileInfo;
use fyler_core::gitstatus::GitBadge;
use fyler_core::id::EntryId;
use fyler_core::pane::{PaneId, PaneLayout, SplitDirection};
use fyler_core::path::TreePath;
use fyler_core::plan::OperationPlan;
use fyler_core::report::{ApplyProgress, CommitReport};
use fyler_core::search::{SearchCandidate, SearchHit};
use fyler_core::transfer::{TransferOp, TransferPlan};
use fyler_core::validate::ValidateError;

use crate::confirm::{ConfirmChoice, ConfirmDetail, IconStyle};
use crate::{cmdline, confirm, input, modeline, tree_view};

const CJK_FONT_NAME: &str = "fyler-cjk";
const PICKER_RESULT_LIMIT: usize = 100;

/// ファイルpickerで候補を確定したときの動作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerAction {
    /// 対象をツリー上へ表示し、カーソルを移動する。
    Jump,
    /// OSの既定アプリケーションで対象を開く。
    Open,
}

/// GUIからapp層へ返すユーザー操作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuiAction {
    Confirm(ConfirmChoice),
    PickerSelect {
        pane_id: PaneId,
        path: TreePath,
        action: PickerAction,
    },
    FeedbackSubmit {
        kind: FeedbackKind,
        body: String,
    },
    FeedbackClosed,
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
    /// CJKフォントの上寄りを補正する、フォントサイズ比の下方向オフセット。
    ///
    /// CJKフォントはascent metricsが既定フォントと異なり上寄りに描画されるため、
    /// フォントサイズ比で下方向へずらす。`0`で無効。
    pub font_y_offset_factor: f32,
    /// ツリーへ描画するファイルアイコンのスタイル。
    pub icon_style: IconStyle,
    /// ヘルプダイアログへ表示する、エンジン非依存表記の行。
    pub help_lines: Vec<String>,
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
    /// 指定paneのbaselineから構築した候補でファイルpickerを開く。
    ShowFilePicker {
        pane_id: PaneId,
        candidates: Vec<SearchCandidate>,
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
    Progress {
        completed: usize,
        total: usize,
        /// これから実行する操作の表示ラベル。
        current: Option<String>,
        cancel_requested: bool,
    },
    Report(CommitReport),
    UndoReport {
        lines: Vec<String>,
        any_failed: bool,
    },
    TransferReport(CommitReport<TransferOp>),
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
        candidates: Vec<SearchCandidate>,
        query: String,
        selected: usize,
        hits: Vec<SearchHit>,
    },
    Feedback {
        kind: FeedbackKind,
        body: String,
        stage: FeedbackStage,
    },
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FeedbackStage {
    Input,
    Confirm,
    Sending,
    Done(&'static str),
    Failed(FeedbackResultKind, &'static str),
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
    icon_style: IconStyle,
    help_lines: Vec<String>,
}

struct PaneViewState {
    engine: Arc<dyn EditorEngine>,
    root: PathBuf,
    git_badges: HashMap<EntryId, GitBadge>,
    incomplete_dirs: HashSet<EntryId>,
    offline: bool,
    unreadable: usize,
    file_infos: HashMap<EntryId, FileInfo>,
    collapsed_dirs: HashSet<EntryId>,
    engine_error: Option<String>,
    last_cursor_line: usize,
    tree_viewport: Option<tree_view::TreeViewport>,
}

impl FylerApp {
    fn new(
        gui_events: mpsc::Receiver<GuiEvent>,
        action_tx: mpsc::Sender<GuiAction>,
        confirm_detail: ConfirmDetail,
        icon_style: IconStyle,
        help_lines: Vec<String>,
        repaint_context: egui::Context,
        event_dequeued: Arc<dyn Fn() + Send + Sync>,
    ) -> anyhow::Result<Self> {
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
            icon_style,
            help_lines,
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
                            incomplete_dirs: HashSet::new(),
                            offline: false,
                            unreadable: 0,
                            file_infos: HashMap::new(),
                            collapsed_dirs: HashSet::new(),
                            engine_error: None,
                            last_cursor_line: 0,
                            tree_viewport: None,
                        },
                    );
                }
                GuiEvent::RemovePane(pane_id) => {
                    self.panes.remove(&pane_id);
                    if matches!(
                        self.dialog,
                        Some(DialogState::FilePicker {
                            pane_id: owner,
                            ..
                        }) if owner == pane_id
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
                    EditorEvent::ChangeDirectory { .. } => {}
                    EditorEvent::ChangeSort { .. } => {}
                    EditorEvent::ToggleHidden => {}
                    EditorEvent::Fold { .. } => {}
                    EditorEvent::JumpBookmark { .. } => {}
                    EditorEvent::OpenFilePicker => {}
                    EditorEvent::FeedbackRequested => {}
                    EditorEvent::ShowHelp => self.dialog = Some(DialogState::Help),
                    EditorEvent::PaneAction(_) => {}
                    EditorEvent::TransferRequested { .. } => {}
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
                            self.dialog,
                            Some(DialogState::FilePicker {
                                pane_id: owner,
                                ..
                            }) if owner == pane_id
                        ) {
                            self.dialog = None;
                        }
                        if let Some(pane) = self.panes.get_mut(&pane_id) {
                            pane.engine_error = Some(format!("Editor engine stopped: {reason}"));
                        }
                    }
                },
                GuiEvent::RootChanged { pane_id, root } => {
                    if let Some(pane) = self.panes.get_mut(&pane_id) {
                        pane.root = root;
                    }
                }
                GuiEvent::GitBadges { pane_id, badges } => {
                    if let Some(pane) = self.panes.get_mut(&pane_id) {
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
                GuiEvent::ShowFilePicker {
                    pane_id,
                    candidates,
                } => {
                    let hits = fyler_core::search::search(&candidates, "", PICKER_RESULT_LIMIT);
                    self.dialog = Some(DialogState::FilePicker {
                        pane_id,
                        candidates,
                        query: String::new(),
                        selected: 0,
                        hits,
                    });
                    self.picker_needs_focus = true;
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
                GuiEvent::ShowApplyProgress { total } => {
                    self.dialog = Some(DialogState::Progress {
                        completed: 0,
                        total,
                        current: None,
                        cancel_requested: false,
                    });
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
        if should_forward_input(
            self.dialog.is_none(),
            self.fatal_error.is_none(),
            self.active
                .and_then(|active| self.panes.get(&active))
                .is_some_and(|pane| pane.engine_error.is_none()),
        ) && let Some(active) = self.active
            && let (Some(pane), Some(snapshot)) =
                (self.panes.get_mut(&active), snapshots.get(&active))
            && let Err(error) = input::forward_input(ui.ctx(), pane.engine.as_ref(), &snapshot.mode)
        {
            pane.engine_error = Some(format!("Failed to send input to editor engine: {error}"));
        }

        egui::Panel::bottom("global-command-area").show(ui, |ui| {
            if let Some(state) = &self.popupmenu {
                cmdline::draw_popupmenu(ui, state);
            }
            if let Some(state) = &self.cmdline {
                cmdline::draw_cmdline(ui, state);
            } else if let Some(message) = &self.message {
                cmdline::draw_message(ui, message);
            }
        });

        let layout = self.layout.clone();
        let active = self.active;
        let fatal_error = self.fatal_error.clone();
        let ime = egui::CentralPanel::default()
            .show(ui, |ui| {
                if let Some(error) = fatal_error {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                    None
                } else if let (Some(layout), Some(active)) = (layout.as_ref(), active) {
                    draw_layout(
                        ui,
                        layout,
                        active,
                        &mut self.panes,
                        &snapshots,
                        self.icon_style,
                    )
                } else {
                    None
                }
            })
            .inner;
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

        let mut confirm_choice = None;
        let mut cancel_apply = false;
        let mut dismiss_errors = false;
        let mut dismiss_report = false;
        let mut open_with_choice = None;
        let mut picker_result = None;
        let mut feedback_result = None;
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
            Some(DialogState::Help) => {
                dismiss_errors = draw_help(ui, &self.help_lines);
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
                candidates,
                query,
                selected,
                hits,
            }) => {
                picker_result = draw_file_picker(
                    ui,
                    *pane_id,
                    candidates,
                    query,
                    selected,
                    hits,
                    &mut self.picker_needs_focus,
                );
            }
            Some(DialogState::Feedback { kind, body, stage }) => {
                feedback_result =
                    draw_feedback(ui, kind, body, stage, &mut self.feedback_needs_focus);
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
        if let Some(result) = picker_result {
            self.dialog = None;
            if let Some(action) = result
                && self.action_tx.send(action).is_err()
            {
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
    }
}

fn should_forward_input(dialog_absent: bool, fatal_absent: bool, engine_healthy: bool) -> bool {
    dialog_absent && fatal_absent && engine_healthy
}

struct ImeGeometry {
    tree_rect: egui::Rect,
    cursor_rect: egui::Rect,
}

fn draw_layout(
    ui: &mut egui::Ui,
    layout: &PaneLayout,
    active: PaneId,
    panes: &mut BTreeMap<PaneId, PaneViewState>,
    snapshots: &BTreeMap<PaneId, Arc<fyler_core::editor::EditorSnapshot>>,
    icon_style: IconStyle,
) -> Option<ImeGeometry> {
    let rect = ui.available_rect_before_wrap();
    ui.allocate_rect(rect, egui::Sense::hover());
    draw_layout_in_rect(ui, rect, layout, active, panes, snapshots, icon_style)
}

#[allow(clippy::too_many_arguments)]
fn draw_layout_in_rect(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    layout: &PaneLayout,
    active: PaneId,
    panes: &mut BTreeMap<PaneId, PaneViewState>,
    snapshots: &BTreeMap<PaneId, Arc<fyler_core::editor::EditorSnapshot>>,
    icon_style: IconStyle,
) -> Option<ImeGeometry> {
    match layout {
        PaneLayout::Leaf(id) => {
            let pane = panes.get_mut(id)?;
            let snapshot = snapshots.get(id)?;
            let stroke = if *id == active {
                egui::Stroke::new(2.0, ui.visuals().selection.stroke.color)
            } else {
                egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color)
            };
            ui.painter()
                .rect_stroke(rect, 0.0, stroke, egui::StrokeKind::Inside);

            let inner = rect.shrink(4.0);
            let modeline_height = ui.text_style_height(&egui::TextStyle::Monospace) + 8.0;
            let tree_rect = egui::Rect::from_min_max(
                inner.min,
                egui::pos2(
                    inner.max.x,
                    (inner.max.y - modeline_height).max(inner.min.y),
                ),
            );
            let modeline_rect =
                egui::Rect::from_min_max(egui::pos2(inner.min.x, tree_rect.max.y), inner.max);
            let cursor_changed = snapshot.cursor.line != pane.last_cursor_line;
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
                            icon_style,
                            cursor_changed,
                            pane.tree_viewport,
                            *id,
                        ))
                    }
                })
                .inner;
            ui.scope_builder(egui::UiBuilder::new().max_rect(modeline_rect), |ui| {
                modeline::draw(
                    ui,
                    snapshot,
                    &pane.root,
                    &pane.file_infos,
                    pane.offline,
                    pane.unreadable,
                    pane.engine_error.is_some(),
                );
            });
            pane.last_cursor_line = snapshot.cursor.line;
            let output = output?;
            pane.tree_viewport = Some(output.viewport);
            if *id == active
                && matches!(snapshot.mode, Mode::Insert | Mode::Replace | Mode::Cmdline)
            {
                output.cursor_rect.map(|cursor_rect| ImeGeometry {
                    tree_rect: output.tree_rect,
                    cursor_rect,
                })
            } else {
                None
            }
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
            let first_ime =
                draw_layout_in_rect(ui, first_rect, first, active, panes, snapshots, icon_style);
            let second_ime = draw_layout_in_rect(
                ui,
                second_rect,
                second,
                active,
                panes,
                snapshots,
                icon_style,
            );
            first_ime.or(second_ime)
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
    candidates: &[SearchCandidate],
    hits: &[SearchHit],
    selected: &mut usize,
) -> Option<Option<GuiAction>> {
    if keys.escape {
        return Some(None);
    }
    if hits.is_empty() {
        *selected = 0;
        return None;
    }
    if keys.previous {
        *selected = selected.saturating_sub(1);
    }
    if keys.next {
        *selected = (*selected + 1).min(hits.len() - 1);
    }
    *selected = (*selected).min(hits.len() - 1);

    let action = if keys.ctrl_enter {
        Some(PickerAction::Open)
    } else if keys.enter {
        Some(PickerAction::Jump)
    } else {
        None
    }?;
    let candidate = candidates.get(hits[*selected].index)?;
    Some(Some(GuiAction::PickerSelect {
        pane_id,
        path: candidate.path.clone(),
        action,
    }))
}

fn update_picker_hits(
    candidates: &[SearchCandidate],
    query: &str,
    selected: &mut usize,
    hits: &mut Vec<SearchHit>,
) {
    *hits = fyler_core::search::search(candidates, query, PICKER_RESULT_LIMIT);
    *selected = 0;
}

fn draw_file_picker(
    ui: &mut egui::Ui,
    pane_id: PaneId,
    candidates: &[SearchCandidate],
    query: &mut String,
    selected: &mut usize,
    hits: &mut Vec<SearchHit>,
    needs_focus: &mut bool,
) -> Option<Option<GuiAction>> {
    let keys = read_picker_keys(ui.ctx());
    let mut clicked_selection = None;
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
        if response.changed() {
            update_picker_hits(candidates, query, selected, hits);
        }

        ui.add_space(6.0);
        egui::ScrollArea::vertical()
            .id_salt("fyler-file-picker-results")
            .max_height(360.0)
            .show(ui, |ui| {
                for (position, hit) in hits.iter().enumerate() {
                    let Some(candidate) = candidates.get(hit.index) else {
                        continue;
                    };
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
    apply_picker_keys(keys, pane_id, candidates, hits, selected)
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

fn draw_help(ui: &mut egui::Ui, help_lines: &[String]) -> bool {
    let dismiss_from_keyboard = ui
        .ctx()
        .input(|input| input.key_pressed(egui::Key::Enter) || input.key_pressed(egui::Key::Escape));

    egui::Modal::new(egui::Id::new("fyler-help"))
        .show(ui.ctx(), |ui| {
            ui.heading("Help");
            ui.add_space(8.0);
            for line in help_lines {
                ui.monospace(line);
            }
            ui.add_space(12.0);
            ui.button("Dismiss (Enter / Esc)").clicked() || dismiss_from_keyboard
        })
        .inner
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
) -> anyhow::Result<()> {
    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "fyler",
        native_options,
        Box::new(move |creation_context| {
            let GuiOptions {
                confirm_detail,
                font_path,
                font_y_offset_factor,
                icon_style,
                help_lines,
            } = gui_options;
            install_fallback_font(
                &creation_context.egui_ctx,
                font_path.as_deref(),
                font_y_offset_factor,
            );
            let app = FylerApp::new(
                event_rx,
                action_tx,
                confirm_detail,
                icon_style,
                help_lines,
                creation_context.egui_ctx.clone(),
                event_dequeued,
            )
            .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { error.into() })?;
            Ok(Box::new(app))
        }),
    )
    .map_err(|error| anyhow::anyhow!("Failed to start GUI: {error}"))
}

/// 指定パスを優先し、存在しなければ候補列の先頭から利用可能なパスを返す。
fn resolve_font_path(configured: Option<&Path>, candidates: &[PathBuf]) -> Option<PathBuf> {
    configured
        .filter(|path| path.exists())
        .map(Path::to_path_buf)
        .or_else(|| candidates.iter().find(|path| path.exists()).cloned())
}

fn install_fallback_font(context: &egui::Context, configured: Option<&Path>, y_offset_factor: f32) {
    let candidates = default_font_candidates();
    let Some(path) = resolve_font_path(configured, &candidates) else {
        return;
    };
    let Ok(bytes) = fs::read(path) else {
        return;
    };

    let mut definitions = egui::FontDefinitions::default();
    definitions.font_data.insert(
        CJK_FONT_NAME.to_owned(),
        Arc::new(egui::FontData::from_owned(bytes).tweak(egui::FontTweak {
            y_offset_factor,
            ..Default::default()
        })),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        definitions
            .families
            .entry(family)
            .or_default()
            .push(CJK_FONT_NAME.to_owned());
    }
    context.set_fonts(definitions);
}

fn default_font_candidates() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let Some(windows_directory) = std::env::var_os("WINDIR").map(PathBuf::from) else {
            return Vec::new();
        };
        let fonts = windows_directory.join("Fonts");
        vec![
            fonts.join("YuGothM.ttc"),
            fonts.join("meiryo.ttc"),
            fonts.join("msgothic.ttc"),
        ]
    }

    #[cfg(not(windows))]
    {
        vec![PathBuf::from(
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        )]
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

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

    fn candidate(path: &str, kind: fyler_core::tree::EntryKind) -> SearchCandidate {
        let display = path.to_owned();
        let key = display.to_lowercase();
        let name_offset = key.rfind('/').map_or(0, |offset| offset + 1);
        SearchCandidate {
            path: TreePath::parse(path),
            kind,
            display,
            key,
            name_offset,
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
                icon_style: IconStyle::Ascii,
                help_lines: Vec::new(),
            },
            event_tx,
            action_rx,
        )
    }

    static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let suffix = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "fyler-gui-font-test-{}-{suffix}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn resolve_font_path_prefers_existing_configured_path() {
        let directory = TestDirectory::new();
        let configured = directory.path().join("configured.ttf");
        let candidate = directory.path().join("candidate.ttf");
        fs::write(&configured, b"configured").unwrap();
        fs::write(&candidate, b"candidate").unwrap();

        assert_eq!(
            resolve_font_path(Some(&configured), &[candidate]),
            Some(configured)
        );
    }

    #[test]
    fn resolve_font_path_falls_back_to_first_existing_candidate() {
        let directory = TestDirectory::new();
        let missing_configured = directory.path().join("missing-configured.ttf");
        let missing_candidate = directory.path().join("missing-candidate.ttf");
        let existing_candidate = directory.path().join("existing-candidate.ttf");
        fs::write(&existing_candidate, b"candidate").unwrap();

        assert_eq!(
            resolve_font_path(
                Some(&missing_configured),
                &[missing_candidate, existing_candidate.clone()]
            ),
            Some(existing_candidate)
        );
    }

    #[test]
    fn resolve_font_path_returns_none_when_every_path_is_missing() {
        let directory = TestDirectory::new();

        assert_eq!(
            resolve_font_path(
                Some(&directory.path().join("missing-configured.ttf")),
                &[
                    directory.path().join("missing-a.ttf"),
                    directory.path().join("missing-b.ttf"),
                ]
            ),
            None
        );
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
                        incomplete_dirs: HashSet::new(),
                        offline: false,
                        unreadable: 0,
                        file_infos: HashMap::new(),
                        collapsed_dirs: HashSet::new(),
                        engine_error: None,
                        last_cursor_line: 0,
                        tree_viewport: None,
                    },
                ),
                (
                    second,
                    PaneViewState {
                        engine: recording_engine(),
                        root: PathBuf::from("second"),
                        git_badges: HashMap::new(),
                        incomplete_dirs: HashSet::new(),
                        offline: false,
                        unreadable: 0,
                        file_infos: HashMap::new(),
                        collapsed_dirs: HashSet::new(),
                        engine_error: None,
                        last_cursor_line: 0,
                        tree_viewport: None,
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
            icon_style: IconStyle::Ascii,
            help_lines: Vec::new(),
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
    fn picker_query_update_recalculates_hits_and_resets_selection() {
        let candidates = vec![
            candidate("src/main.rs", fyler_core::tree::EntryKind::File),
            candidate("README.md", fyler_core::tree::EntryKind::File),
        ];
        let mut selected = 1;
        let mut hits = fyler_core::search::search(&candidates, "", PICKER_RESULT_LIMIT);

        update_picker_hits(&candidates, "read", &mut selected, &mut hits);

        assert_eq!(selected, 0);
        assert_eq!(hits.len(), 1);
        assert_eq!(candidates[hits[0].index].path, TreePath::parse("README.md"));
    }

    #[test]
    fn picker_keys_close_move_jump_and_open() {
        let pane = PaneId::new(3);
        let candidates = vec![
            candidate("first", fyler_core::tree::EntryKind::File),
            candidate("second", fyler_core::tree::EntryKind::File),
        ];
        let hits = fyler_core::search::search(&candidates, "", PICKER_RESULT_LIMIT);
        let mut selected = 0;

        assert_eq!(
            apply_picker_keys(
                PickerKeys {
                    next: true,
                    ..Default::default()
                },
                pane,
                &candidates,
                &hits,
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
                &candidates,
                &hits,
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
                &candidates,
                &hits,
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
                &candidates,
                &hits,
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
                &candidates,
                &hits,
                &mut selected,
            ),
            Some(None)
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
                .send(GuiEvent::ShowFilePicker {
                    pane_id: pane,
                    candidates: vec![candidate("file.txt", fyler_core::tree::EntryKind::File)],
                })
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
}
