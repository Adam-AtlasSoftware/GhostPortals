//! Look and feel shared by the manager and the installer.

use eframe::egui::{self, Color32, CornerRadius, Margin, Stroke};

/// A soft violet: the portal glow.
pub const ACCENT: Color32 = Color32::from_rgb(124, 106, 240);
pub const GOOD: Color32 = Color32::from_rgb(64, 186, 128);
pub const WARN: Color32 = Color32::from_rgb(230, 160, 60);
pub const MUTED: Color32 = Color32::from_rgb(140, 140, 160);

const BG: Color32 = Color32::from_rgb(22, 22, 30);
const CARD: Color32 = Color32::from_rgb(32, 32, 44);
const EDGE: Color32 = Color32::from_rgb(52, 52, 70);

pub fn apply(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = BG;
    v.extreme_bg_color = Color32::from_rgb(16, 16, 22);
    v.selection.bg_fill = ACCENT;
    v.hyperlink_color = ACCENT;
    v.widgets.inactive.corner_radius = CornerRadius::same(6);
    v.widgets.hovered.corner_radius = CornerRadius::same(6);
    v.widgets.active.corner_radius = CornerRadius::same(6);
    // One look regardless of the system theme.
    ctx.set_theme(egui::Theme::Dark);
    ctx.set_visuals_of(egui::Theme::Dark, v);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 8.0);
        s.spacing.button_padding = egui::vec2(12.0, 5.0);
    });
}

/// A rounded panel grouping related controls.
pub fn card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, EDGE))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
    ui.add_space(10.0);
}

/// A one-line notice with a coloured edge.
pub fn banner(ui: &mut egui::Ui, color: Color32, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.18))
        .stroke(Stroke::new(1.0, color))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(add);
        });
    ui.add_space(10.0);
}

/// A status light.
pub fn dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 6.0, color);
    ui.painter().circle_stroke(rect.center(), 8.0, Stroke::new(2.0, color.gamma_multiply(0.35)));
}
