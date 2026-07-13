//! Fyler Screens の titlebar / toolbar / breadcrumb。

use std::path::{Path, PathBuf};

use eframe::egui;

use crate::theme;

pub const NAV_RAIL_WIDTH: f32 = 208.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChromeAction {
    NavigateParent,
    ToggleHidden,
    ReviewChanges,
    ShowSettings,
}

pub fn draw_titlebar(ui: &mut egui::Ui, root: &Path) {
    ui.painter().rect_filled(ui.max_rect(), 0.0, theme::SURFACE);
    ui.painter().line_segment(
        [ui.max_rect().left_bottom(), ui.max_rect().right_bottom()],
        egui::Stroke::new(1.0, theme::BORDER_SUBTLE),
    );

    ui.horizontal_centered(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        ui.add_space(12.0);
        draw_logo(ui);
        ui.add_space(9.0);
        ui.label(
            egui::RichText::new("fyler")
                .monospace()
                .strong()
                .color(theme::TEXT),
        );
        ui.add_space(8.0);
        ui.label(egui::RichText::new("·").color(theme::TEXT_FAINT));
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(root.display().to_string())
                .monospace()
                .size(12.0)
                .color(theme::TEXT_MUTED),
        );

        let controls_width = 46.0 * 3.0;
        let drag_width = (ui.available_width() - controls_width).max(0.0);
        let (drag_rect, drag_response) = ui.allocate_exact_size(
            egui::vec2(drag_width, theme::TITLEBAR_HEIGHT),
            egui::Sense::click_and_drag(),
        );
        if drag_response.double_clicked() {
            let maximized = ui
                .ctx()
                .input(|input| input.viewport().maximized.unwrap_or(false));
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
        } else if drag_response.drag_started() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        let _ = drag_rect;

        if window_button(ui, "—", false).clicked() {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        }
        if window_button(ui, "□", false).clicked() {
            let maximized = ui
                .ctx()
                .input(|input| input.viewport().maximized.unwrap_or(false));
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
        }
        if window_button(ui, "×", true).clicked() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
    });
}

pub fn draw_toolbar(ui: &mut egui::Ui, show_hidden: bool, dirty: bool) -> Option<ChromeAction> {
    ui.painter().rect_filled(ui.max_rect(), 0.0, theme::CANVAS);
    ui.painter().line_segment(
        [ui.max_rect().left_bottom(), ui.max_rect().right_bottom()],
        egui::Stroke::new(1.0, theme::BORDER_SUBTLE),
    );

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

        let hidden_label = if show_hidden {
            "hidden  on"
        } else {
            "hidden  off"
        };
        if ui
            .add(
                egui::Button::new(
                    egui::RichText::new(hidden_label)
                        .monospace()
                        .size(12.0)
                        .color(if show_hidden {
                            theme::TEXT
                        } else {
                            theme::TEXT_MUTED
                        }),
                )
                .frame(false),
            )
            .on_hover_text("Toggle hidden files")
            .clicked()
        {
            action = Some(ChromeAction::ToggleHidden);
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add_space(8.0);
            if ui
                .add(
                    egui::Button::new(
                        egui::RichText::new("settings")
                            .monospace()
                            .size(12.0)
                            .color(theme::TEXT_MUTED),
                    )
                    .frame(false),
                )
                .on_hover_text("Appearance and safety settings")
                .clicked()
            {
                action = Some(ChromeAction::ShowSettings);
            }
            let review = if dirty {
                egui::RichText::new("review changes  ·  pending")
                    .monospace()
                    .size(12.0)
                    .color(theme::ACCENT)
            } else {
                egui::RichText::new("review changes  ·  0")
                    .monospace()
                    .size(12.0)
                    .color(theme::TEXT_MUTED)
            };
            if ui
                .add_enabled(
                    dirty,
                    egui::Button::new(review).min_size(egui::vec2(150.0, 24.0)),
                )
                .on_hover_text("Review pending changes before applying")
                .clicked()
            {
                action = Some(ChromeAction::ReviewChanges);
            }
        });
    });
    action
}

