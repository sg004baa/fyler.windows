//! cmdline(`:` / `/`)とメッセージの描画。
//!
//! cmdlineはユーザーに開放する(`:%s/old/new/` は中核機能。脅威モデル参照)。
//! 内容はエンジンから `EditorEvent::CmdlineShow` / `CmdlineHide` で届く
//! (NvimEngineではext_cmdline由来だが、GUIはそれを知らなくてよい)。
//! メッセージ(`E486: Pattern not found` 等)は `EditorEvent::Message` で届く。

use eframe::egui;
use fyler_core::editor::{CmdlineState, EditorMessage};

/// cmdline入力中の表示。プロンプト文字 + 内容 + カーソル。
pub fn draw_cmdline(ui: &mut egui::Ui, state: &CmdlineState) {
    todo!("M1: cmdline描画")
}

/// エディタメッセージの表示(Errorは目立つ色で)。
pub fn draw_message(ui: &mut egui::Ui, message: &EditorMessage) {
    todo!("M1: メッセージ描画")
}
