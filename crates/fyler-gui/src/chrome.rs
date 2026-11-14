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

/// Navigation controls share the top row; caption controls are app-painted but remain
/// owned by the native window for input.
///
/// `caption_cell_size` is one native caption cell in logical points; navigation controls
/// use two-thirds of its width. `native_caption_left` delimits the reserved caption strip.
/// The returned drag region excludes navigation and that strip.
pub fn draw_toolbar(
    ui: &mut egui::Ui,
    can_go_back: bool,
    can_go_forward: bool,
    caption_cell_size: egui::Vec2,
    native_caption_left: Option<f32>,
) -> (Option<ChromeAction>, Option<egui::Rect>) {
    let bounds = ui.max_rect();
    let content_right = native_caption_left
        .map(|left| left.clamp(bounds.left(), bounds.right()))
        .unwrap_or(bounds.right());
    let content = egui::Rect::from_min_max(bounds.min, egui::pos2(content_right, bounds.bottom()));
    ui.set_clip_rect(ui.clip_rect().intersect(content));
    ui.painter()
        .with_clip_rect(content)
        .rect_filled(content, 0.0, theme::SURFACE);
    ui.painter().with_clip_rect(content).line_segment(
        [content.left_bottom(), content.right_bottom()],
        egui::Stroke::new(1.0, theme::BORDER_SUBTLE),
    );

    let navigation_button_size = egui::vec2(caption_cell_size.x * (2.0 / 3.0), caption_cell_size.y);
    let mut drag_rect = None;
    let mut action = None;
    ui.allocate_ui_with_layout(
        content.size(),
        egui::Layout::top_down(egui::Align::Min),
        |ui| {
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 1.0;
                let controls_width = 8.0 + 4.0 * navigation_button_size.x + 3.0;
                if content.width() >= controls_width {
                    ui.add_space(8.0);
                    let back = toolbar_button(ui, "←", can_go_back, navigation_button_size)
                        .on_hover_text("Go back in history (:back)")
                        .on_disabled_hover_text("No earlier location in history");
                    if back.clicked() {
                        action = Some(ChromeAction::HistoryBack);
                    }
                    let forward = toolbar_button(ui, "→", can_go_forward, navigation_button_size)
                        .on_hover_text("Go forward in history (:forward)")
                        .on_disabled_hover_text("No later location in history");
                    if forward.clicked() {
                        action = Some(ChromeAction::HistoryForward);
                    }
                    let parent = toolbar_button(ui, "↑", true, navigation_button_size)
                        .on_hover_text("Parent directory");
                    if parent.clicked() {
                        action = Some(ChromeAction::NavigateParent);
                    }
                    let refresh = toolbar_button(ui, "↻", true, navigation_button_size)
                        .on_hover_text("Refresh from disk (:refresh)");
                    if refresh.clicked() {
                        action = Some(ChromeAction::Refresh);
                    }
                    let rect = egui::Rect::from_min_max(
                        egui::pos2(refresh.rect.right(), bounds.top()),
                        egui::pos2(content_right, bounds.bottom()),
                    );
                    if rect.is_positive() {
                        let drag = ui.interact(
                            rect,
                            ui.id().with("chrome-drag"),
                            egui::Sense::click_and_drag(),
                        );
                        if drag.drag_started() {
                            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
                        }
                        drag_rect = Some(rect);
                    }
                }
            });
        },
    );
    (action, drag_rect)
}

// Window sizing, caption buttons, and caption hit-testing are handled by the OS.

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

fn toolbar_button(
    ui: &mut egui::Ui,
    label: &'static str,
    enabled: bool,
    size: egui::Vec2,
) -> egui::Response {
    ui.spacing_mut().interact_size = size;
    let background = ui.painter().add(egui::Shape::Noop);
    let response = ui.add_enabled(
        enabled,
        egui::Button::new(
            egui::RichText::new(label)
                .monospace()
                .size(16.0)
                .color(theme::TEXT_SECONDARY),
        )
        .frame(false)
        .min_size(size),
    );
    paint_toolbar_hover(ui, background, &response);
    response
}

