use eframe::egui;

use crate::theme;

fn badge_frame(ui: &mut egui::Ui, text: impl Into<String>, color: egui::Color32) -> egui::Response {
    egui::Frame::NONE
        .fill(theme::SURFACE_RAISED)
        .stroke(egui::Stroke::new(1.0, theme::BORDER))
        .corner_radius(egui::CornerRadius::same(4))
        .inner_margin(egui::Margin::symmetric(7, 3))
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(text.into())
                    .monospace()
                    .size(11.0)
                    .color(color),
            );
        })
        .response
}

pub fn count_badge(ui: &mut egui::Ui, count: usize, noun: &str, color: egui::Color32) {
    badge_frame(ui, format!("{count} {noun}"), color);
}

pub fn summary_badge(ui: &mut egui::Ui, count: usize, noun: &str, color: egui::Color32) {
    if count > 0 {
        count_badge(ui, count, noun, color);
    }
}

pub fn key_badge(ui: &mut egui::Ui, key: &str) -> egui::Response {
    badge_frame(ui, key, theme::TEXT_MUTED).interact(egui::Sense::click())
}
