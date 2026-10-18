//! Loupe live view and the tether bar. Shown whenever tethered capture is running, including
//! when the library has no photo selected yet.

use egui::{Color32, Pos2, Rect, Vec2, pos2, vec2};

use crate::LightcraftApp;
use crate::i18n::tr;

pub fn pump(app: &mut LightcraftApp) -> bool {
    lightcraft_engine::tether::pump(&mut app.session)
}

/// Draw the live view in `canvas`. Returns whether tethered capture owns the loupe.
pub fn show(app: &mut LightcraftApp, ui: &mut egui::Ui, canvas: Rect) -> bool {
    let status = app.session.tether.status();
    if !status.running && status.error.is_none() {
        return false;
    }
    ui.painter().rect_filled(canvas, 0.0, Color32::from_rgb(18, 18, 18));
    let bar_h = 118.0;
    let bar = Rect::from_min_max(pos2(canvas.left(), canvas.bottom() - bar_h), canvas.max);
    let picture = Rect::from_min_max(canvas.min, pos2(canvas.right(), bar.top()));
    if let Some(frame) = app.session.tether.frame() {
        if let Some(tex) = texture(ui, &frame) {
            paint_frame(ui, picture, &tex, frame.width, frame.height);
        }
    } else {
        let text = if status.connected { tr("Waiting for the live view") } else { tr("Looking for a Sony camera in PC Remote") };
        ui.painter().text(picture.center(), egui::Align2::CENTER_CENTER, text, egui::FontId::proportional(16.0), Color32::from_gray(180));
    }
    if status.magnified && status.connected {
        magnifier_pad(app, ui, picture);
    }
    let mut bar_ui = ui.new_child(egui::UiBuilder::new().max_rect(bar.shrink2(vec2(12.0, 8.0))).layout(egui::Layout::top_down(egui::Align::Min)));
    let title = if status.model.is_empty() { tr("Tethered Capture").to_string() } else { status.model.clone() };
    let title = if status.exposure_comp.is_empty() { title } else { format!("{title}   {} {}", tr("EC"), status.exposure_comp) };
    bar_ui.label(egui::RichText::new(title).color(Color32::WHITE));
    bar_ui.horizontal(|ui| {
        dial(app, ui, "Aperture", "aperture", &status.aperture, &status.aperture_choices, status.connected);
        dial(app, ui, "Shutter", "shutter", &status.shutter, &status.shutter_choices, status.connected);
        dial(app, ui, "ISO", "iso", &status.iso, &status.iso_choices, status.connected);
        if let Some(wb) = &status.white_balance {
            dial(app, ui, "White Balance", "whiteBalance", wb, &status.white_balance_choices, status.connected);
        }
        if ui.add_enabled(status.connected, egui::Button::new(tr("Focus"))).clicked() {
            go(app, ui, "tether.set", serde_json::json!({"focus": true}));
        }
        if ui.add_enabled(status.connected, egui::Button::new(if status.magnified { tr("Magnifier on") } else { tr("Magnify") })).clicked() {
            go(app, ui, "tether.set", serde_json::json!({"magnify": !status.magnified}));
        }
        if ui.add_enabled(status.connected, egui::Button::new(tr("Shoot"))).clicked() {
            go(app, ui, "tether.capture", serde_json::json!({}));
        }
        if ui.button(tr("Stop")).clicked() {
            go(app, ui, "tether.stop", serde_json::json!({}));
        }
    });
    let note = if let Some(err) = &status.error {
        err.clone()
    } else if !status.hint.is_empty() {
        status.hint.clone()
    } else {
        tr("On the camera: USB Connection → PC Remote, and PC Save Image Size → Original.").to_string()
    };
    bar_ui.label(egui::RichText::new(note).size(11.0).color(Color32::from_gray(170)));
    true
}

