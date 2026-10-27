//! Negative Craft: the film section above Light. Drawn only after Window ▸ Edit Sections ▸
//! Negative Craft, or when this photo is already converted.

use super::sliders::{control, section as edit_section, sub_title};
use super::*;

pub(super) fn section(app: &mut LightcraftApp, ui: &mut egui::Ui, id: PhotoId, d: &DevelopSettings) {
    edit_section(app, ui, d, "film", "Negative Craft", |app, ui, d| {
        let t = Tokens::get(ui.ctx());
        egui::Frame::NONE.inner_margin(egui::Margin { left: 24, right: 22, top: 2, bottom: 6 }).show(ui, |ui| {
            stock_picker(app, ui, d);
            ui.add_space(12.0);
            if d.film.enabled {
                tools(app, ui, d);
                if let Some(hint) = click_hint(app.ui.tool.as_str()) {
                    callout(ui, hint);
                }
                ui.add_space(10.0);
                crop(app, ui);
                ui.add_space(8.0);
                if is_color(d) {
                    scan_print(app, ui, d);
                    ui.add_space(8.0);
                    looks(app, ui, d);
                }
            } else {
                ui.add_space(4.0);
                if wide_button(ui, "filmConvert", "Convert", true).clicked() {
                    let result = app.run("film.setEnabled", json!({"enabled": true}));
                    show_result(app, ui, result);
                }
                ui.add_space(10.0);
                ui.label(
                    RichText::new(crate::i18n::tr("Leave the blank film in the scan. Convert prints from that border."))
                        .font(t.font(12.0))
                        .color(t.text_dim),
                );
            }
        });
        if d.film.enabled {
            control(app, ui, d, "film.inset", true);
            for c in ["film.exposure", "film.grade", "film.black"] {
                control(app, ui, d, c, true);
            }
            if is_color(d) {
                divider(ui);
                for c in
                    ["film.temp", "film.tint", "film.shadowTemp", "film.shadowTint", "film.midTemp", "film.midTint", "film.highTemp", "film.highTint"]
                {
                    control(app, ui, d, c, true);
                }
            }
            let open = app.ui.flyout_open("filmMore");
            if flyout_row(ui, "filmMore", "More", Icon::Sliders, open).clicked() {
                app.ui.toggle_flyout("filmMore");
            }
            if open {
                more(app, ui, id, d);
            }
        }
        ui.add_space(6.0);
    });
}

fn stock_picker(app: &mut LightcraftApp, ui: &mut egui::Ui, d: &DevelopSettings) {
    use lightcraft_develop::FilmStock;
    // Slides stay on `film.setStock` so an old frame still opens. They are not a choice here.
    let stocks = [(FilmStock::ColorNeg, "Color Neg", "colorNeg"), (FilmStock::BwNeg, "B&W", "bwNeg")];
    let items: Vec<(&str, &str)> = stocks.iter().map(|(_, label, key)| (*label, *key)).collect();
    let active = stocks.iter().position(|(s, _, _)| *s == d.film.stock);
    if let Some(i) = crate::widgets::segmented(ui, "filmStock", &items, active, 2)
        && stocks[i].0 != d.film.stock
    {
        if stocks[i].0 != FilmStock::ColorNeg && matches!(app.ui.tool.as_str(), "filmGray" | "filmShadow" | "filmHighlight") {
            app.ui.tool.clear();
        }
        let _ = app.run("film.setStock", json!({"stock": stocks[i].0}));
    }
}

fn show_result(app: &mut LightcraftApp, ui: &egui::Ui, result: Result<Value, String>) {
    match result {
        Ok(v) => {
            if let Some(err) = v.get("frameError").and_then(Value::as_str) {
                app.toast(ui.ctx(), err);
            }
        }
        Err(e) => app.toast_error(ui.ctx(), e),
    }
}

fn is_color(d: &DevelopSettings) -> bool {
    d.film.stock == lightcraft_develop::FilmStock::ColorNeg
}

fn click_hint(tool: &str) -> Option<&'static str> {
    match tool {
        "filmBase" => Some("Click the blank film around the picture. A pale border is fine. Not a blown-out lamp."),
        "filmGray" => Some("Click a tone that should print gray."),
        "filmShadow" => Some("Click a dark tone that should be neutral."),
        "filmHighlight" => Some("Click a light tone that should be neutral."),
        _ => None,
    }
}

