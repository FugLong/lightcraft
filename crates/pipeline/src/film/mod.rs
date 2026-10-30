//! Negative Craft: convert a film scan in scene-linear transmission, before white balance.
//!
//! The model, in order (see `docs/negative-craft.md`):
//!
//! 1. **Density.** Clear film (the base) is the reference: `D_c = log10(base_c / T_c)`. On a raw
//!    file with its own colour matrix that is measured in white-balanced camera RGB; the matrix
//!    only stores the finished positive in Rec.2020 ([`scanner_space`]).
//! 2. **Dyes.** Hanson's integral-to-analytical inverse recovers dye amounts from the camera's
//!    integral densities ([`dye_matrix`]). A gray subject is equal dye, not equal integral density.
//! 3. **Filter pack.** Fog stays 0: the rebate is zero density. The axis is the slope of the
//!    least coloured whites, so a white prints gray and a colour off that slope does not.
//!    A Gray pick or Match roll is [`solve::Axis::Keep`].
//! 4. **Print.** Scene light is exponential in density, as exposure is in a negative:
//!    `L = 10^(contrast · (D' − mid) / 0.6)`, with the black anchor subtracted so it prints at
//!    zero and the middle prints at 0.18. Every channel uses the same curve, so a gray subject
//!    stays gray at any contrast.
//!
//! Convert is a lab printer, in this order:
//!
//! 1. Linearize the scan ([`linear`]).
//! 2. Find the rebate and take Dmin from that ring ([`frame`], [`base`]). No rebate → refuse,
//!    unless a Border click already stored a mask. A histogram of the picture is never Dmin.
//! 3. Density relative to that mask, then Hanson ([`kernel`]).
//! 4. Filter pack: fog 0, axis through the neutral whites ([`solve::Axis::Lab`]). Per-frame tone.
//!
//! A failed measure leaves conversion off. `map.wgsl`'s `film_k` is [`FilmKernel::apply`].
//!
//! No plugin binary, scanner LUT, or manufacturer's stock profile is used here.

mod base;
mod frame;
mod kernel;
mod roll;
mod solve;
#[cfg(test)]
mod tests;

use std::borrow::Cow;

use lightcraft_develop::{Film, FilmCapture, FilmFrame, FilmScan, FilmStock};
use lightcraft_geom::{Affine, CropGeometry, Point, Rect};
use lightcraft_raster::Rgb32f;

pub use self::base::Base;
pub use self::frame::Detection;
pub use self::kernel::{FILM_GAMMA, FilmKernel, GREY, WORDS, dye_matrix};
pub use self::roll::{DRIFT_WARN, SharedFrame, share_roll};
pub use self::solve::{Axis, Print};
use crate::SourceInfo;

/// Fraction of the detected frame left out on every side when the picture is measured: the
/// blurred line where the picture meets the rebate.
const EDGE_TRIM: f64 = 0.03;
/// Most pixels a measurement reads. A larger view is sampled on a grid.
const MAX_SAMPLES: usize = 250_000;

const NOTHING_TO_MEASURE: &str = "This scan has too few pixels to measure.";
const NO_REBATE: &str = "No blank film found. Click Border on the orange unexposed film.";
const BORDER_BLOWN: &str = "that spot is blown out — click the blank film, not the white lamp";
const BORDER_DARK: &str = "that spot is too dark — click the blank film, not the picture";
const BORDER_PLAIN: &str = "that spot is not orange film — click the blank orange border";

/// What one frame measured.
#[derive(Clone, Debug, Default)]
pub struct FrameStats {
    pub base: [f64; 3],
    /// Luminance of the base.
    pub rebate_level: f64,
    /// The base came from blank film found on this scan. A frame without one must not move a
    /// roll's shared mask colour.
    pub mask_measured: bool,
    pub print: Option<Print>,
    /// Histogram of the picture's luminance density, 0 to 2.5, scaled to its tallest bin.
    pub bins: Vec<f32>,
}

