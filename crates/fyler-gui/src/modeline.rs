//! モードライン(NORMAL / INSERT / VISUAL ...)の描画。

use std::collections::HashMap;
use std::path::Path;

use eframe::egui;
use fyler_core::editor::{EditorSnapshot, Mode};
use fyler_core::fileinfo::{FileInfo, human_readable_size};
use fyler_core::grammar::PrefixParse;
use fyler_core::id::EntryId;

use crate::{conceal, theme};

/// モード名・dirtyインジケータ・現在ルート・カーソル位置などを描く。
/// `Mode::Other(s)` は生文字列をそのまま表示する(隠さない)。
pub fn draw(
    ui: &mut egui::Ui,
    snapshot: &EditorSnapshot,
    root: &Path,
    file_infos: &HashMap<EntryId, FileInfo>,
    offline: bool,
    unreadable: usize,
    crashed: bool,
) {
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
    let file_info = cursor_file_info(snapshot, file_infos);

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

    ui.painter().rect_filled(ui.max_rect(), 0.0, theme::SURFACE);
    ui.painter().line_segment(
        [ui.max_rect().left_top(), ui.max_rect().right_top()],
        egui::Stroke::new(1.0, theme::BORDER_SUBTLE),
    );
    ui.horizontal_centered(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        draw_mode_badge(ui, mode);
        ui.label(
            egui::RichText::new(root.display().to_string())
                .monospace()
                .size(12.0)
                .color(theme::TEXT_MUTED),
        );
        if snapshot.dirty {
            ui.label(
                egui::RichText::new("·  pending changes")
                    .monospace()
                    .size(12.0)
                    .color(theme::ACCENT),
            );
        }
        if let Some((is_error, label)) = health_label(offline, unreadable, crashed) {
            ui.label(
                egui::RichText::new(label)
                    .monospace()
                    .size(12.0)
                    .color(if is_error { theme::RED } else { theme::YELLOW }),
            );
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(12.0);
            ui.label(
                egui::RichText::new(format!("ln {line}, col {column}"))
                    .monospace()
                    .size(11.0)
                    .color(theme::TEXT_FAINT),
            );
            if let Some(file_info) = &file_info {
                ui.label(
                    egui::RichText::new(file_info)
                        .monospace()
                        .size(11.0)
                        .color(theme::TEXT_FAINT),
                );
            }
        });
    });
}

fn draw_mode_badge(ui: &mut egui::Ui, mode: &str) {
    let (background, foreground) = match mode {
        "INSERT" => (theme::ACCENT, theme::CANVAS),
        "REPLACE" => (theme::YELLOW, theme::CANVAS),
        "VISUAL" | "VISUAL LINE" | "VISUAL BLOCK" => (theme::BLUE, theme::CANVAS),
        _ => (theme::SURFACE_RAISED, theme::TEXT),
    };
    let text = egui::RichText::new(mode)
        .monospace()
        .size(11.0)
        .strong()
        .color(foreground);
    egui::Frame::NONE
        .fill(background)
        .inner_margin(egui::Margin::symmetric(12, 5))
        .show(ui, |ui| {
            ui.label(text);
        });
}

fn health_label(offline: bool, unreadable: usize, crashed: bool) -> Option<(bool, String)> {
    if crashed {
        None
    } else if offline {
        Some((true, "[offline]".to_owned()))
    } else if unreadable > 0 {
        Some((false, format!("[! {unreadable} unreadable]")))
    } else {
        None
    }
}

fn cursor_file_info(
    snapshot: &EditorSnapshot,
    file_infos: &HashMap<EntryId, FileInfo>,
) -> Option<String> {
    let line = snapshot.lines.get(snapshot.cursor.line)?;
    let PrefixParse::WithId { id, .. } = fyler_core::grammar::split_id_prefix(&line.text) else {
        return None;
    };
    let info = file_infos.get(&id)?;
    let mut parts = Vec::with_capacity(3);
    if let Some(size) = info.size {
        parts.push(human_readable_size(size));
    }
    if let Some(modified) = &info.modified {
        parts.push(modified.clone());
    }
    if info.is_placeholder {
        parts.push("[cloud]".to_owned());
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

#[cfg(test)]
mod tests {
    use super::health_label;

    #[test]
    fn pane_health_prefers_crash_then_offline_then_unreadable() {
        assert_eq!(health_label(true, 3, true), None);
        assert_eq!(
            health_label(true, 3, false),
            Some((true, "[offline]".to_owned()))
        );
        assert_eq!(
            health_label(false, 3, false),
            Some((false, "[! 3 unreadable]".to_owned()))
        );
        assert_eq!(health_label(false, 0, false), None);
    }
}