fn callout(ui: &mut egui::Ui, hint: &str) {
    let t = Tokens::get(ui.ctx());
    ui.add_space(8.0);
    egui::Frame::NONE
        .fill(t.button)
        .stroke(Stroke::new(1.0, t.accent))
        .corner_radius(egui::CornerRadius::same(4))
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.label(RichText::new(crate::i18n::tr(hint)).font(t.semibold(14.0)).color(t.text));
        });
}

fn crop(app: &mut LightcraftApp, ui: &mut egui::Ui) {
    let t = Tokens::get(ui.ctx());
    ui.label(RichText::new(crate::i18n::tr("Crop")).font(t.font(12.0)).color(t.text_dim));
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        if text_button(ui, "filmCrop", "Auto", false).clicked()
            && let Err(e) = app.run("film.cropToImage", json!({}))
        {
            app.toast(ui.ctx(), e);
        }
        if text_button(ui, "filmCropAdjust", "Adjust", app.ui.right == RightPanel::Crop).clicked() {
            app.ui.tool.clear();
            let _ = app.run("panel.crop", json!({}));
        }
    });
}

fn tools(app: &mut LightcraftApp, ui: &mut egui::Ui, d: &DevelopSettings) {
    let t = Tokens::get(ui.ctx());
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let (swatch, _) = ui.allocate_exact_size(vec2(24.0, 24.0), Sense::hover());
        ui.painter().rect(
            swatch,
            egui::CornerRadius::same(4),
            base_color(d.film.base()),
            Stroke::new(1.0, t.button_border),
            egui::StrokeKind::Inside,
        );
        if text_button(ui, "filmAnalyze", "Auto", false).clicked() {
            let result = app.run("film.analyze", json!({}));
            show_result(app, ui, result);
        }
        if text_button(ui, "filmBase", "Border", app.ui.tool == "filmBase").clicked() {
            toggle_tool(app, "filmBase");
        }
        if is_color(d) && text_button(ui, "filmGray", "Gray", app.ui.tool == "filmGray").clicked() {
            toggle_tool(app, "filmGray");
        }
    });
}

fn scan_print(app: &mut LightcraftApp, ui: &mut egui::Ui, d: &DevelopSettings) {
    use lightcraft_develop::{FilmPrint, FilmScan};
    let t = Tokens::get(ui.ctx());
    ui.label(RichText::new(crate::i18n::tr("Scan")).font(t.font(12.0)).color(t.text_dim));
    ui.add_space(4.0);
    let scans = [(FilmScan::Camera, "Camera", "camera"), (FilmScan::Minilab, "Minilab", "minilab"), (FilmScan::Flat, "Flat", "flat")];
    let items: Vec<(&str, &str)> = scans.iter().map(|(_, l, k)| (*l, *k)).collect();
    let active = scans.iter().position(|(v, _, _)| *v == d.film.scan);
    if let Some(i) = crate::widgets::segmented(ui, "filmScan", &items, active, 3)
        && scans[i].0 != d.film.scan
    {
        let _ = app.run("film.setScan", json!({"scan": scans[i].0}));
    }
    ui.add_space(8.0);
    ui.label(RichText::new(crate::i18n::tr("Print")).font(t.font(12.0)).color(t.text_dim));
    ui.add_space(4.0);
    let prints = [
        (FilmPrint::Neutral, "Neutral", "neutral"),
        (FilmPrint::Portrait, "Portrait", "portrait"),
        (FilmPrint::Vivid, "Vivid", "vivid"),
        (FilmPrint::Lab, "Lab", "lab"),
    ];
    let items: Vec<(&str, &str)> = prints.iter().map(|(_, l, k)| (*l, *k)).collect();
    let active = prints.iter().position(|(v, _, _)| *v == d.film.print);
    if let Some(i) = crate::widgets::segmented(ui, "filmPrint", &items, active, 4)
        && prints[i].0 != d.film.print
    {
        let _ = app.run("film.setPrint", json!({"print": prints[i].0}));
    }
}