#[derive(Clone, Debug)]
pub struct Analysis {
    pub film: Film,
    pub stats: FrameStats,
    /// Set when nothing could be measured. Conversion is then left off.
    pub frame_error: Option<String>,
}

impl Print {
    /// Write this print onto `film`: the measurement, contrast and exposure. Base, frame and the
    /// looks are left alone.
    pub fn write_to(&self, film: &mut Film) {
        self.write_measurement_to(film);
        film.grade = self.contrast;
        film.exposure = self.exposure;
    }

    /// Write only what was measured, so the user's Contrast and Exposure stay.
    pub fn write_measurement_to(&self, film: &mut Film) {
        film.set_fog(self.fog);
        film.set_axis(self.axis);
        film.d_lo = self.black;
        film.d_hi = self.highlight;
        film.pivot = self.mid;
        film.mid_a = self.mid_a;
        film.mid_b = self.mid_b;
    }
}

/// Find the picture on a scan in its stored encoding.
pub fn detect_frame(img: &Rgb32f, film: &Film) -> Option<Detection> {
    frame::detect(&linear(img, film.capture), film.stock)
}

/// Convert a whole scan: find the rebate, take Dmin from it, print with a lab pack.
/// Manual channel trims and neutral picks are cleared; the looks are kept.
/// A failed measure leaves conversion off — a white default mask must not print.
pub fn analyze_image(img: &Rgb32f, film: &Film) -> Analysis {
    lab_convert(img, film, false)
}

/// Convert a view the user already cropped. The crop is the picture: nothing outside it is read.
/// Blank film still inside the crop is the base; without any, a Border click already stored is kept.
pub fn analyze_picture(img: &Rgb32f, film: &Film) -> Analysis {
    lab_convert(img, film, true)
}

/// One lab conversion. `cropped` means the raster *is* the picture (the surround is already gone).
fn lab_convert(img: &Rgb32f, film: &Film, cropped: bool) -> Analysis {
    let mut work = fresh(film);
    let lin = linear(img, work.capture);
    let Some(found) = frame::detect(&lin, work.stock) else {
        return refused(film, NOTHING_TO_MEASURE);
    };
    // A crop is the search window. If it still holds a rebate, tone the picture inside
    // that rebate — a lab does not print the border.
    work.frame = if cropped && !found.bordered { FilmFrame::default() } else { found.frame };
    let base = match mask_of(&found, film) {
        Ok(base) => base,
        Err(why) => return refused(film, why),
    };
    work.set_base(base.rgb);
    let stats = measure(img, &work, &base, Axis::Lab, found.mask);
    finish(work, stats, film)
}

/// Dmin is orange blank film, or a Border click that already is. A pale ring and a picture
/// histogram are not: either one leaves the orange mask in, and the print comes out blue.
fn mask_of(found: &Detection, film: &Film) -> Result<Base, &'static str> {
    if found.mask && base_ok(found.base.rgb, film.stock) {
        return Ok(found.base);
    }
    if has_mask(film) && base_ok(film.base(), film.stock) {
        return Ok(Base { rgb: film.base(), lamp_cut: found.base.lamp_cut, clip: found.base.clip });
    }
    Err(NO_REBATE)
}

fn base_ok(rgb: [f64; 3], stock: FilmStock) -> bool {
    if !rgb.iter().all(|c| c.is_finite() && *c > 1e-4) {
        return false;
    }
    if stock.negative() && !stock.mono() {
        return base::is_orange(rgb.map(|v| v as f32));
    }
    true
}

/// Which dye correction Convert measures with.
///
/// Camera RGB (a raw, before the colour matrix) uses Camera Hanson unless the user already
/// picked Minilab or Flat. A rendered file stays Flat: Camera Hanson there, with fog 0 and
/// axis 1, tints a gray the file has already made equal.
pub fn prepare_scan(info: &SourceInfo, film: &mut Film) {
    if info.transmission {
        if !film.scan_picked && film.scan == FilmScan::Flat {
            film.scan = FilmScan::Camera;
        }
        return;
    }
    if film.scan == FilmScan::Camera && scanner_space(info).is_none() {
        film.scan = FilmScan::Flat;
    }
}

