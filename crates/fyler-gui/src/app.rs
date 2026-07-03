//! eframeアプリ本体。毎フレーム、エンジンのsnapshotだけを描画する。

use std::sync::{Arc, mpsc};
use std::thread;

use eframe::egui;
use fyler_core::editor::{CmdlineState, EditorEngine, EditorEvent, EditorMessage, MessageKind};

use crate::{cmdline, input, modeline, tree_view};

/// fylerのGUIアプリケーション。
///
/// 描画契約:
/// - 毎フレーム [`EditorEngine::snapshot`] を1回だけ取得し、そのsnapshotのみで
///   描画する(lines/cursor/modeを別々のタイミングで読まない。整合性のため)
/// - RPC完了を同期待ちしない。入力は [`EditorEngine::send`] へ投げるだけ
pub struct FylerApp {
    pub engine: Arc<dyn EditorEngine>,
    event_rx: mpsc::Receiver<EditorEvent>,
    cmdline: Option<CmdlineState>,
    message: Option<EditorMessage>,
    engine_error: Option<String>,
    // TODO(M2): SaveState(fyler_core::save)、確認ダイアログ・validateエラーの表示状態
    // TODO(M2): EditContext(collapsed_dirs)の管理
}

impl FylerApp {
    fn new(
        engine: Arc<dyn EditorEngine>,
        engine_events: mpsc::Receiver<EditorEvent>,
        repaint_context: egui::Context,
    ) -> anyhow::Result<Self> {
        let (event_tx, event_rx) = mpsc::channel();
        thread::Builder::new()
            .name("fyler-editor-events".to_owned())
            .spawn(move || {
                while let Ok(event) = engine_events.recv() {
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
        })
    }

    fn receive_events(&mut self) {
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                EditorEvent::SnapshotUpdated => {}
                EditorEvent::CommitRequested { .. } => {
                    self.message = Some(EditorMessage {
                        kind: MessageKind::Info,
                        text: "M1では保存要求を実行しません".to_owned(),
                    });
                }
                EditorEvent::CmdlineShow(state) => self.cmdline = Some(state),
                EditorEvent::CmdlineHide => self.cmdline = None,
                EditorEvent::Message(message) => self.message = Some(message),
                EditorEvent::EngineCrashed { reason } => {
                    self.engine_error = Some(format!("編集エンジンが停止しました: {reason}"));
                }
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

        // TODO(M2): SaveStateがAwaitingConfirmationのとき confirm::draw をモーダル表示
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
    event_rx: mpsc::Receiver<EditorEvent>,
) -> anyhow::Result<()> {
    let options = eframe::NativeOptions::default();
    eframe::run_native(
        "fyler",
        options,
        Box::new(move |creation_context| {
            let app = FylerApp::new(
                Arc::clone(&engine),
                event_rx,
                creation_context.egui_ctx.clone(),
            )
            .map_err(|error| -> Box<dyn std::error::Error + Send + Sync> { error.into() })?;
            Ok(Box::new(app))
        }),
    )
    .map_err(|error| anyhow::anyhow!("GUIを起動できません: {error}"))
}