pub fn draw_breadcrumb(ui: &mut egui::Ui, root: &Path) {
    ui.painter().rect_filled(ui.max_rect(), 0.0, theme::CANVAS);
    ui.painter().line_segment(
        [ui.max_rect().left_bottom(), ui.max_rect().right_bottom()],
        egui::Stroke::new(1.0, theme::BORDER_SUBTLE),
    );
    let parts = breadcrumb_parts(root);
    ui.horizontal_centered(|ui| {
        ui.spacing_mut().item_spacing.x = 1.0;
        ui.add_space(12.0);
        for (index, part) in parts.iter().enumerate() {
            let current = index + 1 == parts.len();
            let text = egui::RichText::new(part)
                .monospace()
                .size(12.0)
                .color(if current {
                    theme::TEXT
                } else {
                    theme::TEXT_MUTED
                });
            ui.add_enabled(false, egui::Button::new(text).frame(current));
            if !current {
                ui.label(egui::RichText::new("›").color(theme::TEXT_FAINT));
            }
        }
    });
}
pub fn draw_navigation_rail(
    ui: &mut egui::Ui,
    root: &Path,
    bookmarks: &[(String, PathBuf)],
    recent_roots: &[PathBuf],
    drives: &[PathBuf],
) -> Option<PathBuf> {
    ui.set_clip_rect(ui.max_rect());
    ui.painter().rect_filled(ui.max_rect(), 0.0, theme::SURFACE);
    ui.painter().line_segment(
        [ui.max_rect().right_top(), ui.max_rect().right_bottom()],
        egui::Stroke::new(1.0, theme::BORDER_SUBTLE),
    );

    let mut target = None;
    ui.add_space(12.0);
    navigation_section(ui, "PINNED");
    navigation_row(ui, navigation_path_label(root), true);
    for (name, path) in bookmarks {
        if path != root && navigation_row(ui, name, false).clicked() {
            target = Some(path.clone());
        }
    }

    let recent = recent_roots
        .iter()
        .filter(|path| path.as_path() != root)
        .filter(|path| !bookmarks.iter().any(|(_, bookmark)| bookmark == *path))
        .collect::<Vec<_>>();
    if !recent.is_empty() {
        ui.add_space(10.0);
        navigation_section(ui, "RECENT");
        for path in recent {
            if navigation_row(ui, navigation_path_label(path), false).clicked() {
                target = Some(path.clone());
            }
        }
    }

    if !drives.is_empty() {
        ui.add_space(10.0);
        navigation_section(ui, "DRIVES");
        for path in drives {
            if navigation_row(ui, path.display().to_string(), false).clicked() {
                target = Some(path.clone());
            }
        }
    }

    target
}

fn navigation_section(ui: &mut egui::Ui, title: &str) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 18.0), egui::Sense::hover());
    ui.painter().text(
        egui::pos2(rect.left() + 14.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        title,
        egui::FontId::monospace(10.0),
        theme::TEXT_FAINT,
    );
}

fn navigation_row(ui: &mut egui::Ui, label: impl Into<String>, selected: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), theme::TREE_ROW_HEIGHT),
        egui::Sense::click(),
    );
    if selected {
        ui.painter()
            .rect_filled(rect, 0.0, theme::accent_selection_fill());
        ui.painter().rect_filled(
            egui::Rect::from_min_size(rect.min, egui::vec2(2.0, rect.height())),
            0.0,
            theme::ACCENT,
        );
    } else if response.hovered() {
        ui.painter().rect_filled(rect, 0.0, theme::HOVER);
    }
    ui.painter().text(
        egui::pos2(rect.left() + 14.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        format!("D  {}", label.into()),
        egui::FontId::monospace(12.0),
        if selected {
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

fn draw_logo(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
    ui.painter().rect_stroke(
        rect.shrink(0.75),
        egui::CornerRadius::same(3),
        egui::Stroke::new(1.5, theme::ACCENT),
        egui::StrokeKind::Inside,
    );
    for (offset, width) in [(5.0, 8.0), (8.0, 5.0), (11.0, 8.0)] {
        ui.painter().line_segment(
            [
                egui::pos2(rect.left() + 4.0, rect.top() + offset),
                egui::pos2(rect.left() + 4.0 + width, rect.top() + offset),
            ],
            egui::Stroke::new(1.2, theme::TEXT),
        );
    }
}

fn window_button(ui: &mut egui::Ui, label: &str, close: bool) -> egui::Response {
    let response = ui.add_sized(
        [46.0, theme::TITLEBAR_HEIGHT],
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
}