fn has_mask(film: &Film) -> bool {
    film.base() != Film::default().base()
}

/// Measure `film`'s frame again with its own base: after the margin, a base pick, or a neutral
/// pick. The gray axis runs through the picks when there are any, otherwise the stored pack.
pub fn place(img: &Rgb32f, film: &Film) -> Option<Print> {
    let axis = match axis_from_picks(film) {
        Some((fog, axis)) => Axis::Keep(fog, axis),
        None => Axis::Keep(film.fog(), film.axis()),
    };
    place_with(img, film, axis)
}

/// Print `film`'s frame again with its gray axis as it is (a roll's, or a gray pick's).
pub fn place_keeping_axis(img: &Rgb32f, film: &Film) -> Option<Print> {
    place_with(img, film, Axis::Keep(film.fog(), film.axis()))
}

/// The density histogram and base readout for the current frame, without writing settings.
pub fn inspect(img: &Rgb32f, film: &Film) -> FrameStats {
    let base = scan_base(img, film);
    measure(img, film, &base, Axis::Keep(film.fog(), film.axis()), false)
}

fn place_with(img: &Rgb32f, film: &Film, axis: Axis) -> Option<Print> {
    measure(img, film, &scan_base(img, film), axis, false).print
}

/// `film`'s base, with the lamp and clip levels of this scan.
fn scan_base(img: &Rgb32f, film: &Film) -> Base {
    let scan = frame::base_of(&linear(img, film.capture));
    Base { rgb: film.base(), lamp_cut: scan.and_then(|b| b.lamp_cut), clip: scan.map_or(f64::INFINITY, |b| b.clip) }
}

fn fresh(film: &Film) -> Film {
    let mut film = film.clone();
    film.set_gamma([1.0; 3]);
    film.clear_picks();
    film
}

fn refused(film: &Film, why: &str) -> Analysis {
    let mut film = film.clone();
    film.enabled = false;
    Analysis { film, stats: FrameStats::default(), frame_error: Some(why.to_string()) }
}

fn finish(mut work: Film, stats: FrameStats, previous: &Film) -> Analysis {
    match stats.print {
        Some(print) => {
            work.enabled = true;
            print.write_to(&mut work);
            Analysis { film: work, stats, frame_error: None }
        }
        None => {
            let mut film = previous.clone();
            film.enabled = false;
            Analysis { film, stats, frame_error: Some(NOTHING_TO_MEASURE.to_string()) }
        }
    }
}

/// Measure the picture inside `film`'s frame, less [`EDGE_TRIM`] and the user's margin.
/// Clipped lamp is left out; a wrong Dmin must not delete the picture.
fn measure(img: &Rgb32f, film: &Film, base: &Base, axis: Axis, bordered: bool) -> FrameStats {
    let margin = EDGE_TRIM + if film.inset.is_finite() { (film.inset / 100.0).clamp(0.0, 0.4) } else { 0.0 };
    let region = inset_frame(&film.frame, margin);
    let px = picture_pixels(img, &region, film.capture, base);
    let d = solve::densities(&px, film);
    let print = solve::solve(&d, axis, film.stock.mono());
    FrameStats { base: film.base(), rebate_level: y2020(film.base()), mask_measured: bordered, print, bins: density_bins(&d) }
}

