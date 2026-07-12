//! モードライン(NORMAL / INSERT / VISUAL ...)の描画。

use std::collections::HashMap;
use std::path::Path;

use eframe::egui;
use fyler_core::editor::{EditorSnapshot, Mode};
use fyler_core::fileinfo::{FileInfo, human_readable_size};
use fyler_core::grammar::PrefixParse;
use fyler_core::id::EntryId;

use crate::conceal;

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
    let dirty = if snapshot.dirty { " [+]" } else { "" };
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

    ui.horizontal(|ui| {
        ui.monospace(format!("{mode}{dirty}"));
        ui.monospace(root.display().to_string());
        if let Some((is_error, label)) = health_label(offline, unreadable, crashed) {
            let color = if is_error {
                ui.visuals().error_fg_color
            } else {
                egui::Color32::from_rgb(230, 190, 60)
            };
            ui.colored_label(color, label);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.monospace(format!("{line}:{column}"));
            if let Some(file_info) = &file_info {
                ui.monospace(file_info);
            }
        });
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