fn dial(app: &mut LightcraftApp, ui: &mut egui::Ui, label: &str, name: &str, value: &str, choices: &[(u64, String)], ready: bool) {
    let shown = if value.is_empty() { "—".to_string() } else { value.to_string() };
    ui.vertical(|ui| {
        ui.label(egui::RichText::new(tr(label)).size(11.0).color(Color32::from_gray(170)));
        ui.horizontal(|ui| {
            if ui.add_enabled(ready, egui::Button::new("−")).clicked() {
                go(app, ui, "tether.set", serde_json::json!({"dial": name, "step": -1}));
            }
            let button = ui.add_enabled(ready, egui::Button::new(egui::RichText::new(&shown).monospace()));
            if ready && !choices.is_empty() {
                egui::Popup::menu(&button).show(|ui| {
                    ui.set_min_width(148.0);
                    egui::ScrollArea::vertical().max_height(280.0).show(ui, |ui| {
                        for (value, label) in choices {
                            if ui.selectable_label(label == &shown, label).clicked() {
                                go(app, ui, "tether.set", serde_json::json!({"dial": name, "value": value}));
                            }
                        }
                    });
                });
            }
            if ui.add_enabled(ready, egui::Button::new("+")).clicked() {
                go(app, ui, "tether.set", serde_json::json!({"dial": name, "step": 1}));
            }
        });
    });
    ui.add_space(10.0);
}

/// Stops from middle gray in the middle of the live view. The body does not publish a
/// separate meter needle, so this is read from the picture it is already sending.
fn magnifier_pad(app: &mut LightcraftApp, ui: &mut egui::Ui, picture: Rect) {
    let origin = pos2(picture.left() + 12.0, picture.bottom() - 78.0);
    let mut pad = ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_min_size(origin, vec2(96.0, 66.0))));
    pad.vertical(|ui| {
        ui.horizontal(|ui| {
            ui.add_space(28.0);
            if ui.button("↑").clicked() {
                go(app, ui, "tether.set", serde_json::json!({"nudge": "up"}));
            }
        });
        ui.horizontal(|ui| {
            if ui.button("←").clicked() {
                go(app, ui, "tether.set", serde_json::json!({"nudge": "left"}));
            }
            if ui.button("↓").clicked() {
                go(app, ui, "tether.set", serde_json::json!({"nudge": "down"}));
            }
            if ui.button("→").clicked() {
                go(app, ui, "tether.set", serde_json::json!({"nudge": "right"}));
            }
        });
    });
}

fn go(app: &mut LightcraftApp, ui: &egui::Ui, id: &str, params: serde_json::Value) {
    if let Err(e) = app.session.execute(id, &params) {
        app.toast(ui.ctx(), e.to_string());
    }
}

#[derive(Clone)]
struct LiveTex {
    frame_gen: u64,
    tex: egui::TextureHandle,
}

fn texture(ui: &egui::Ui, frame: &lightcraft_engine::tether::LiveFrame) -> Option<LiveTex> {
    let w = usize::try_from(frame.width).unwrap_or(0);
    let h = usize::try_from(frame.height).unwrap_or(0);
    if w == 0 || h == 0 || frame.rgba.len() != w.saturating_mul(h).saturating_mul(4) {
        return None;
    }
    let id = egui::Id::new("tether-live");
    if let Some(existing) = ui.data(|d| d.get_temp::<LiveTex>(id))
        && existing.frame_gen == frame.generation
    {
        return Some(existing);
    }
    let image = egui::ColorImage::from_rgba_unmultiplied([w, h], &frame.rgba);
    let tex = ui.ctx().load_texture("tether-live", image, egui::TextureOptions::LINEAR);
    let stored = LiveTex { frame_gen: frame.generation, tex };
    ui.data_mut(|d| d.insert_temp(id, stored.clone()));
    Some(stored)
}

fn paint_frame(ui: &egui::Ui, picture: Rect, tex: &LiveTex, width: u32, height: u32) {
    let aspect = (width as f32) / (height.max(1) as f32);
    let mut dw = picture.width();
    let mut dh = if aspect > 0.0 { dw / aspect } else { picture.height() };
    if dh > picture.height() {
        dh = picture.height();
        dw = dh * aspect;
    }
    let dest = Rect::from_center_size(picture.center(), Vec2::new(dw, dh));
    ui.painter().image(tex.tex.id(), dest, Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)), Color32::WHITE);
}