fn picture_pixels(img: &Rgb32f, frame: &FilmFrame, capture: FilmCapture, base: &Base) -> Vec<[f32; 3]> {
    let (w, h) = (img.width, img.height);
    if w == 0 || h == 0 {
        return Vec::new();
    }
    let (x0, y0, x1, y1) = bounds(frame);
    let span = |a: f64, b: f64, n: usize| ((a.clamp(0.0, 1.0) * n as f64) as usize, ((b.clamp(0.0, 1.0) * n as f64).ceil() as usize).min(n));
    let ((xa, xb), (ya, yb)) = (span(x0, x1, w), span(y0, y1, h));
    let area = xb.saturating_sub(xa).saturating_mul(yb.saturating_sub(ya));
    let step = ((area as f64 / MAX_SAMPLES as f64).sqrt().ceil() as usize).max(1);
    let gamma = capture.gamma() as f32;
    let mut out = Vec::with_capacity(area / (step * step) + 1);
    for y in (ya..yb).step_by(step) {
        for x in (xa..xb).step_by(step) {
            let Some(p) = img.data.get(y.saturating_mul(w).saturating_add(x)) else { continue };
            let inside = inside_frame(frame, (x as f64 + 0.5) / w as f64, (y as f64 + 0.5) / h as f64);
            if inside && !base.is_clip(p.map(|c| kernel::capture_channel(c, gamma))) {
                out.push(*p);
            }
        }
    }
    out
}

fn bounds(frame: &FilmFrame) -> (f64, f64, f64, f64) {
    frame
        .corners
        .iter()
        .fold((f64::MAX, f64::MAX, f64::MIN, f64::MIN), |(x0, y0, x1, y1), c| (x0.min(c[0]), y0.min(c[1]), x1.max(c[0]), y1.max(c[1])))
}

fn density_bins(d: &[[f64; 3]]) -> Vec<f32> {
    const BINS: usize = 32;
    let mut bins = vec![0.0f32; BINS];
    for p in d {
        let i = ((y2020(*p).clamp(0.0, 2.5) / 2.5) * (BINS - 1) as f64).round() as usize;
        if let Some(slot) = bins.get_mut(i) {
            *slot += 1.0;
        }
    }
    let max = bins.iter().copied().fold(0.0f32, f32::max);
    if max > 0.0 {
        bins.iter_mut().for_each(|b| *b /= max);
    }
    bins
}

/// The gray axis through the stored neutral picks, or `None` without any. Picks that span at
/// least 0.05 density of green set offsets and slopes (least squares); otherwise the slopes stay
/// and the offsets go through the picks' mean.
pub fn axis_from_picks(film: &Film) -> Option<([f64; 3], [f64; 3])> {
    let d: Vec<[f64; 3]> = film.picks().into_iter().map(|t| linear_density(t, film)).collect();
    let n = d.len() as f64;
    if d.is_empty() {
        return None;
    }
    let mean = |c: usize| d.iter().map(|p| p[c]).sum::<f64>() / n;
    let mg = mean(1);
    let sgg: f64 = d.iter().map(|p| (p[1] - mg).powi(2)).sum();
    let spread = d.iter().map(|p| p[1]).fold(f64::MIN, f64::max) - d.iter().map(|p| p[1]).fold(f64::MAX, f64::min);
    let mut axis = film.axis().map(|v| if v.is_finite() && v > 0.0 { v } else { 1.0 });
    let mut fog = [0.0; 3];
    for c in [0usize, 2] {
        if spread >= 0.05 && sgg > 1e-9 {
            let sgc: f64 = d.iter().map(|p| (p[1] - mg) * (p[c] - mean(c))).sum();
            axis[c] = (sgc / sgg).clamp(0.4, 2.5);
        }
        fog[c] = mean(c) - axis[c] * mg;
    }
    axis[1] = 1.0;
    Some((fog, axis))
}

/// Dye-corrected density of one transmission that is already linear.
pub fn linear_density(t: [f64; 3], film: &Film) -> [f64; 3] {
    let mut linear = film.clone();
    linear.capture = FilmCapture::Linear;
    solve::densities(&[t.map(|v| v as f32)], &linear).first().copied().unwrap_or([0.0; 3])
}

