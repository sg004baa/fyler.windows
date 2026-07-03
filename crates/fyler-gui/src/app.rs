//! eframeアプリ本体。毎フレーム、エンジンのsnapshotだけを描画する。

use std::sync::Arc;

use eframe::egui;
use fyler_core::editor::EditorEngine;

use crate::{input, modeline, tree_view};

/// fylerのGUIアプリケーション。
///
/// 描画契約:
/// - 毎フレーム [`EditorEngine::snapshot`] を1回だけ取得し、そのsnapshotのみで
///   描画する(lines/cursor/modeを別々のタイミングで読まない。整合性のため)
/// - RPC完了を同期待ちしない。入力は [`EditorEngine::send`] へ投げるだけ
pub struct FylerApp {
    pub engine: Arc<dyn EditorEngine>,
    // TODO(M1): EditorEventレシーバ(CommitRequested/CmdlineShow/Message/EngineCrashed)
    // TODO(M2): SaveState(fyler_core::save)、確認ダイアログ・validateエラーの表示状態
    // TODO(M2): EditContext(collapsed_dirs)の管理
}

impl FylerApp {
    pub fn new(engine: Arc<dyn EditorEngine>) -> Self {
        Self { engine }
    }
}

impl eframe::App for FylerApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // 入力 → エンジンへ(ノンブロッキング)
        input::forward_input(ui.ctx(), self.engine.as_ref());

        // 描画は単一snapshotのみを使う
        let snapshot = self.engine.snapshot();

        egui::Panel::bottom("modeline").show(ui, |ui| {
            modeline::draw(ui, &snapshot);
            // TODO(M1): cmdline::draw(EditorEvent::CmdlineShowの内容)とmessages表示
        });

        egui::CentralPanel::default().show(ui, |ui| {
            tree_view::draw(ui, &snapshot);
        });

        // TODO(M2): SaveStateがAwaitingConfirmationのとき confirm::draw をモーダル表示
        // TODO(M1): EngineCrashed受信時はエラー表示に切り替え、入力転送を止める
    }
}

/// GUIを起動する(メインスレッドで呼ぶこと。eframeの制約)。
///
/// 実装契約(M1):
/// - `eframe::run_native` で [`FylerApp`] を起動する
/// - エンジンのイベント(`EditorEvent`)受信で `ctx.request_repaint()` を呼び、
///   ポーリングなしで再描画されるようにする
pub fn run(engine: Arc<dyn EditorEngine>) -> anyhow::Result<()> {
    todo!("M1: eframe::run_nativeによる起動")
}
