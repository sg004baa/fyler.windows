//! eframeアプリ本体。毎フレーム、エンジンのsnapshotだけを描画する。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::thread;

use eframe::egui;
use fyler_core::editor::{CmdlineState, EditorEngine, EditorEvent, EditorMessage, Mode};
use fyler_core::fileinfo::FileInfo;
use fyler_core::gitstatus::GitBadge;
use fyler_core::id::EntryId;
use fyler_core::pane::{PaneId, PaneLayout, SplitDirection};
use fyler_core::path::TreePath;
use fyler_core::plan::OperationPlan;
use fyler_core::report::{ApplyProgress, CommitReport};
use fyler_core::transfer::{TransferOp, TransferPlan};
use fyler_core::validate::ValidateError;

use crate::confirm::{ConfirmChoice, ConfirmDetail, IconStyle};
use crate::{cmdline, confirm, input, modeline, tree_view};

const CJK_FONT_NAME: &str = "fyler-cjk";

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
    /// 指定された表示用パスをクリップボードへコピーする。
    CopyPath(String),
    /// 保存planと実行前に確認すべき警告を表示する。
    ShowPlan {
        plan: OperationPlan,
        warnings: Vec<String>,
        /// 承認時に既存実体をごみ箱へ退避する移動先。plan順。
        overwrites: Vec<TreePath>,
    },
    /// apply開始時に操作総数を設定して進捗ダイアログを表示する。
    ShowApplyProgress {
        /// 承認済みplanに含まれる操作総数。
        total: usize,
    },
    /// apply workerから届いた操作単位の進捗を表示へ反映する。
    ApplyProgress(ApplyProgress),
    ShowTransferPlan {
        plan: TransferPlan,
        target: PaneId,
        overwrites: Vec<PathBuf>,
    },
    TransferProgress(ApplyProgress<TransferOp>),
    /// キャンセル要求を受理済みとして進捗ダイアログの操作を無効化する。
    ApplyCancelRequested,
    ShowReport(CommitReport),
    ShowTransferReport(CommitReport<TransferOp>),
    ShowValidationErrors(Vec<ValidateError>),
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
    TransferReport(CommitReport<TransferOp>),
    ValidationErrors(Vec<ValidateError>),
    Help,
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
    cmdline: Option<CmdlineState>,
    message: Option<EditorMessage>,
    pending_copy: Option<String>,
    fatal_error: Option<String>,
    dialog: Option<DialogState>,
    confirm_tx: mpsc::Sender<ConfirmChoice>,
    confirm_detail: ConfirmDetail,
    icon_style: IconStyle,
}

struct PaneViewState {
    engine: Arc<dyn EditorEngine>,
    root: PathBuf,
    git_badges: HashMap<EntryId, GitBadge>,
    file_infos: HashMap<EntryId, FileInfo>,
    collapsed_dirs: HashSet<EntryId>,
    engine_error: Option<String>,
    last_cursor_line: usize,
    tree_viewport: Option<tree_view::TreeViewport>,
}

impl FylerApp {
    fn new(
        gui_events: mpsc::Receiver<GuiEvent>,
        confirm_tx: mpsc::Sender<ConfirmChoice>,
        confirm_detail: ConfirmDetail,
        icon_style: IconStyle,
        repaint_context: egui::Context,
    ) -> anyhow::Result<Self> {
        let (event_tx, event_rx) = mpsc::channel();
        thread::Builder::new()
            .name("fyler-editor-events".to_owned())
            .spawn(move || {
                while let Ok(event) = gui_events.recv() {
                    if event_tx.send(event).is_err() {
                        return;
                    }
                    repaint_context.request_repaint();
                }
            })
            .map_err(|error| anyhow::anyhow!("エディタイベント監視を開始できません: {error}"))?;

        Ok(Self {
            panes: BTreeMap::new(),
            layout: None,
            active: None,
            event_rx,
            cmdline: None,
            message: None,
            pending_copy: None,
            fatal_error: None,
            dialog: None,
            confirm_tx,
            confirm_detail,
            icon_style,
        })
    }