/// Camera matrix that developed this source, and its inverse.
///
/// Density belongs in white-balanced camera RGB. The stored image is that RGB multiplied by the
/// camera matrix. Measuring the mask after the matrix mixes the dyes; measuring before it, then
/// putting the positive back through the same matrix, keeps the working space and the gray axis
/// in agreement. `None` for a JPEG, a slide scan, or a raw with no matrix of its own.
#[derive(Clone, Copy, Debug)]
pub struct ScannerSpace {
    pub to_camera: [[f32; 3]; 3],
    pub to_working: [[f32; 3]; 3],
}

pub fn scanner_space(info: &SourceInfo) -> Option<ScannerSpace> {
    let cc = info.camera_color.as_deref().filter(|_| info.raw && !info.relative_wb)?;
    let t = lightcraft_raw::color::camera_transform_of(&cc.tags, cc.developed_for);
    if t.matrix_is_fallback {
        return None;
    }
    let inverse = t.matrix.inverse()?;
    let to_camera = inverse.to_f32();
    let to_working = t.matrix.to_f32();
    let sane = |m: [[f32; 3]; 3]| m.iter().flatten().all(|v| v.is_finite() && v.abs() < 1.0e3);
    if !sane(to_camera) || !sane(to_working) {
        return None;
    }
    Some(ScannerSpace { to_camera, to_working })
}

/// `img = M · img`. Negatives are kept: a working-space blue below zero is how the orange mask
/// survives the colour matrix, and clamping it before the inverse prints blue.
pub fn apply_matrix(img: &mut Rgb32f, m: &[[f32; 3]; 3]) {
    apply_matrix_to(img, m, false);
}

/// [`apply_matrix`], then clamp at 0. The positive, after it has been carried back to Rec.2020.
pub fn apply_matrix_clamped(img: &mut Rgb32f, m: &[[f32; 3]; 3]) {
    apply_matrix_to(img, m, true);
}

fn apply_matrix_to(img: &mut Rgb32f, m: &[[f32; 3]; 3], clamp: bool) {
    let w = img.width;
    crate::for_rows(&mut img.data, w, |_, row| {
        for p in row.iter_mut() {
            *p = kernel::mat(m, *p).map(|v| match (v.is_finite(), clamp) {
                (false, _) => 0.0,
                (true, true) => v.max(0.0),
                (true, false) => v,
            });
        }
    });
}

/// Convert `img` in place. A disabled stage leaves the pixels alone, so as-shot white balance
/// still runs after this on an ordinary photo.
pub fn apply(img: &mut Rgb32f, film: &Film) {
    if !film.enabled {
        return;
    }
    let k = FilmKernel::from_film(film);
    let w = img.width;
    crate::for_rows(&mut img.data, w, |_, row| {
        for p in row.iter_mut() {
            *p = k.apply(*p);
        }
    });
}

/// The pixels white balance reads: Negative Craft, then back in the working space.
///
/// Density is measured in camera RGB when the file has its own matrix, and the positive is
/// returned to Rec.2020 before white balance. A disabled stage leaves `img` unchanged.
pub fn apply_before_white_balance(img: &mut Rgb32f, info: &SourceInfo, film: &Film) {
    if !film.enabled {
        return;
    }
    // Already white-balanced camera RGB. Density is measured here; the matrix only carries the
    // positive back, and the clamp is that one step.
    if info.transmission {
        apply(img, film);
        if let Some(m) = info.to_working.as_ref() {
            apply_matrix_clamped(img, m);
        }
        return;
    }
    let scanner = scanner_space(info);
    if let Some(space) = &scanner {
        apply_matrix(img, &space.to_camera);
    }
    apply(img, film);
    if let Some(space) = &scanner {
        apply_matrix_clamped(img, &space.to_working);
    }
}

