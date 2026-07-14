//! Fyler Screens の titlebar / toolbar / breadcrumb。

use std::path::{Path, PathBuf};

use eframe::egui;

use crate::confirm::IconStyle;
use crate::{icon, theme};

pub const NAV_RAIL_WIDTH: f32 = 208.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChromeAction {
    NavigateParent,
}

/// タイトルバー・ツールバー・ぱんくずを1行へ統合したトップchrome。
///
/// フレームレスウィンドウのため、空き領域をドラッグ/ダブルクリックで
/// ウィンドウ移動・最大化に使う。ウィンドウ操作ボタンもこの行に置く。
pub fn draw_toolbar(ui: &mut egui::Ui, root: &Path) -> Option<ChromeAction> {
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
        ui.add_enabled(false, chrome_button("←"))
            .on_disabled_hover_text("Back history is not available");
        ui.add_enabled(false, chrome_button("→"))
            .on_disabled_hover_text("Forward history is not available");
        if ui
            .add(chrome_button("↑"))
            .on_hover_text("Parent directory")
            .clicked()
        {
            action = Some(ChromeAction::NavigateParent);
        }
        ui.add_enabled(false, chrome_button("↻"))
            .on_disabled_hover_text("Filesystem changes refresh automatically");
        ui.add_space(6.0);
        ui.separator();
        ui.add_space(6.0);
        draw_breadcrumb(ui, root);

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

/// ぱんくずをツールバー行へインラインで描く(currentの囲みなし)。
fn draw_breadcrumb(ui: &mut egui::Ui, root: &Path) {
    let parts = breadcrumb_parts(root);
    ui.spacing_mut().item_spacing.x = 1.0;
    for (index, part) in parts.iter().enumerate() {
        let current = index + 1 == parts.len();
        ui.label(
            egui::RichText::new(part)
                .monospace()
                .size(12.0)
                .color(if current {
                    theme::TEXT
                } else {
                    theme::TEXT_MUTED
                }),
        );
        if !current {
            ui.label(egui::RichText::new("›").color(theme::TEXT_FAINT));
        }
    }
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
    let mut entries = Vec::with_capacity(1 + bookmarks.len() + recent_roots.len() + drives.len());
    entries.push(NavigationEntry {
        section: NavigationSection::Pinned,
        label: navigation_path_label(root),
        path: root.to_path_buf(),
        current: true,
    });
    entries.extend(
        bookmarks
            .iter()
            .filter(|(_, path)| path.as_path() != root)
            .map(|(label, path)| NavigationEntry {
                section: NavigationSection::Pinned,
                label: label.clone(),
                path: path.clone(),
                current: false,
            }),
    );
    entries.extend(
        recent_roots
            .iter()
            .filter(|path| path.as_path() != root)
            .filter(|path| !bookmarks.iter().any(|(_, bookmark)| bookmark == *path))
            .map(|path| NavigationEntry {
                section: NavigationSection::Recent,
                label: navigation_path_label(path),
                path: path.clone(),
                current: false,
            }),
    );
    entries.extend(drives.iter().map(|path| NavigationEntry {
        section: NavigationSection::Drives,
        label: path.display().to_string(),
        path: path.clone(),
        current: false,
    }));
    entries
}

pub(crate) fn draw_navigation_rail(
    ui: &mut egui::Ui,
    entries: &[NavigationEntry],
    focused: bool,
    selected: usize,
    icon_style: IconStyle,
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
                    icon_style,
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
    icon_style: IconStyle,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), theme::TREE_ROW_HEIGHT),
        egui::Sense::click(),
    );
    if current || focused_selection {
        ui.painter()
            .rect_filled(rect, 0.0, theme::accent_selection_fill());
        ui.painter().rect_filled(
            egui::Rect::from_min_size(rect.min, egui::vec2(2.0, rect.height())),
            0.0,
            if current { theme::ACCENT } else { theme::BLUE },
        );
    } else if response.hovered() {
        ui.painter().rect_filled(rect, 0.0, theme::HOVER);
    }
    if focused_selection {
        ui.painter().rect_stroke(
            rect.shrink(1.0),
            0.0,
            egui::Stroke::new(1.0, theme::BLUE),
            egui::StrokeKind::Inside,
        );
    }
    ui.painter().text(
        egui::pos2(rect.left() + 14.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        format!("{}  {label}", icon::directory(icon_style)),
        egui::FontId::monospace(12.0),
        if current || focused_selection {
            theme::TEXT
        } else {
            theme::TEXT_SECONDARY
        },
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

fn breadcrumb_parts(root: &Path) -> Vec<String> {
    let mut parts = root
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .filter(|part| !part.is_empty() && part != "\\")
        .collect::<Vec<_>>();
    if parts.is_empty() {
        parts.push(root.display().to_string());
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breadcrumb_preserves_path_order() {
        let parts = breadcrumb_parts(Path::new("/home/user/project"));
        assert_eq!(parts, ["/", "home", "user", "project"]);
    }

    #[test]
    fn relative_root_is_still_visible() {
        assert_eq!(
            breadcrumb_parts(Path::new("project/src")),
            ["project", "src"]
        );
    }

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
            ],
            &[PathBuf::from("/recent")],
            &[PathBuf::from("/")],
        );

        assert_eq!(entries.len(), 4);
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.section == NavigationSection::Pinned)
                .count(),
            2
        );
        assert_eq!(entries[0].label, "work");
        assert_eq!(entries[1].label, "docs");
        assert_eq!(entries[2].section.title(), "RECENT");
        assert_eq!(entries[3].section.title(), "DRIVES");
    }
}
