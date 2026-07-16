//! Fyler Screens の titlebar / toolbar / breadcrumb。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use eframe::egui;

use crate::{icon, theme};

pub const NAV_RAIL_WIDTH: f32 = 208.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChromeAction {
    NavigateParent,
    HistoryBack,
    HistoryForward,
    Refresh,
}

/// タイトルバー・ツールバー・ぱんくずを1行へ統合したトップchrome。
///
/// フレームレスウィンドウのため、空き領域をドラッグ/ダブルクリックで
/// ウィンドウ移動・最大化に使う。ウィンドウ操作ボタンもこの行に置く。
///
/// `can_go_back` / `can_go_forward` はアクティブpaneのnavigation historyの
/// 可用性(GUIが把握できる範囲)。↻(reload)は権威判定をapp層に委ねるため
/// 常に有効にする。
pub fn draw_toolbar(
    ui: &mut egui::Ui,
    can_go_back: bool,
    can_go_forward: bool,
) -> Option<ChromeAction> {
    ui.painter().rect_filled(ui.max_rect(), 0.0, theme::SURFACE);
    ui.painter().line_segment(
        [ui.max_rect().left_bottom(), ui.max_rect().right_bottom()],
        egui::Stroke::new(1.0, theme::BORDER_SUBTLE),
    );

    // 空き領域全体をドラッグ判定に敷く。個々のウィジェットは後段で上に描かれ、
    // クリックはウィジェットが優先的に受け取る(eguiの重なり順)。
    let drag = ui.interact(
        ui.max_rect(),
        ui.id().with("chrome-drag"),
        egui::Sense::click_and_drag(),
    );
    if drag.double_clicked() {
        let maximized = ui
            .ctx()
            .input(|input| input.viewport().maximized.unwrap_or(false));
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
    } else if drag.drag_started() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }

    let mut action = None;
    ui.horizontal_centered(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        ui.add_space(8.0);
        if ui
            .add_enabled(can_go_back, chrome_button("←"))
            .on_hover_text("Go back in history (:back)")
            .on_disabled_hover_text("No earlier location in history")
            .clicked()
        {
            action = Some(ChromeAction::HistoryBack);
        }
        if ui
            .add_enabled(can_go_forward, chrome_button("→"))
            .on_hover_text("Go forward in history (:forward)")
            .on_disabled_hover_text("No later location in history")
            .clicked()
        {
            action = Some(ChromeAction::HistoryForward);
        }
        if ui
            .add(chrome_button("↑"))
            .on_hover_text("Parent directory")
            .clicked()
        {
            action = Some(ChromeAction::NavigateParent);
        }
        if ui
            .add(chrome_button("↻"))
            .on_hover_text("Reload from disk (:reload)")
            .clicked()
        {
            action = Some(ChromeAction::Refresh);
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            if window_button(ui, "×", true).clicked() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
            if window_button(ui, "□", false).clicked() {
                let maximized = ui
                    .ctx()
                    .input(|input| input.viewport().maximized.unwrap_or(false));
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
            }
            if window_button(ui, "—", false).clicked() {
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            }
        });
    });
    action
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NavigationSection {
    Pinned,
    Recent,
    Drives,
}

impl NavigationSection {
    fn title(self) -> &'static str {
        match self {
            Self::Pinned => "PINNED",
            Self::Recent => "RECENT",
            Self::Drives => "DRIVES",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NavigationEntry {
    section: NavigationSection,
    label: String,
    pub(crate) path: PathBuf,
    current: bool,
}

pub(crate) fn navigation_entries(
    root: &Path,
    bookmarks: &[(String, PathBuf)],
    recent_roots: &[PathBuf],
    drives: &[PathBuf],
) -> Vec<NavigationEntry> {
    let mut entries = Vec::with_capacity(bookmarks.len() + recent_roots.len() + drives.len());
    let mut seen = HashSet::new();
    for (label, path) in bookmarks {
        if seen.insert(path.clone()) {
            entries.push(NavigationEntry {
                section: NavigationSection::Pinned,
                label: label.clone(),
                path: path.clone(),
                current: path.as_path() == root,
            });
        }
    }
    for path in recent_roots {
        if path.as_path() != root && seen.insert(path.clone()) {
            entries.push(NavigationEntry {
                section: NavigationSection::Recent,
                label: navigation_path_label(path),
                path: path.clone(),
                current: false,
            });
        }
    }
    for path in drives {
        if seen.insert(path.clone()) {
            entries.push(NavigationEntry {
                section: NavigationSection::Drives,
                label: path.display().to_string(),
                path: path.clone(),
                current: path.as_path() == root,
            });
        }
    }
    entries
}

pub(crate) fn draw_navigation_rail(
    ui: &mut egui::Ui,
    entries: &[NavigationEntry],
    focused: bool,
    selected: usize,
) -> Option<usize> {
    ui.set_clip_rect(ui.max_rect());
    ui.painter().rect_filled(ui.max_rect(), 0.0, theme::SURFACE);
    ui.painter().line_segment(
        [ui.max_rect().right_top(), ui.max_rect().right_bottom()],
        egui::Stroke::new(1.0, theme::BORDER_SUBTLE),
    );

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let mut clicked = None;
            let mut previous_section = None;
            ui.add_space(12.0);
            for (index, entry) in entries.iter().enumerate() {
                if previous_section != Some(entry.section) {
                    if previous_section.is_some() {
                        ui.add_space(10.0);
                    }
                    let count = entries[index..]
                        .iter()
                        .take_while(|candidate| candidate.section == entry.section)
                        .count();
                    navigation_section(ui, entry.section.title(), count);
                    previous_section = Some(entry.section);
                }
                let response = navigation_row(
                    ui,
                    &entry.label,
                    entry.current,
                    focused && selected == index,
                );
                if focused && selected == index {
                    response.scroll_to_me(None);
                }
                if response.clicked() {
                    clicked = Some(index);
                }
            }
            clicked
        })
        .inner
}