/// The image an automatic adjustment reads: the print, in the working space, before white
/// balance. Auto tone, white balance, and the black-and-white mix use it, so they do not solve
/// the orange mask. Disabled film returns `img`.
pub fn for_auto<'a>(img: &'a Rgb32f, info: &SourceInfo, film: &Film) -> Cow<'a, Rgb32f> {
    if !film.enabled {
        return Cow::Borrowed(img);
    }
    let mut out = img.clone();
    apply_before_white_balance(&mut out, info, film);
    Cow::Owned(out)
}

/// Render-cache key. Disabled film hashes as 0, because it does not change pixels.
pub fn cache_key(f: &Film) -> u64 {
    if !f.enabled {
        return 0;
    }
    FilmKernel::from_film(f).words().iter().fold(0xcbf2_9ce4_8422_2325u64, |h, w| (h ^ u64::from(*w)).wrapping_mul(0x0100_0000_01b3))
}

/// The scan encoding removed: linear transmission. Gamma 1.8 and 2.2 raise each channel to
/// that power, so a measurement and the print see one transmission.
pub fn transmission(sample: [f64; 3], capture: FilmCapture) -> [f64; 3] {
    let g = capture.gamma() as f32;
    sample.map(|v| f64::from(kernel::capture_channel(v as f32, g)))
}

fn linear(img: &Rgb32f, capture: FilmCapture) -> Cow<'_, Rgb32f> {
    let g = capture.gamma() as f32;
    if g <= 1.001 {
        return Cow::Borrowed(img);
    }
    Cow::Owned(Rgb32f::from_fn(img.width, img.height, |x, y| img.get(x, y).map(|c| kernel::capture_channel(c, g))))
}

/// Median of a 5×5 neighborhood. `x` and `y` are normalized to the oriented image.
pub fn sample(img: &Rgb32f, x: f64, y: f64) -> Option<[f64; 3]> {
    median_around(img, x, y, 2)
}

/// Blank film at a click, as linear transmission: the median of a small patch, so grain does
/// not set Dmin. The click is the probe; Convert's guess is not substituted for it.
pub fn sample_border(img: &Rgb32f, x: f64, y: f64, capture: FilmCapture) -> Option<[f64; 3]> {
    median_around(&linear(img, capture), x, y, 3)
}

/// Why a border click cannot be the base: a white lamp has no mask colour, and the holder is
/// not film. On a colour negative the click has to be orange; a pale or neutral spot is the
/// fringe, and using it as Dmin prints blue.
pub fn border_error(sample: [f64; 3], stock: FilmStock) -> Option<&'static str> {
    if !sample.iter().all(|c| c.is_finite()) {
        return Some(BORDER_BLOWN);
    }
    let max = sample[0].max(sample[1]).max(sample[2]);
    let min = sample[0].min(sample[1]).min(sample[2]);
    if max >= 0.97 && min >= 0.85 {
        return Some(BORDER_BLOWN);
    }
    if y2020(sample) < 0.02 {
        return Some(BORDER_DARK);
    }
    if stock.negative() && !stock.mono() && !base::is_orange(sample.map(|v| v as f32)) {
        return Some(BORDER_PLAIN);
    }
    None
}

fn median_around(img: &Rgb32f, x: f64, y: f64, r: isize) -> Option<[f64; 3]> {
    if img.width == 0 || img.height == 0 || !x.is_finite() || !y.is_finite() {
        return None;
    }
    let cx = (x.clamp(0.0, 1.0) * img.width as f64) as isize;
    let cy = (y.clamp(0.0, 1.0) * img.height as f64) as isize;
    let mut cols: [Vec<f64>; 3] = Default::default();
    for dy in -r..=r {
        for dx in -r..=r {
            let p = img.get_clamped(cx + dx, cy + dy);
            for (col, v) in cols.iter_mut().zip(p) {
                col.push(f64::from(v));
            }
        }
    }
    let [mut r, mut g, mut b] = cols;
    Some([percentile(&mut r, 0.5)?, percentile(&mut g, 0.5)?, percentile(&mut b, 0.5)?])
}

