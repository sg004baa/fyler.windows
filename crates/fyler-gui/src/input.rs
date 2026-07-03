//! 入力のエンジン転送: eguiイベント → `EditorCommand`。

use eframe::egui;
use fyler_core::editor::EditorEngine;

/// このフレームの入力イベントをエンジンへ転送する。
///
/// 実装契約(DESIGN.md「EditorEngineトレイト」):
/// - `egui::Event::Key` → `fyler_core::editor::KeyInput` に正規化して
///   `EditorCommand::Key`。印字可能文字はShift適用済みの `Key::Char` にする
/// - `egui::Event::Text`(**IME確定文字列・日本語入力を含む**)→
///   `EditorCommand::Text`。Keyと二重送信しないこと(egui側でKeyとTextの両方が
///   来る文字の扱いをM0スパイクで確定する)
/// - `egui::Event::Paste` → `EditorCommand::Paste`
/// - 確認ダイアログ表示中はエンジンへ転送せずダイアログが入力を消費する
/// - 送信は失敗し得る(エンジン停止)。失敗はエラー表示へ回す(panicしない)
pub fn forward_input(ctx: &egui::Context, engine: &dyn EditorEngine) {
    todo!("M1: 入力転送(IME経路はM0スパイクで検証済みの方式に従う)")
}
