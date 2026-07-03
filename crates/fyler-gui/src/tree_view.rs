//! ツリー本体の描画。

use eframe::egui;
use fyler_core::editor::EditorSnapshot;

/// snapshotのバッファ行をツリーとして描画する。
///
/// 実装契約:
/// - 各行は [`crate::conceal`] を通してから描く(生テキストを直接描かない)
/// - カーソルは [`crate::conceal::display_cursor`] の補正後座標に描く。
///   モードによって形を変える(Normal=ブロック、Insert=バー等)
/// - Visual系モードの選択範囲ハイライトもここ(M1はカーソルのみでよい)
/// - アイコン・git status・インデントガイドはバッファ文字列に含まれない
///   Rust側装飾として描く(M5)
pub fn draw(ui: &mut egui::Ui, snapshot: &EditorSnapshot) {
    todo!("M1: read-onlyのツリー描画(M1のゴール)")
}
