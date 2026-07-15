//! ステータスライン(NORMAL / INSERT / VISUAL ...)の描画。

use std::collections::HashMap;
use std::path::Path;

use eframe::egui;
use fyler_core::editor::{EditorSnapshot, Mode};
use fyler_core::fileinfo::{FileInfo, human_readable_size};
use fyler_core::grammar::PrefixParse;
use fyler_core::id::EntryId;
use fyler_core::options::StatusItem;

use crate::{conceal, theme};

/// ステータスラインを設定順の項目で描く。左右クラスタはユーザーがカスタムできる。
/// `Mode::Other(s)` は生文字列をそのまま表示する。
#[allow(clippy::too_many_arguments)]
pub fn draw(
    ui: &mut egui::Ui,
    snapshot: &EditorSnapshot,
    root: &Path,
    branch: Option<&str>,
    file_infos: &HashMap<EntryId, FileInfo>,
    left: &[StatusItem],
    right: &[StatusItem],
    offline: bool,
    unreadable: usize,
    crashed: bool,
) {
    let context = StatusContext::new(snapshot, root, branch, file_infos);

    ui.painter().rect_filled(ui.max_rect(), 0.0, theme::SURFACE);
    ui.painter().line_segment(
        [ui.max_rect().left_top(), ui.max_rect().right_top()],
        egui::Stroke::new(1.0, theme::BORDER_SUBTLE),
    );
    ui.horizontal_centered(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        for item in left {
            render_item(ui, *item, &context);
        }
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
            for item in right.iter().rev() {
                render_item(ui, *item, &context);
            }
        });
    });
}

/// ステータスライン各項目が参照する、1フレーム分の表示値。
struct StatusContext {
    mode: String,
    branch: Option<String>,
    path: String,
    line: usize,
    column: usize,
    percent: usize,
    size: Option<String>,
    modified: Option<String>,
}

impl StatusContext {
    fn new(
        snapshot: &EditorSnapshot,
        root: &Path,
        branch: Option<&str>,
        file_infos: &HashMap<EntryId, FileInfo>,
    ) -> Self {
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
        }
        .to_owned();
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
        let total = snapshot.lines.len();
        let percent = if total <= 1 {
            100
        } else {
            snapshot.cursor.line.min(total - 1) * 100 / (total - 1)
        };
        let (size, modified) = cursor_size_and_modified(snapshot, file_infos);
        Self {
            mode,
            branch: branch.map(ToOwned::to_owned),
            path: root.display().to_string(),
            line,
            column,
            percent,
            size,
            modified,
        }
    }
}

fn render_item(ui: &mut egui::Ui, item: StatusItem, context: &StatusContext) {
    match item {
        StatusItem::Mode => draw_mode_badge(ui, &context.mode),
        StatusItem::Branch => {
            if let Some(branch) = &context.branch {
                ui.label(
                    egui::RichText::new(branch)
                        .monospace()
                        .size(12.0)
                        .color(theme::GREEN),
                );
            }
        }
        StatusItem::Path => {
            ui.label(
                egui::RichText::new(&context.path)
                    .monospace()
                    .size(12.0)
                    .color(theme::TEXT_MUTED),
            );
        }
        StatusItem::Line => faint(ui, format!("ln {}", context.line)),
        StatusItem::Column => faint(ui, format!("col {}", context.column)),
        StatusItem::Percent => faint(ui, format!("{}%", context.percent)),
        StatusItem::Size => {
            if let Some(size) = &context.size {
                faint(ui, size.clone());
            }
        }
        StatusItem::Modified => {
            if let Some(modified) = &context.modified {
                faint(ui, modified.clone());
            }
        }
    }
}

fn faint(ui: &mut egui::Ui, text: String) {
    ui.label(
        egui::RichText::new(text)
            .monospace()
            .size(11.0)
            .color(theme::TEXT_FAINT),
    );
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

fn cursor_size_and_modified(
    snapshot: &EditorSnapshot,
    file_infos: &HashMap<EntryId, FileInfo>,
) -> (Option<String>, Option<String>) {
    let Some(line) = snapshot.lines.get(snapshot.cursor.line) else {
        return (None, None);
    };
    let PrefixParse::WithId { id, .. } = fyler_core::grammar::split_id_prefix(&line.text) else {
        return (None, None);
    };
    let Some(info) = file_infos.get(&id) else {
        return (None, None);
    };
    let mut size = info.size.map(human_readable_size);
    if info.is_placeholder {
        size = Some(match size {
            Some(size) => format!("{size} [cloud]"),
            None => "[cloud]".to_owned(),
        });
    }
    (size, info.modified.clone())
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