fn paint_toolbar_hover(
    ui: &egui::Ui,
    background: egui::layers::ShapeIdx,
    response: &egui::Response,
) {
    if response.hovered() && response.enabled() {
        ui.painter().set(
            background,
            egui::Shape::rect_filled(response.rect, 0.0, theme::TOOLBAR_HOVER),
        );
    }
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

    #[test]
    fn four_navigation_buttons_share_neutral_hover_fill() {
        use egui::{Event, Pos2, RawInput, Rect, Vec2};

        fn frame(ctx: &egui::Context, pointer: Option<Pos2>) -> egui::FullOutput {
            theme::install(ctx);
            let input = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 40.0))),
                events: pointer.into_iter().map(Event::PointerMoved).collect(),
                ..Default::default()
            };
            ctx.run_ui(input, |ctx| {
                egui::Panel::top("toolbar-test")
                    .exact_size(32.0)
                    .frame(egui::Frame::NONE)
                    .show(ctx, |ui| {
                        draw_toolbar(ui, true, true, egui::vec2(69.0, 27.75), None);
                    });
            })
        }

        fn text_center(output: &egui::FullOutput, text: &str) -> Pos2 {
            output
                .shapes
                .iter()
                .find_map(|clipped| match &clipped.shape {
                    egui::Shape::Text(shape) if shape.galley.job.text == text => {
                        Some(shape.pos + shape.galley.size() * 0.5)
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("missing toolbar glyph {text:?}"))
        }

        fn hover_fills(output: &egui::FullOutput) -> Vec<(usize, Rect)> {
            output
                .shapes
                .iter()
                .enumerate()
                .filter_map(|(index, clipped)| match &clipped.shape {
                    egui::Shape::Rect(shape) if shape.fill == theme::TOOLBAR_HOVER => {
                        Some((index, shape.rect))
                    }
                    _ => None,
                })
                .collect()
        }

        let ctx = egui::Context::default();
        let baseline = frame(&ctx, None); // warmup pass
        assert!(
            hover_fills(&baseline).is_empty(),
            "non-hovered controls stay transparent"
        );
        let centers: Vec<(&str, Pos2)> = ["←", "→", "↑", "↻"]
            .into_iter()
            .map(|label| (label, text_center(&baseline, label)))
            .collect();

        for (label, center) in centers {
            let output = frame(&ctx, Some(center));
            let fills = hover_fills(&output);
            assert_eq!(
                fills.len(),
                1,
                "{label} should paint exactly one hover fill"
            );
            let (fill_index, fill_rect) = fills[0];
            assert_eq!(
                fill_rect.size(),
                Vec2::new(46.0, 27.75),
                "{label} uses the full native-sized hit rectangle"
            );
            assert_eq!(fill_rect.center().y, 16.0, "{label} is vertically centered");
            assert!(
                fill_rect.contains(center),
                "{label} fill misses its control"
            );
            assert!(
                !output.shapes.iter().any(|clipped| matches!(
                    &clipped.shape,
                    egui::Shape::Rect(shape) if shape.fill == theme::RED
                )),
                "{label} must not use a red special-case fill"
            );
            if label == "←" {
                let text_index = output
                    .shapes
                    .iter()
                    .position(|clipped| {
                        matches!(
                            &clipped.shape,
                            egui::Shape::Text(shape) if shape.galley.job.text == label
                        )
                    })
                    .expect("hovered button text should be painted");
                assert!(
                    text_index > fill_index,
                    "button text must remain above its fill"
                );
            }
        }

        let disabled_ctx = egui::Context::default();
        let _ = frame(&disabled_ctx, None);
        let disabled_output = {
            let input = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 40.0))),
                ..Default::default()
            };
            disabled_ctx.run_ui(input, |ctx| {
                egui::Panel::top("toolbar-disabled-test")
                    .exact_size(32.0)
                    .frame(egui::Frame::NONE)
                    .show(ctx, |ui| {
                        draw_toolbar(ui, false, false, egui::vec2(69.0, 27.75), None);
                    });
            })
        };
        for label in ["←", "→"] {
            let center = text_center(&baseline, label);
            let output = {
                let input = RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 40.0))),
                    events: vec![Event::PointerMoved(center)],
                    ..Default::default()
                };
                disabled_ctx.run_ui(input, |ctx| {
                    egui::Panel::top("toolbar-disabled-test")
                        .exact_size(32.0)
                        .frame(egui::Frame::NONE)
                        .show(ctx, |ui| {
                            draw_toolbar(ui, false, false, egui::vec2(69.0, 27.75), None);
                        });
                })
            };
            assert!(
                hover_fills(&output).is_empty(),
                "disabled {label} must not highlight"
            );
        }
        assert!(hover_fills(&disabled_output).is_empty());
    }

    #[test]
    fn native_sized_button_hit_area_matches_caption_dimensions() {
        use egui::{Event, PointerButton, Pos2, RawInput, Rect, Vec2};

        let ctx = egui::Context::default();
        theme::install(&ctx);
        let caption_cell_size = Vec2::new(69.0, 27.75);
        let size = Vec2::new(caption_cell_size.x * (2.0 / 3.0), caption_cell_size.y);
        let mut action = None;
        let mut run = |events| {
            let input = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 40.0))),
                events,
                ..Default::default()
            };
            ctx.run_ui(input, |ctx| {
                egui::Panel::top("native-sized-button-test")
                    .exact_size(32.0)
                    .frame(egui::Frame::NONE)
                    .show(ctx, |ui| {
                        action = draw_toolbar(ui, true, true, caption_cell_size, None).0;
                    });
            })
        };

        let baseline = run(Vec::new());
        let hover = run(vec![Event::PointerMoved(Pos2::new(30.0, 10.0))]);
        let hit_rect = hover
            .shapes
            .iter()
            .find_map(|clipped| match &clipped.shape {
                egui::Shape::Rect(shape) if shape.fill == theme::TOOLBAR_HOVER => Some(shape.rect),
                _ => None,
            })
            .expect("hover paints the button hit rectangle");
        assert_eq!(hit_rect.size(), size);
        assert_eq!(hit_rect.center().y, 16.0);
        assert!(baseline.shapes.iter().any(|clipped| matches!(
            &clipped.shape,
            egui::Shape::Text(text) if text.galley.job.text == "←"
        )));

        let click = hit_rect.right_bottom() - Vec2::splat(1.0);
        let _ = run(vec![
            Event::PointerMoved(click),
            Event::PointerButton {
                pos: click,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            },
        ]);
        let _ = run(vec![Event::PointerButton {
            pos: click,
            button: PointerButton::Primary,
            pressed: false,
            modifiers: Default::default(),
        }]);
        assert_eq!(action, Some(ChromeAction::HistoryBack));
    }
    #[test]
    fn blank_drag_region_stops_before_native_caption_at_supported_widths() {
        use egui::{Pos2, RawInput, Rect, Vec2};
        for (width, caption_left) in [(720.0, 620.0), (1344.0, 1244.0)] {
            let ctx = egui::Context::default();
            let input = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(width, 40.0))),
                ..Default::default()
            };
            let mut drag_region = None;
            let _ = ctx.run_ui(input, |ctx| {
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE)
                    .show(ctx, |ui| {
                        let (_, region) = draw_toolbar(
                            ui,
                            true,
                            true,
                            egui::vec2(69.0, 32.0),
                            Some(caption_left),
                        );
                        drag_region = region;
                    });
            });
            let drag_region = drag_region.expect("toolbar leaves a blank drag area");
            assert_eq!(drag_region.right(), caption_left);
            assert!(drag_region.left() < drag_region.right());
        }
    }

    #[test]
    fn toolbar_clips_controls_and_drag_region_to_native_strip_at_narrow_width() {
        use egui::{Pos2, RawInput, Rect, Vec2};

        let ctx = egui::Context::default();
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(100.0, 40.0))),
            events: vec![egui::Event::PointerMoved(Pos2::new(90.0, 20.0))],
            ..Default::default()
        };
        let mut drag_region = None;
        let output = ctx.run_ui(input, |ctx| {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ctx, |ui| {
                    let (_, drag) =
                        draw_toolbar(ui, true, true, egui::vec2(69.0, 32.0), Some(40.0));
                    drag_region = drag;
                });
        });

        assert!(
            drag_region.is_none(),
            "narrow content has no drag hit region"
        );
        assert!(
            !output.shapes.iter().any(|shape| {
                matches!(&shape.shape, egui::Shape::Text(text)
                    if ["←", "→", "↑", "↻"].contains(&text.galley.job.text.as_str()))
            }),
            "navigation glyphs are not painted when the native strip leaves no room"
        );
        assert!(output.shapes.iter().all(|shape| {
            shape
                .shape
                .visual_bounding_rect()
                .intersect(shape.clip_rect)
                .right()
                <= 40.0
        }));
    }
}
