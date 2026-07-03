//! モードライン(NORMAL / INSERT / VISUAL ...)の描画。

use eframe::egui;
use fyler_core::editor::EditorSnapshot;

/// モード名・dirtyインジケータ・カーソル位置などを描く。
/// `Mode::Other(s)` は生文字列をそのまま表示する(隠さない)。
pub fn draw(ui: &mut egui::Ui, snapshot: &EditorSnapshot) {
    todo!("M1: モードライン描画")
}