/// The cast Convert removed, at a mid green density, and the printed midtone's a* b*.
/// Positive green is red density short of green (the print was green). Positive blue is extra
/// blue density.
pub fn cast_readout(f: &Film) -> String {
    let d = 0.45;
    let green = -(f.fog_r + (f.axis_r - 1.0) * d);
    let blue = f.fog_b + (f.axis_b - 1.0) * d;
    format!("cast green {green:+.2}, blue {blue:+.2} · a* {:+.1} b* {:+.1}", f.mid_a, f.mid_b)
}

/// The pixels inside `crop`, in crop coordinates. An uncropped scan is returned unchanged.
/// An angled crop is sampled nearest-pixel, so a pixel outside the crop is not blended in.
pub fn within_crop(img: &Rgb32f, crop: CropGeometry) -> Rgb32f {
    if crop.is_identity() || img.width == 0 || img.height == 0 {
        return img.clone();
    }
    let (w, h) = (img.width as f64, img.height as f64);
    let finite = [crop.rect.x0, crop.rect.y0, crop.rect.x1, crop.rect.y1, crop.angle].iter().all(|v| v.is_finite());
    if !finite {
        return img.clone();
    }
    let r = crop.rect_px(w, h);
    let (rw, rh) = (r.width(), r.height());
    if !rw.is_finite() || !rh.is_finite() || rw < 1.0 || rh < 1.0 {
        return img.clone();
    }
    let out_w = (rw.round() as usize).clamp(1, img.width);
    let out_h = (rh.round() as usize).clamp(1, img.height);
    let map = crop.output_to_source(w, h, out_w as f64, out_h as f64);
    let mut out = Rgb32f::new(out_w, out_h);
    for y in 0..out_h {
        for x in 0..out_w {
            let p = map.apply(Point::new(x as f64 + 0.5, y as f64 + 0.5));
            if !(p.x.is_finite() && p.y.is_finite() && p.x >= 0.0 && p.y >= 0.0 && p.x < w && p.y < h) {
                continue;
            }
            let (sx, sy) = (p.x as usize, p.y as usize);
            let Some(px) = img.data.get(sy.saturating_mul(img.width).saturating_add(sx)) else { continue };
            if let Some(slot) = out.data.get_mut(y.saturating_mul(out_w).saturating_add(x)) {
                *slot = *px;
            }
        }
    }
    out
}

/// True when `x`, `y` (normalized) lie inside the frame.
pub fn inside_frame(frame: &FilmFrame, x: f64, y: f64) -> bool {
    let c = &frame.corners;
    let mut inside = false;
    for i in 0..4 {
        let (p, q) = (c[i], c[(i + 3) % 4]);
        if (p[1] > y) != (q[1] > y) {
            let den = q[1] - p[1];
            if den.abs() > 1e-12 && x < (q[0] - p[0]) * (y - p[1]) / den + p[0] {
                inside = !inside;
            }
        }
    }
    inside
}

/// Pull each corner toward the frame's centre so a fraction `margin` of the frame is dropped on
/// every side.
pub fn inset_frame(frame: &FilmFrame, margin: f64) -> FilmFrame {
    if !margin.is_finite() || margin <= 1e-6 {
        return *frame;
    }
    let scale = 1.0 - 2.0 * margin.clamp(0.0, 0.45);
    let cx = frame.corners.iter().map(|p| p[0]).sum::<f64>() / 4.0;
    let cy = frame.corners.iter().map(|p| p[1]).sum::<f64>() / 4.0;
    FilmFrame { corners: frame.corners.map(|p| [cx + (p[0] - cx) * scale, cy + (p[1] - cy) * scale]) }
}

