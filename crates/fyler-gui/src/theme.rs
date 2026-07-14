//! Claude Design の Fyler Screens を egui へ写像する表示テーマ。

use eframe::egui;

pub const CANVAS: egui::Color32 = egui::Color32::from_rgb(11, 11, 12);
pub const SURFACE: egui::Color32 = egui::Color32::from_rgb(17, 17, 20);
pub const SURFACE_RAISED: egui::Color32 = egui::Color32::from_rgb(22, 22, 26);
pub const HOVER: egui::Color32 = egui::Color32::from_rgb(28, 28, 33);
pub const BORDER: egui::Color32 = egui::Color32::from_rgb(42, 42, 48);
pub const BORDER_SUBTLE: egui::Color32 = egui::Color32::from_rgb(31, 31, 35);
pub const TEXT: egui::Color32 = egui::Color32::from_rgb(232, 230, 227);
pub const TEXT_SECONDARY: egui::Color32 = egui::Color32::from_rgb(181, 178, 172);
pub const TEXT_MUTED: egui::Color32 = egui::Color32::from_rgb(117, 114, 108);
pub const TEXT_FAINT: egui::Color32 = egui::Color32::from_rgb(74, 72, 68);
pub const ACCENT: egui::Color32 = egui::Color32::from_rgb(255, 107, 26);
/// active pane枠やカーソル行バーに使う、主張を抑えた暗いオレンジ。
pub const ACCENT_DIM: egui::Color32 = egui::Color32::from_rgb(138, 66, 30);
/// 非アクティブpaneへ重ねて暗くする半透明ベール。
pub fn inactive_pane_veil() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(11, 11, 12, 130)
}
pub const BLUE: egui::Color32 = egui::Color32::from_rgb(90, 143, 174);
pub const GREEN: egui::Color32 = egui::Color32::from_rgb(111, 174, 90);
pub const YELLOW: egui::Color32 = egui::Color32::from_rgb(217, 169, 58);
pub const RED: egui::Color32 = egui::Color32::from_rgb(216, 93, 93);

pub const TOOLBAR_HEIGHT: f32 = 34.0;
pub const STATUSBAR_HEIGHT: f32 = 26.0;
pub const TREE_ROW_HEIGHT: f32 = 24.0;

/// アプリ全体へ Fyler Screens の near-black palette と compact spacing を適用する。
pub fn install(context: &egui::Context) {
    context.set_theme(egui::Theme::Dark);
    context.style_mut_of(egui::Theme::Dark, |style| {
        style.visuals = visuals();
        style.spacing.item_spacing = egui::vec2(6.0, 4.0);
        style.spacing.button_padding = egui::vec2(8.0, 4.0);
        style.spacing.interact_size = egui::vec2(32.0, 24.0);
        style.spacing.slider_width = 112.0;
        style.text_styles.insert(
            egui::TextStyle::Heading,
            egui::FontId::new(15.0, egui::FontFamily::Proportional),
        );
        style.text_styles.insert(
            egui::TextStyle::Body,
            egui::FontId::new(13.0, egui::FontFamily::Proportional),
        );
        style.text_styles.insert(
            egui::TextStyle::Button,
            egui::FontId::new(12.0, egui::FontFamily::Proportional),
        );
        style.text_styles.insert(
            egui::TextStyle::Monospace,
            egui::FontId::new(13.0, egui::FontFamily::Monospace),
        );
        style.text_styles.insert(
            egui::TextStyle::Small,
            egui::FontId::new(11.0, egui::FontFamily::Proportional),
        );
    });
}

fn visuals() -> egui::Visuals {
    let mut visuals = egui::Visuals::dark();
    visuals.dark_mode = true;
    visuals.override_text_color = Some(TEXT);
    visuals.panel_fill = CANVAS;
    visuals.window_fill = SURFACE;
    visuals.extreme_bg_color = CANVAS;
    visuals.faint_bg_color = SURFACE_RAISED;
    visuals.code_bg_color = CANVAS;
    visuals.window_stroke = egui::Stroke::new(1.0, BORDER);
    visuals.window_corner_radius = egui::CornerRadius::same(8);
    visuals.error_fg_color = RED;
    visuals.warn_fg_color = YELLOW;
    visuals.hyperlink_color = ACCENT;
    visuals.selection.bg_fill = accent_selection_fill();
    visuals.selection.stroke = egui::Stroke::new(1.0, ACCENT);

    visuals.widgets.noninteractive.bg_fill = SURFACE;
    visuals.widgets.noninteractive.weak_bg_fill = SURFACE;
    visuals.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, BORDER_SUBTLE);
    visuals.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, TEXT_SECONDARY);
    visuals.widgets.noninteractive.corner_radius = egui::CornerRadius::same(4);

    visuals.widgets.inactive.bg_fill = SURFACE_RAISED;
    visuals.widgets.inactive.weak_bg_fill = SURFACE_RAISED;
    visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, BORDER);
    visuals.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, TEXT_SECONDARY);
    visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(6);

    visuals.widgets.hovered.bg_fill = HOVER;
    visuals.widgets.hovered.weak_bg_fill = HOVER;
    visuals.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, BORDER);
    visuals.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, TEXT);
    visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(6);

    visuals.widgets.active.bg_fill = ACCENT;
    visuals.widgets.active.weak_bg_fill = ACCENT;
    visuals.widgets.active.bg_stroke = egui::Stroke::new(1.0, ACCENT);
    visuals.widgets.active.fg_stroke = egui::Stroke::new(1.0, CANVAS);
    visuals.widgets.active.corner_radius = egui::CornerRadius::same(6);

    visuals.widgets.open = visuals.widgets.hovered;
    visuals
}

pub fn accent_selection_fill() -> egui::Color32 {
    egui::Color32::from_rgba_unmultiplied(255, 107, 26, 28)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_keeps_filename_contrast_above_chrome() {
        assert!(
            u16::from(TEXT.r()) + u16::from(TEXT.g()) + u16::from(TEXT.b())
                > u16::from(TEXT_MUTED.r()) + u16::from(TEXT_MUTED.g()) + u16::from(TEXT_MUTED.b())
        );
        assert_ne!(ACCENT, RED);
        assert_eq!(TREE_ROW_HEIGHT, 24.0);
    }
}