fn navigation_section(ui: &mut egui::Ui, title: &str, count: usize) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 18.0), egui::Sense::hover());
    let painter = ui.painter();
    let font = egui::FontId::monospace(10.0);
    painter.text(
        egui::pos2(rect.left() + 14.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        title,
        font.clone(),
        theme::TEXT_FAINT,
    );
    painter.text(
        egui::pos2(rect.right() - 14.0, rect.center().y),
        egui::Align2::RIGHT_CENTER,
        count,
        font,
        theme::TEXT_FAINT,
    );
}

fn navigation_row(
    ui: &mut egui::Ui,
    label: &str,
    current: bool,
    focused_selection: bool,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), theme::TREE_ROW_HEIGHT),
        egui::Sense::click(),
    );
    if focused_selection {
        ui.painter()
            .rect_filled(rect, 0.0, theme::accent_selection_fill());
    } else if response.hovered() {
        ui.painter().rect_filled(rect, 0.0, theme::HOVER);
    }
    // 左端バー: カーソル(選択)はBLUEで追従、現在rootは控えめなACCENTマーカー。
    if focused_selection || current {
        ui.painter().rect_filled(
            egui::Rect::from_min_size(rect.min, egui::vec2(2.0, rect.height())),
            0.0,
            if focused_selection {
                theme::BLUE
            } else {
                theme::ACCENT
            },
        );
    }
    if focused_selection {
        ui.painter().rect_stroke(
            rect.shrink(1.0),
            0.0,
            egui::Stroke::new(1.0, theme::BLUE),
            egui::StrokeKind::Inside,
        );
    }
    let color = if focused_selection || current {
        theme::TEXT
    } else {
        theme::TEXT_SECONDARY
    };
    let painter = ui.painter();
    let label_font = egui::FontId::monospace(12.0);
    let icon_galley = painter.layout_no_wrap(
        icon::directory().to_owned(),
        egui::FontId::new(12.0, icon::font_family()),
        color,
    );
    let icon_x = rect.left() + 14.0;
    painter.galley(
        egui::pos2(icon_x, rect.center().y - icon_galley.size().y / 2.0),
        icon_galley.clone(),
        color,
    );
    let gap = painter
        .layout_no_wrap("  ".to_owned(), label_font.clone(), color)
        .size()
        .x;
    painter.text(
        egui::pos2(icon_x + icon_galley.size().x + gap, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        label_font,
        color,
    );
    response
}

fn navigation_path_label(path: &Path) -> String {
    path.file_name()
        .filter(|name| !name.is_empty())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn window_button(ui: &mut egui::Ui, label: &str, close: bool) -> egui::Response {
    let response = ui.add_sized(
        [40.0, theme::TOOLBAR_HEIGHT],
        egui::Button::new(
            egui::RichText::new(label)
                .monospace()
                .size(13.0)
                .color(theme::TEXT_MUTED),
        )
        .frame(false),
    );
    if close && response.hovered() {
        ui.painter().rect_filled(response.rect, 0.0, theme::RED);
        ui.painter().text(
            response.rect.center(),
            egui::Align2::CENTER_CENTER,
            label,
            egui::FontId::monospace(13.0),
            theme::CANVAS,
        );
    }
    response
}

fn chrome_button(label: &'static str) -> egui::Button<'static> {
    egui::Button::new(
        egui::RichText::new(label)
            .monospace()
            .size(14.0)
            .color(theme::TEXT_SECONDARY),
    )
    .frame(false)
    .min_size(egui::vec2(26.0, 26.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_rail_uses_leaf_name_and_keeps_root_visible() {
        assert_eq!(
            navigation_path_label(Path::new("/home/user/project")),
            "project"
        );
        assert_eq!(navigation_path_label(Path::new("/")), "/");
    }
    #[test]
    fn navigation_entries_are_sectioned_countable_and_deduplicated() {
        let entries = navigation_entries(
            Path::new("/work"),
            &[
                ("current".to_owned(), PathBuf::from("/work")),
                ("docs".to_owned(), PathBuf::from("/docs")),
                ("docs-duplicate".to_owned(), PathBuf::from("/docs")),
            ],
            &[PathBuf::from("/recent"), PathBuf::from("/docs")],
            &[PathBuf::from("/"), PathBuf::from("/recent")],
        );

        assert_eq!(entries.len(), 4);
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.section == NavigationSection::Pinned)
                .count(),
            2
        );
        assert_eq!(entries[0].label, "current");
        assert!(entries[0].current);
        assert_eq!(entries[1].label, "docs");
        assert!(!entries[1].current);
        assert_eq!(entries[2].section.title(), "RECENT");
        assert_eq!(entries[3].section.title(), "DRIVES");
    }

    #[test]
    fn navigation_entries_skip_drive_when_bookmark_already_uses_path() {
        let entries = navigation_entries(
            Path::new("/other"),
            &[("root".to_owned(), PathBuf::from("/"))],
            &[],
            &[PathBuf::from("/")],
        );

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].section, NavigationSection::Pinned);
        assert_eq!(entries[0].path, PathBuf::from("/"));
    }
}
