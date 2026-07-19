//! Small shared visual vocabulary so Send/Receive don't drift apart --
//! deliberately minimal (a handful of colors and one card wrapper), not a
//! full design system.

pub const ACCENT: egui::Color32 = egui::Color32::from_rgb(90, 160, 250);
pub const SUCCESS: egui::Color32 = egui::Color32::from_rgb(90, 200, 130);
pub const ERROR: egui::Color32 = egui::Color32::from_rgb(230, 100, 100);
pub const WARNING: egui::Color32 = egui::Color32::from_rgb(230, 170, 80);

/// A softly rounded, faintly shaded panel for grouping one logical section
/// of controls -- used instead of bare `ui.label`/`ui.horizontal` chains so
/// the tabs read as a few clear steps rather than a flat list of widgets.
pub fn card<R>(ui: &mut egui::Ui, add_contents: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::group(ui.style())
        .fill(ui.visuals().faint_bg_color)
        .rounding(egui::Rounding::same(8.0))
        .inner_margin(egui::Margin::same(12.0))
        .show(ui, add_contents)
        .inner
}

/// A small colored status dot + label, e.g. for "Listening" / "Idle".
pub fn status_dot(ui: &mut egui::Ui, color: egui::Color32, label: &str) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 4.5, color);
        ui.label(label);
    });
}

/// A read-only, greyed field showing a value sourced from the shared
/// Settings identity (Mail's From/Location, Social's Author) instead of a
/// `TextEdit`. Purely a display -- there's a dedicated Settings button
/// (⚙, top right) for actually changing these now, so clicking a compose
/// field no longer also opens Settings; that used to be the only way in
/// (before the dedicated button existed) but became a confusing second
/// affordance once it wasn't.
pub fn identity_field(ui: &mut egui::Ui, value: Option<&str>) {
    let text = match value {
        Some(v) if !v.trim().is_empty() => egui::RichText::new(v).color(ui.visuals().text_color()),
        _ => egui::RichText::new("(not set -- see Settings)").italics().weak(),
    };
    egui::Frame::none()
        .fill(ui.visuals().faint_bg_color)
        .rounding(egui::Rounding::same(4.0))
        .inner_margin(egui::Margin::symmetric(6.0, 4.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(text);
        });
}
