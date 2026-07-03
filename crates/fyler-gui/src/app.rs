//! eframeアプリ本体。毎フレーム、エンジンのsnapshotだけを描画する。

use std::sync::{Arc, mpsc};
use std::thread;

use eframe::egui;
use fyler_core::editor::{CmdlineState, EditorEngine, EditorEvent, EditorMessage};
use fyler_core::plan::OperationPlan;
use fyler_core::validate::ValidateError;

use crate::confirm::ConfirmChoice;
use crate::{cmdline, confirm, input, modeline, tree_view};

/// app層からGUIへ渡す描画指示。
#[derive(Debug, Clone)]
pub enum GuiEvent {
    Editor(EditorEvent),
    ShowPlan(OperationPlan),
    ShowValidationErrors(Vec<ValidateError>),
    CloseDialog,
}

#[derive(Debug, Clone)]
enum DialogState {
    Plan(OperationPlan),
    ValidationErrors(Vec<ValidateError>),
}

/// fylerのGUIアプリケーション。
///
/// 描画契約:
/// - 毎フレーム [`EditorEngine::snapshot`] を1回だけ取得し、そのsnapshotのみで
///   描画する(lines/cursor/modeを別々のタイミングで読まない。整合性のため)
/// - RPC完了を同期待ちしない。入力は [`EditorEngine::send`] へ投げるだけ
pub struct FylerApp {
    pub engine: Arc<dyn EditorEngine>,
    event_rx: mpsc::Receiver<GuiEvent>,
    cmdline: Option<CmdlineState>,
    message: Option<EditorMessage>,
    engine_error: Option<String>,
    dialog: Option<DialogState>,
    confirm_tx: mpsc::Sender<ConfirmChoice>,
}

impl FylerApp {
    fn new(
        engine: Arc<dyn EditorEngine>,
        gui_events: mpsc::Receiver<GuiEvent>,
        confirm_tx: mpsc::Sender<ConfirmChoice>,
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
            engine,
            event_rx,
            cmdline: None,
            message: None,
            engine_error: None,
            dialog: None,
            confirm_tx,
        })
    }

    fn receive_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                GuiEvent::Editor(event) => match event {
                    EditorEvent::SnapshotUpdated => {}
                    EditorEvent::CommitRequested { .. } => {}
                    EditorEvent::CmdlineShow(state) => self.cmdline = Some(state),
                    EditorEvent::CmdlineHide => self.cmdline = None,
                    EditorEvent::Message(message) => self.message = Some(message),
                    EditorEvent::EngineCrashed { reason } => {
                        self.engine_error = Some(format!("編集エンジンが停止しました: {reason}"));
                    }
                },
                GuiEvent::ShowPlan(plan) => {
                    self.dialog = Some(DialogState::Plan(plan));
                }
                GuiEvent::ShowValidationErrors(errors) => {
                    self.dialog = Some(DialogState::ValidationErrors(errors));
                }
                GuiEvent::CloseDialog => self.dialog = None,
            }
        }
    }
}

impl eframe::App for FylerApp {
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.receive_events();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // 描画と入力判断は、フレーム冒頭に1回だけ得た同一snapshotを使う。
        let snapshot = self.engine.snapshot();

        if self.engine_error.is_none()
            && self.dialog.is_none()
            && let Err(error) = input::forward_input(ui.ctx(), self.engine.as_ref(), &snapshot.mode)
        {
            self.engine_error = Some(format!("編集エンジンへ入力を送信できません: {error}"));
        }

        egui::Panel::bottom("modeline").show(ui, |ui| {
            modeline::draw(ui, &snapshot);
            if let Some(state) = &self.cmdline {
                cmdline::draw_cmdline(ui, state);
            } else if let Some(message) = &self.message {
                cmdline::draw_message(ui, message);
            }
        });

        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(error) = &self.engine_error {
                ui.colored_label(ui.visuals().error_fg_color, error);
            } else {
                tree_view::draw(ui, &snapshot);
            }
        });

        let mut confirm_choice = None;
        let mut dismiss_errors = false;
        match &self.dialog {
            Some(DialogState::Plan(plan)) => {
                confirm_choice = confirm::draw_plan(ui, plan);
            }
            Some(DialogState::ValidationErrors(errors)) => {
                dismiss_errors = egui::Modal::new(egui::Id::new("save-validation-errors"))
                    .show(ui.ctx(), |ui| {
                        ui.heading("保存できません");
                        ui.add_space(8.0);
                        confirm::draw_validation_errors(ui, errors);
                        ui.add_space(12.0);
                        ui.button("Dismiss").clicked()
                    })
                    .inner;
            }
            None => {}
        }

        if dismiss_errors {
            self.dialog = None;
        }
        if let Some(choice) = confirm_choice
            && self.confirm_tx.send(choice).is_err()
        {
            self.engine_error = Some("確認結果をアプリへ送信できません".to_owned());
        }
    }
}

/// GUIを起動する(メインスレッドで呼ぶこと。eframeの制約)。
///
/// 実装契約(M1):
/// - `eframe::run_native` で [`FylerApp`] を起動する
/// - エンジンのイベント(`EditorEvent`)受信で `ctx.request_repaint()` を呼び、
///   ポーリングなしで再描画されるようにする
pub fn run(
    engine: Arc<dyn EditorEngine>,
    event_rx: mpsc::Receiver<GuiEvent>,
    confirm_tx: mpsc::Sender<ConfirmChoice>,
) -> anyhow::Result<()> {
    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "fyler",
        options,
        Box::new(move |creation_context| {
            let app = FylerApp::new(
                Arc::clone(&engine),
                event_rx,
                confirm_tx,
                creation_context.egui_ctx.clone(),
            )
            .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { error.into() })?;
            Ok(Box::new(app))
        }),
    )
    .map_err(|error| anyhow::anyhow!("GUIを起動できません: {error}"))
}