/// The picture as a crop, a hair inside the frame so the blurred edge line stays out.
/// `w` and `h` are the oriented image in pixels. `None` when the frame is the whole scan or the
/// corners do not make a rectangle.
pub fn crop_for_frame(frame: &FilmFrame, w: f64, h: f64) -> Option<CropGeometry> {
    if !(w.is_finite() && h.is_finite()) || w < 1.0 || h < 1.0 || frame.is_full() {
        return None;
    }
    let frame = inset_frame(frame, 0.01);
    let px: Vec<Point> = frame.corners.iter().map(|c| Point::new(c[0] * w, c[1] * h)).filter(|p| p.x.is_finite() && p.y.is_finite()).collect();
    let (Some(p0), Some(p1)) = (px.first(), px.get(1)) else { return None };
    let (dx, dy) = (p1.x - p0.x, p1.y - p0.y);
    if dx.hypot(dy) < 1.0 {
        return None;
    }
    // The edge's clockwise tilt, folded so the crop angle stays inside −45°..45°.
    let mut angle = (-dy.atan2(dx).to_degrees()).rem_euclid(90.0);
    if angle > 45.0 {
        angle -= 90.0;
    }
    if angle.abs() < 0.25 {
        angle = 0.0;
    }
    let into = Affine::rotate_about(angle.to_radians(), Point::new(w * 0.5, h * 0.5));
    let (mut minx, mut miny, mut maxx, mut maxy) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for q in px.iter().map(|p| into.apply(*p)) {
        minx = minx.min(q.x);
        miny = miny.min(q.y);
        maxx = maxx.max(q.x);
        maxy = maxy.max(q.y);
    }
    if maxx - minx < 1.0 || maxy - miny < 1.0 {
        return None;
    }
    Some(CropGeometry { rect: Rect::new(minx / w, miny / h, maxx / w, maxy / h), angle }.constrained(w, h))
}

pub(crate) fn y2020(c: [f64; 3]) -> f64 {
    0.2627 * c[0] + 0.6780 * c[1] + 0.0593 * c[2]
}

/// The `p` quantile of `v` (sorted in place), nearest rank. `None` for an empty slice.
pub(crate) fn percentile(v: &mut [f64], p: f64) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_unstable_by(f64::total_cmp);
    let i = ((v.len() - 1) as f64 * p.clamp(0.0, 1.0)).round() as usize;
    v.get(i).copied()
}

/// A smoothed histogram of `ln(values)` between the `from` quantile and the maximum.
pub(crate) struct LogHistogram {
    pub counts: Vec<f64>,
    lo: f64,
    step: f64,
}

impl LogHistogram {
    pub fn edge(&self, i: usize) -> f64 {
        self.lo + i as f64 * self.step
    }

    pub fn center(&self, i: usize) -> f64 {
        self.lo + (i as f64 + 0.5) * self.step
    }

    pub fn span(&self) -> f64 {
        self.step * self.counts.len() as f64
    }
}

pub(crate) fn log_histogram(values: &[f64], from: f64, bins: usize, kernel: &[f64]) -> Option<LogHistogram> {
    let mut logs: Vec<f64> = values.iter().filter(|v| v.is_finite()).map(|v| v.max(1e-5).ln()).collect();
    let lo = percentile(&mut logs, from)?;
    let hi = logs.last().copied()?;
    let step = ((hi - lo) / bins as f64).max(1e-6);
    let mut raw = vec![0.0f64; bins];
    for l in logs.iter().filter(|l| **l >= lo) {
        let i = (((l - lo) / step) as usize).min(bins - 1);
        if let Some(slot) = raw.get_mut(i) {
            *slot += 1.0;
        }
    }
    let total: f64 = kernel.iter().sum();
    let half = kernel.len() / 2;
    let counts = (0..bins)
        .map(|i| {
            kernel.iter().enumerate().fold(0.0, |acc, (k, w)| {
                let j = (i + k).checked_sub(half).and_then(|j| raw.get(j));
                acc + w * j.copied().unwrap_or(0.0)
            }) / total.max(1e-12)
        })
        .collect();
    Some(LogHistogram { counts, lo, step })
}
