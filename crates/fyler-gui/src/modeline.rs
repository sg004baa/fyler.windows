//! モードライン(NORMAL / INSERT / VISUAL ...)の描画。

use eframe::egui;
use fyler_core::editor::{EditorSnapshot, Mode};

use crate::conceal;

/// モード名・dirtyインジケータ・カーソル位置などを描く。
/// `Mode::Other(s)` は生文字列をそのまま表示する(隠さない)。
pub fn draw(ui: &mut egui::Ui, snapshot: &EditorSnapshot) {
    let mode = match &snapshot.mode {
        Mode::Normal => "NORMAL",
        Mode::Insert => "INSERT",
        Mode::Replace => "REPLACE",
        Mode::Visual => "VISUAL",
        Mode::VisualLine => "VISUAL LINE",
        Mode::VisualBlock => "VISUAL BLOCK",
        Mode::OperatorPending => "OPERATOR",
        Mode::Cmdline => "CMDLINE",
        Mode::Other(mode) => mode,
    };
    let dirty = if snapshot.dirty { " [+]" } else { "" };

    let (line, column) = snapshot
        .lines
        .get(snapshot.cursor.line)
        .map(|line| {
            let cursor = conceal::display_cursor(&line.text, snapshot.cursor);
            let display = conceal::conceal_line(&line.text).display;
            let byte_index = cursor.col.min(display.len());
            let byte_index = (0..=byte_index)
                .rev()
                .find(|index| display.is_char_boundary(*index))
                .unwrap_or_default();
            (cursor.line + 1, display[..byte_index].chars().count() + 1)
        })
        .unwrap_or((snapshot.cursor.line + 1, 1));

    ui.horizontal(|ui| {
        ui.monospace(format!("{mode}{dirty}"));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.monospace(format!("{line}:{column}"));
        });
    });
}
