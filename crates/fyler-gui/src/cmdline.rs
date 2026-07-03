//! cmdline(`:` / `/`)とメッセージの描画。
//!
//! cmdlineはユーザーに開放する(`:%s/old/new/` は中核機能。脅威モデル参照)。
//! 内容はエンジンから `EditorEvent::CmdlineShow` / `CmdlineHide` で届く
//! (NvimEngineではext_cmdline由来だが、GUIはそれを知らなくてよい)。
//! メッセージ(`E486: Pattern not found` 等)は `EditorEvent::Message` で届く。

use eframe::egui;
use fyler_core::editor::{CmdlineState, EditorMessage, MessageKind};

/// cmdline入力中の表示。プロンプト文字 + 内容 + カーソル。
pub fn draw_cmdline(ui: &mut egui::Ui, state: &CmdlineState) {
    let mut cursor = state.cursor.min(state.content.len());
    while !state.content.is_char_boundary(cursor) {
        cursor -= 1;
    }

    let before = &state.content[..cursor];
    let after = &state.content[cursor..];
    let cursor_char_len = after.chars().next().map(char::len_utf8).unwrap_or(0);
    let (under_cursor, after_cursor) = after.split_at(cursor_char_len);
    let under_cursor = if under_cursor.is_empty() {
        " "
    } else {
        under_cursor
    };

    let font_id = egui::TextStyle::Monospace.resolve(ui.style());
    let normal = egui::TextFormat {
        font_id: font_id.clone(),
        color: ui.visuals().text_color(),
        ..Default::default()
    };
    let cursor_format = egui::TextFormat {
        font_id,
        color: ui.visuals().selection.stroke.color,
        background: ui.visuals().selection.bg_fill,
        ..Default::default()
    };
    let mut job = egui::text::LayoutJob::default();
    job.append(&state.prompt.to_string(), 0.0, normal.clone());
    job.append(before, 0.0, normal.clone());
    job.append(under_cursor, 0.0, cursor_format);
    job.append(after_cursor, 0.0, normal);
    ui.label(job);
}

/// エディタメッセージの表示(Errorは目立つ色で)。
pub fn draw_message(ui: &mut egui::Ui, message: &EditorMessage) {
    let color = match message.kind {
        MessageKind::Info => ui.visuals().text_color(),
        MessageKind::Warn => ui.visuals().warn_fg_color,
        MessageKind::Error => ui.visuals().error_fg_color,
    };
    ui.colored_label(color, &message.text);
}
