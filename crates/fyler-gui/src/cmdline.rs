//! cmdline(`:` / `/`)とメッセージの描画。
//!
//! cmdlineはユーザーに開放する(`:%s/old/new/` は中核機能。脅威モデル参照)。
//! 内容はエンジンから `EditorEvent::CmdlineShow` / `CmdlineHide` で届く
//! (NvimEngineではext_cmdline由来だが、GUIはそれを知らなくてよい)。
//! メッセージ(`E486: Pattern not found` 等)は `EditorEvent::Message` で届く。

use eframe::egui;
use std::ops::Range;

use fyler_core::editor::{CmdlineState, EditorMessage, MessageKind, PopupmenuState};

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

/// cmdline補完候補の表示。最大8件を、選択行が見える窓へ切って描画する。
pub fn draw_popupmenu(ui: &mut egui::Ui, state: &PopupmenuState) {
    let range = visible_window(state.items.len(), state.selected, 8);
    if range.is_empty() {
        return;
    }

    for index in range {
        let item = &state.items[index];
        let mut label = item.word.clone();
        if !item.kind.is_empty() {
            label.push_str("  ");
            label.push_str(&item.kind);
        }
        if !item.menu.is_empty() {
            label.push_str("  ");
            label.push_str(&item.menu);
        }
        let _ = ui.selectable_label(
            state.selected == Some(index),
            egui::RichText::new(label).monospace(),
        );
    }
}

pub fn visible_window(len: usize, selected: Option<usize>, max: usize) -> Range<usize> {
    if len == 0 || max == 0 {
        return 0..0;
    }
    if len <= max {
        return 0..len;
    }

    let selected = selected.unwrap_or(0).min(len - 1);
    let mut start = selected.saturating_sub(max / 2);
    if start + max > len {
        start = len - max;
    }
    start..start + max
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_window_returns_full_range_when_shorter_than_max() {
        assert_eq!(visible_window(3, Some(2), 8), 0..3);
    }

    #[test]
    fn visible_window_starts_at_zero_near_head_or_without_selection() {
        assert_eq!(visible_window(20, None, 8), 0..8);
        assert_eq!(visible_window(20, Some(1), 8), 0..8);
    }

    #[test]
    fn visible_window_centers_middle_selection() {
        assert_eq!(visible_window(20, Some(10), 8), 6..14);
    }

    #[test]
    fn visible_window_clamps_to_tail() {
        assert_eq!(visible_window(20, Some(19), 8), 12..20);
        assert_eq!(visible_window(20, Some(99), 8), 12..20);
    }

    #[test]
    fn visible_window_handles_empty_or_zero_max() {
        assert_eq!(visible_window(0, Some(0), 8), 0..0);
        assert_eq!(visible_window(8, Some(0), 0), 0..0);
    }
}