    fn receive_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
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
                }
                GuiEvent::LayoutChanged { layout, active } => {
                    self.layout = Some(layout);
                    self.active = Some(active);
                }
                GuiEvent::Editor { pane_id, event } => match event {
                    EditorEvent::SnapshotUpdated => {}
                    EditorEvent::ActivateLine { .. } => {}
                    EditorEvent::YankPath { .. } => {}
                    EditorEvent::NavigateInto { .. } => {}
                    EditorEvent::NavigateParent => {}
                    EditorEvent::ChangeDirectory { .. } => {}
                    EditorEvent::ToggleHidden => {}
                    EditorEvent::JumpBookmark { .. } => {}
                    EditorEvent::ShowHelp => self.dialog = Some(DialogState::Help),
                    EditorEvent::PaneAction(_) => {}
                    EditorEvent::TransferRequested { .. } => {}
                    EditorEvent::CommitRequested { .. } => {}
                    EditorEvent::CmdlineShow(state) if self.active == Some(pane_id) => {
                        self.cmdline = Some(state);
                    }
                    EditorEvent::CmdlineShow(_) => {}
                    EditorEvent::CmdlineHide if self.active == Some(pane_id) => {
                        self.cmdline = None;
                    }
                    EditorEvent::CmdlineHide => {}
                    EditorEvent::Message(message) => self.message = Some(message),
                    EditorEvent::EngineCrashed { reason } => {
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
                GuiEvent::CopyPath(path) => self.pending_copy = Some(path),
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
                GuiEvent::ShowTransferReport(report) => {
                    self.dialog = Some(DialogState::TransferReport(report));
                }
                GuiEvent::ShowValidationErrors(errors) => {
                    self.dialog = Some(DialogState::ValidationErrors(errors));
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
        if self.dialog.is_none()
            && self.fatal_error.is_none()
            && let Some(active) = self.active
            && let (Some(pane), Some(snapshot)) =
                (self.panes.get_mut(&active), snapshots.get(&active))
            && pane.engine_error.is_none()
            && let Err(error) = input::forward_input(ui.ctx(), pane.engine.as_ref(), &snapshot.mode)
        {
            pane.engine_error = Some(format!("Failed to send input to editor engine: {error}"));
        }

        egui::Panel::bottom("global-command-area").show(ui, |ui| {
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
        if let Some(ime) = ime {
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
        match &self.dialog {
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
            Some(DialogState::Report(report)) => {
                dismiss_report = confirm::draw_report(ui, report);
            }
            Some(DialogState::TransferReport(report)) => {
                dismiss_report = confirm::draw_transfer_report(ui, report);
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
                dismiss_errors = draw_help(ui);
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
            None => {}
        }

        if dismiss_errors {
            self.dialog = None;
        }
        if dismiss_report {
            self.dialog = None;
        }
        if let Some(choice) = confirm_choice
            && self.confirm_tx.send(choice).is_err()
        {
            self.fatal_error = Some("Failed to send confirmation result to app".to_owned());
        }
        if cancel_apply && self.confirm_tx.send(ConfirmChoice::Cancel).is_err() {
            self.fatal_error = Some("Failed to send cancel request to app".to_owned());
        }
    }
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
                modeline::draw(ui, snapshot, &pane.root, &pane.file_infos);
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

fn draw_help(ui: &mut egui::Ui) -> bool {
    let dismiss_from_keyboard = ui
        .ctx()
        .input(|input| input.key_pressed(egui::Key::Enter) || input.key_pressed(egui::Key::Escape));

    egui::Modal::new(egui::Id::new("fyler-help"))
        .show(ui.ctx(), |ui| {
            ui.heading("Help");
            ui.add_space(8.0);
            for line in [
                "<CR>  Toggle directory / open file",
                "gd    Enter directory",
                "^     Go to parent",
                "g.    Toggle hidden files",
                "gy    Copy path",
                "gm/gc Move/copy to previous pane",
                "<C-w>s/v  Split pane",
                "<C-w>h/j/k/l/w/p  Focus pane",
                "<C-w>q/c  Close pane",
                ":w    Save changes",
                ":cd   Change root",
                ":b    Bookmarks and recent roots",
                "?     Show this help",
            ] {
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
    confirm_tx: mpsc::Sender<ConfirmChoice>,
    gui_options: GuiOptions,
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
            } = gui_options;
            install_fallback_font(
                &creation_context.egui_ctx,
                font_path.as_deref(),
                font_y_offset_factor,
            );
            let app = FylerApp::new(
                event_rx,
                confirm_tx,
                confirm_detail,
                icon_style,
                creation_context.egui_ctx.clone(),
            )
            .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { error.into() })?;
            Ok(Box::new(app))
        }),
    )
    .map_err(|error| anyhow::anyhow!("GUIを起動できません: {error}"))
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
        let (confirm_tx, _confirm_rx) = mpsc::channel();
        let mut app = FylerApp {
            panes: BTreeMap::from([
                (
                    first,
                    PaneViewState {
                        engine: recording_engine(),
                        root: PathBuf::from("first"),
                        git_badges: HashMap::new(),
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
            cmdline: None,
            message: None,
            pending_copy: None,
            fatal_error: None,
            dialog: None,
            confirm_tx,
            confirm_detail: ConfirmDetail::Full,
            icon_style: IconStyle::Ascii,
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

        app.receive_events();

        assert_eq!(app.panes[&first].root, PathBuf::from("first"));
        assert!(app.panes[&first].git_badges.is_empty());
        assert_eq!(app.panes[&second].root, PathBuf::from("changed"));
        assert_eq!(
            app.panes[&second].git_badges.get(&EntryId(9)),
            Some(&GitBadge::Modified)
        );
    }
}