fn looks(app: &mut LightcraftApp, ui: &mut egui::Ui, d: &DevelopSettings) {
    let t = Tokens::get(ui.ctx());
    ui.label(RichText::new(crate::i18n::tr("Look")).font(t.font(12.0)).color(t.text_dim));
    ui.add_space(4.0);
    let looks = [
        ("lc.nc-neutral", "Neutral", "neutral"),
        ("lc.nc-warm", "Warm", "warm"),
        ("lc.nc-cool", "Cool", "cool"),
        ("lc.nc-paper", "Paper", "paper"),
        ("lc.nc-cine", "Cinematic", "cine"),
    ];
    let items: Vec<(&str, &str)> = looks.iter().map(|(_, label, key)| (*label, *key)).collect();
    let active = looks.iter().position(|(id, _, _)| look_on(app, d, id));
    if let Some(i) = crate::widgets::segmented(ui, "filmLook", &items, active, 3) {
        let _ = app.run("preset.apply", json!({"id": looks[i].0}));
    }
}

fn more(app: &mut LightcraftApp, ui: &mut egui::Ui, id: PhotoId, d: &DevelopSettings) {
    let color = is_color(d);
    if color {
        let t = Tokens::get(ui.ctx());
        ui.add_space(4.0);
        ui.label(RichText::new(crate::i18n::tr("Cast Auto removed")).font(t.font(12.0)).color(t.text_dim));
        ui.label(RichText::new(lightcraft_pipeline::film::cast_readout(&d.film)).font(t.font(13.0)).color(t.text));
        ui.add_space(6.0);
        control(app, ui, d, "film.presat", true);
    }
    control(app, ui, d, "film.shoulder", true);
    if color {
        sub_title(ui, "Mask");
        for c in ["film.baseR", "film.baseG", "film.baseB", "film.gammaR", "film.gammaG", "film.gammaB"] {
            control(app, ui, d, c, true);
        }
    }
    sub_title(ui, "Source");
    egui::Frame::NONE.inner_margin(egui::Margin { left: 24, right: 22, top: 0, bottom: 6 }).show(ui, |ui| {
        let t = Tokens::get(ui.ctx());
        if color {
            ui.label(
                RichText::new(crate::i18n::tr("Scene balances neutrals in the picture. Film keeps the stock's own color."))
                    .font(t.font(12.0))
                    .color(t.text_dim),
            );
            ui.add_space(4.0);
            let intents = [(lightcraft_develop::FilmIntent::Stock, "Film", "stock"), (lightcraft_develop::FilmIntent::Scene, "Scene", "scene")];
            let items: Vec<(&str, &str)> = intents.iter().map(|(_, l, k)| (*l, *k)).collect();
            let active = intents.iter().position(|(v, _, _)| *v == d.film.intent);
            if let Some(i) = crate::widgets::segmented(ui, "filmIntent", &items, active, 2)
                && intents[i].0 != d.film.intent
            {
                let _ = app.run("film.setIntent", json!({"intent": intents[i].0}));
            }
            ui.add_space(6.0);
        }
        ui.label(RichText::new(crate::i18n::tr("Leave this on Linear unless the file is still in gamma.")).font(t.font(12.0)).color(t.text_dim));
        ui.add_space(4.0);
        let captures = [
            (lightcraft_develop::FilmCapture::Linear, "Linear", "linear"),
            (lightcraft_develop::FilmCapture::Gamma18, "1.8", "gamma18"),
            (lightcraft_develop::FilmCapture::Gamma22, "2.2", "gamma22"),
        ];
        let items: Vec<(&str, &str)> = captures.iter().map(|(_, l, k)| (*l, *k)).collect();
        let active = captures.iter().position(|(v, _, _)| *v == d.film.capture);
        if let Some(i) = crate::widgets::segmented(ui, "filmCapture", &items, active, 3)
            && captures[i].0 != d.film.capture
        {
            let _ = app.run("film.setCapture", json!({"capture": captures[i].0}));
        }
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            if color && text_button(ui, "filmShadow", "Shadow", app.ui.tool == "filmShadow").clicked() {
                toggle_tool(app, "filmShadow");
            }
            if color && text_button(ui, "filmHighlight", "Highlight", app.ui.tool == "filmHighlight").clicked() {
                toggle_tool(app, "filmHighlight");
            }
            if text_button(ui, "filmAnalyzeRoll", "Match roll", false).clicked() {
                let _ = app.run("film.analyzeRoll", json!({}));
            }
        });
        refresh_meter(app, id, d);
        if let Some((_, _, bins, _, _)) = app.ui.film_meter.clone()
            && !bins.is_empty()
        {
            ui.add_space(8.0);
            let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::hover());
            let n = bins.len().max(1) as f32;
            let w = rect.width() / n;
            for (i, b) in bins.iter().enumerate() {
                let h = rect.height() * b.clamp(0.0, 1.0);
                let x = rect.left() + i as f32 * w;
                let bar = Rect::from_min_max(pos2(x + 0.5, rect.bottom() - h), pos2(x + w - 0.5, rect.bottom()));
                ui.painter().rect_filled(bar, 0.0, t.text_dim);
            }
        }
    });
}

