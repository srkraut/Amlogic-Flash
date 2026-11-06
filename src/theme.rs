//! Light / dark palettes and egui style setup.

use eframe::egui::{self, Color32, FontFamily, FontId, Rounding, Stroke, TextStyle, Theme, Vec2};

#[derive(Clone, Copy)]
pub struct Palette {
    pub bg: Color32,
    pub card: Color32,
    pub card_alt: Color32,
    pub stroke: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub accent: Color32,
    pub accent_soft: Color32,
    pub on_accent: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub err: Color32,
}

pub const LIGHT: Palette = Palette {
    bg: Color32::from_rgb(0xF3, 0xF5, 0xF9),
    card: Color32::from_rgb(0xFF, 0xFF, 0xFF),
    card_alt: Color32::from_rgb(0xF6, 0xF8, 0xFB),
    stroke: Color32::from_rgb(0xE1, 0xE5, 0xEC),
    text: Color32::from_rgb(0x1B, 0x21, 0x2D),
    muted: Color32::from_rgb(0x6A, 0x73, 0x85),
    accent: Color32::from_rgb(0x2F, 0x6B, 0xF2),
    accent_soft: Color32::from_rgb(0xE6, 0xEE, 0xFE),
    on_accent: Color32::WHITE,
    ok: Color32::from_rgb(0x16, 0x9C, 0x4A),
    warn: Color32::from_rgb(0xC9, 0x7A, 0x06),
    err: Color32::from_rgb(0xD2, 0x2F, 0x2F),
};

pub const DARK: Palette = Palette {
    bg: Color32::from_rgb(0x0E, 0x11, 0x17),
    card: Color32::from_rgb(0x17, 0x1C, 0x25),
    card_alt: Color32::from_rgb(0x1D, 0x23, 0x2E),
    stroke: Color32::from_rgb(0x2A, 0x31, 0x3E),
    text: Color32::from_rgb(0xE6, 0xEA, 0xF2),
    muted: Color32::from_rgb(0x8B, 0x94, 0xA7),
    accent: Color32::from_rgb(0x5B, 0x8C, 0xFF),
    accent_soft: Color32::from_rgb(0x1E, 0x2A, 0x45),
    on_accent: Color32::WHITE,
    ok: Color32::from_rgb(0x34, 0xC7, 0x6F),
    warn: Color32::from_rgb(0xF2, 0xA6, 0x2B),
    err: Color32::from_rgb(0xF4, 0x6B, 0x6B),
};

pub fn palette(ctx: &egui::Context) -> Palette {
    if ctx.style().visuals.dark_mode { DARK } else { LIGHT }
}

fn style_for(style: &mut egui::Style, p: &Palette, dark: bool) {
    style.visuals = if dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    let v = &mut style.visuals;
    v.panel_fill = p.bg;
    v.window_fill = p.card;
    v.window_stroke = Stroke::new(1.0_f32, p.stroke);
    v.extreme_bg_color = p.card_alt;
    v.faint_bg_color = p.card_alt;
    v.override_text_color = Some(p.text);
    v.hyperlink_color = p.accent;
    v.selection.bg_fill = p.accent;
    v.selection.stroke = Stroke::new(1.0_f32, p.on_accent);
    v.window_rounding = Rounding::same(12.0);
    v.menu_rounding = Rounding::same(8.0);
    v.window_shadow.blur = 24.0;
    v.popup_shadow.blur = 12.0;
    for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
        w.rounding = Rounding::same(7.0);
    }
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, p.stroke);
    v.widgets.inactive.bg_fill = p.card_alt;
    v.widgets.inactive.weak_bg_fill = p.card_alt;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, p.stroke);
    v.widgets.hovered.weak_bg_fill = p.accent_soft;
    v.widgets.hovered.bg_fill = p.accent_soft;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, p.accent);
    v.widgets.active.weak_bg_fill = p.accent_soft;
    v.widgets.active.bg_stroke = Stroke::new(1.0_f32, p.accent);

    let s = &mut style.spacing;
    s.item_spacing = Vec2::new(8.0, 7.0);
    s.button_padding = Vec2::new(12.0, 6.0);
    s.interact_size = Vec2::new(36.0, 22.0);
    s.slider_width = 130.0;
    s.window_margin = egui::Margin::same(16.0);

    style.text_styles = [
        (TextStyle::Heading, FontId::new(21.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(14.0, FontFamily::Proportional)),
        (TextStyle::Small, FontId::new(11.5, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(12.5, FontFamily::Monospace)),
    ]
    .into();
}

pub fn install(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    ctx.set_fonts(fonts);
    ctx.style_mut_of(Theme::Light, |s| style_for(s, &LIGHT, false));
    ctx.style_mut_of(Theme::Dark, |s| style_for(s, &DARK, true));
}

/// A rounded card with a title row.
pub fn card<R>(ui: &mut egui::Ui, p: &Palette, icon: &str, title: &str, right: impl FnOnce(&mut egui::Ui), body: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::none()
        .fill(p.card)
        .stroke(Stroke::new(1.0_f32, p.stroke))
        .rounding(Rounding::same(12.0))
        .inner_margin(egui::Margin::symmetric(16.0, 14.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(icon).size(17.0).color(p.accent));
                ui.label(egui::RichText::new(title).size(15.5).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), right);
            });
            ui.add_space(8.0);
            body(ui)
        })
        .inner
}

/// Small rounded "chip" label.
pub fn chip(ui: &mut egui::Ui, text: impl Into<String>, fg: Color32, bg: Color32) {
    egui::Frame::none().fill(bg).rounding(Rounding::same(20.0)).inner_margin(egui::Margin::symmetric(9.0, 3.0)).show(ui, |ui| {
        ui.label(egui::RichText::new(text.into()).size(12.0).color(fg));
    });
}

pub fn tint(c: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), alpha)
}
