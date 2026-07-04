//! ツリー本体の描画。

use eframe::egui;
use fyler_core::editor::{EditorSnapshot, Mode};

use crate::{conceal, icon};

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
    let font_id = egui::TextStyle::Monospace.resolve(ui.style());
    let text_color = ui.visuals().text_color();
    let row_height = ui.text_style_height(&egui::TextStyle::Monospace);

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for (line_index, line) in snapshot.lines.iter().enumerate() {
                let concealed = conceal::conceal_line(&line.text);
                let painter = ui.painter().clone();
                let icon_galley = painter.layout_no_wrap(
                    format!("{} ", icon::for_display_name(concealed.display)),
                    font_id.clone(),
                    text_color,
                );
                let text_galley = painter.layout_no_wrap(
                    concealed.display.to_owned(),
                    font_id.clone(),
                    text_color,
                );
                let icon_width = icon_galley.size().x;
                let height = row_height
                    .max(icon_galley.size().y)
                    .max(text_galley.size().y);
                let width = ui.available_width().max(icon_width + text_galley.size().x);
                let (rect, _) =
                    ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());

                painter.galley(rect.min, icon_galley, text_color);
                painter.galley(
                    egui::pos2(rect.left() + icon_width, rect.top()),
                    text_galley,
                    text_color,
                );

                if snapshot.cursor.line == line_index {
                    draw_cursor(
                        ui,
                        rect,
                        concealed.display,
                        &line.text,
                        snapshot,
                        &font_id,
                        icon_width,
                    );
                }
            }
        });
}

fn draw_cursor(
    ui: &egui::Ui,
    row_rect: egui::Rect,
    display: &str,
    raw: &str,
    snapshot: &EditorSnapshot,
    font_id: &egui::FontId,
    text_offset: f32,
) {
    let display_cursor = conceal::display_cursor(raw, snapshot.cursor);
    let byte_index = valid_byte_index(display, display_cursor.col);
    let before = &display[..byte_index];
    let cursor_text = display[byte_index..]
        .chars()
        .next()
        .map(|character| character.to_string())
        .unwrap_or_else(|| " ".to_owned());

    let painter = ui.painter();
    let before_width = painter
        .layout_no_wrap(
            before.to_owned(),
            font_id.clone(),
            ui.visuals().text_color(),
        )
        .size()
        .x;
    let cursor_width = painter
        .layout_no_wrap(
            cursor_text.clone(),
            font_id.clone(),
            ui.visuals().text_color(),
        )
        .size()
        .x
        .max(1.0);
    let cursor_x = row_rect.left() + text_offset + before_width;

    match snapshot.mode {
        Mode::Insert | Mode::Cmdline => {
            painter.line_segment(
                [
                    egui::pos2(cursor_x, row_rect.top()),
                    egui::pos2(cursor_x, row_rect.bottom()),
                ],
                egui::Stroke::new(2.0, ui.visuals().strong_text_color()),
            );
        }
        Mode::Replace => {
            painter.line_segment(
                [
                    egui::pos2(cursor_x, row_rect.bottom() - 1.0),
                    egui::pos2(cursor_x + cursor_width, row_rect.bottom() - 1.0),
                ],
                egui::Stroke::new(2.0, ui.visuals().strong_text_color()),
            );
        }
        _ => {
            let cursor_rect = egui::Rect::from_min_size(
                egui::pos2(cursor_x, row_rect.top()),
                egui::vec2(cursor_width, row_rect.height()),
            );
            painter.rect_filled(cursor_rect, 0.0, ui.visuals().selection.bg_fill);
            painter.text(
                cursor_rect.min,
                egui::Align2::LEFT_TOP,
                cursor_text,
                font_id.clone(),
                ui.visuals().selection.stroke.color,
            );
        }
    }
}

fn valid_byte_index(text: &str, requested: usize) -> usize {
    let mut index = requested.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_byte_index_is_clamped_to_utf8_boundary() {
        assert_eq!(valid_byte_index("新a", 1), 0);
        assert_eq!(valid_byte_index("新a", 3), 3);
        assert_eq!(valid_byte_index("新a", usize::MAX), 4);
    }
}