fn toggle_tool(app: &mut LightcraftApp, id: &str) {
    app.ui.tool = if app.ui.tool == id { String::new() } else { id.to_string() };
}

fn look_on(app: &LightcraftApp, d: &DevelopSettings, id: &str) -> bool {
    let Some(preset) = app.session.presets.iter().find(|p| p.id == id) else { return false };
    let Some(patch) = preset.settings.get("film").and_then(Value::as_object) else { return false };
    let Ok(film) = serde_json::to_value(&d.film) else { return false };
    patch.iter().all(|(k, want)| match (film.get(k), want) {
        (Some(Value::Number(a)), Value::Number(b)) => match (a.as_f64(), b.as_f64()) {
            (Some(a), Some(b)) => (a - b).abs() < 1e-6,
            _ => false,
        },
        (Some(got), want) => got == want,
        _ => false,
    })
}

fn base_color(base: [f64; 3]) -> Color32 {
    let enc = |v: f64| -> u8 {
        let x = (v as f32).clamp(0.0, 1.0);
        let y = if x <= 0.0031308 { x * 12.92 } else { 1.055 * x.powf(1.0 / 2.4) - 0.055 };
        (y.clamp(0.0, 1.0) * 255.0).round() as u8
    };
    Color32::from_rgb(enc(base[0]), enc(base[1]), enc(base[2]))
}

fn wide_button(ui: &mut egui::Ui, id: &str, label: &str, accent: bool) -> egui::Response {
    let label = crate::i18n::tr(label);
    let t = Tokens::get(ui.ctx());
    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::click());
    register(ui, format!("button:{id}"), r);
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, accent, label));
    let fill = if accent {
        if resp.hovered() { t.accent.gamma_multiply(0.85) } else { t.accent }
    } else if resp.hovered() {
        t.hover
    } else {
        t.button
    };
    ui.painter().rect(r, egui::CornerRadius::same(4), fill, Stroke::new(1.0, if accent { fill } else { t.button_border }), egui::StrokeKind::Inside);
    ui.painter().text(r.center(), Align2::CENTER_CENTER, label, t.semibold(13.0), if accent { Color32::WHITE } else { t.text });
    resp
}

/// Density readout depends on the film settings and how the scan is oriented, not on Light or Color.
fn meter_key(d: &DevelopSettings) -> u64 {
    let text = serde_json::to_string(&json!({"orientation": d.orientation, "film": d.film})).unwrap_or_default();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn refresh_meter(app: &mut LightcraftApp, id: PhotoId, d: &DevelopSettings) {
    let hash = meter_key(d);
    let fresh = app.ui.film_meter.as_ref().is_some_and(|(pid, h, _, _, _)| *pid == id.0 && *h == hash);
    if fresh {
        return;
    }
    let Ok(v) = app.run("film.inspect", json!({})) else {
        // Remember the failure for this photo and key, so a missing file is not read every frame.
        app.ui.film_meter = Some((id.0, hash, Vec::new(), 0.0, 0.0));
        return;
    };
    let bins = v.get("bins").and_then(|b| b.as_array()).map(|a| a.iter().filter_map(|n| n.as_f64().map(|x| x as f32)).collect()).unwrap_or_default();
    let rebate = v.get("rebateLevel").and_then(|n| n.as_f64()).unwrap_or(0.0);
    let dominant = v.get("dominant").and_then(|n| n.as_f64()).unwrap_or(0.0);
    app.ui.film_meter = Some((id.0, hash, bins, rebate, dominant));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_film_meter_ignores_sliders_that_do_not_change_the_scan() {
        let mut d = DevelopSettings::default();
        d.film.enabled = true;
        d.film.base_r = 0.4;
        let key = meter_key(&d);
        d.light.exposure = 1.5;
        d.color.saturation = 20.0;
        assert_eq!(meter_key(&d), key, "Light and Color do not reread the density");
        d.film.base_g = 0.3;
        assert_ne!(meter_key(&d), key);
        d.film.base_g = 1.0;
        d.orientation = lightcraft_geom::Orientation::Rotate90;
        assert_ne!(meter_key(&d), key, "orientation changes which edge is the border");
    }
}
