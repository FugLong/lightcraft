//! Negative Craft: convert a film scan in scene-linear transmission, before white balance.
//!
//! Clear film (the mask) is the per-channel base. Density above that base is
//! `D = -log10(transmission / base)`. On a raw file with its own colour matrix, that density is
//! measured in white-balanced camera RGB; the matrix is only how the finished positive is stored
//! in Rec.2020 ([`scanner_space`]). A JPEG has already left that space, so the same curve runs on
//! the working image and is only as true as the file.
//!
//! A color negative's straight line is `D ≈ γ log10(exposure)` with γ near [`FILM_GAMMA`]. The
//! positive is that exposure, `10^(D/γ) − 1`, normalised so a chosen density is scene light 1.
//! Convert places the middle of the picture on the pipeline's middle grey (0.18) and sets contrast
//! so the body of that picture, not a bright speck, fills the print. Exposure is the scale.
//! Contrast is the slope, and it pivots on that middle: moving it does not slide the whole frame
//! brighter or darker. A picture that never reaches clear film has the empty density under it
//! lifted, so the shadows can reach black. A negative too steep for the softest grade keeps that
//! grade, and a shoulder rolls the bright end back to 1. Contrast is not a power on the
//! normalised density.
//!
//! The orange mask cancels dye side-absorption for colour paper, not for a camera. What a wide
//! filter still sees is removed with Hanson's integral-to-analytical inverse ([`printing_density`];
//! Hanson, J. Opt. Soc. Am. 40, 1950). The gray axis then prints a gray subject gray: a robust line
//! `D_c = fog_c + axis_c · D_green`, kept only when the cast has the same sign from the shadows to
//! the highlights. `map.wgsl`'s `film_k` is the same function; keep the two in step.
//!
//! No plugin binary, scanner LUT, or manufacturer's stock profile is used here. The dye numbers are
//! a typical residual of a wide filter against a narrow one, not a measured stock.

use lightcraft_color::luminance_2020;
use lightcraft_develop::{Film, FilmCapture, FilmFrame, FilmIntent, FilmScan, FilmStock};
use lightcraft_geom::{Affine, CropGeometry, Point, Rect};
use lightcraft_raster::Rgb32f;

use crate::SourceInfo;

const EPS: f32 = 1e-6;
const LN10: f32 = std::f32::consts::LN_10;
/// Straight-line gamma of a colour negative (density per decade of exposure).
pub const FILM_GAMMA: f64 = 0.60;
/// Print luminance the 90th percentile of the picture should reach. The extreme bright end may
/// pass 1; the shoulder brings that end back onto the paper.
const BODY_PRINT: f64 = 0.82;
/// Density left under the picture's dark end when that end sits far above clear film.
const SHADOW_TOE: f64 = 0.08;
/// The dark end has to be at least this fraction of the bright end before the empty density
/// under the picture is lifted. A frame that already reaches toward clear film is left alone.
const SHADOW_GAP: f64 = 0.45;
/// Scene-linear ceiling. Density above the frame's white stays, up to this, for the tone curve.
const HEADROOM: f32 = 64.0;

/// Packed parameters of one pixel of [`apply`]. `words` is what `film_k` reads after the pixel count.
#[derive(Clone, Copy, Debug)]
pub struct FilmKernel {
    pub base: [f32; 3],
    pub gamma: [f32; 3],
    pub presat: f32,
    pub slope: f32,
    pub power: f32,
    pub offset: f32,
    pub shoulder: f32,
    pub fog: [f32; 3],
    pub white: [f32; 3],
    pub axis: [f32; 3],
    /// `(cyan in green, magenta in blue, cyan in blue)`. Zero skips the dye correction.
    pub dye: [f32; 3],
    pub aim: [f32; 3],
    pub chroma: f32,
    pub balance: [f32; 3],
    pub shadow: [f32; 3],
    pub mid: [f32; 3],
    pub high: [f32; 3],
    pub negative: bool,
    pub mono: bool,
    pub capture: f32,
    /// Straightened density removed under the shadows. Zero keeps clear film as the only black.
    pub lift: f32,
    /// Straightened density the contrast slider holds still, and its scene light at the grade
    /// exposure was solved for. Zero skips the hold, so a slide and an old print are unchanged.
    pub pivot: f32,
    pub anchor: f32,
}

impl FilmKernel {
    pub fn from_film(f: &Film) -> Self {
        let (fog, white) = endpoints(f);
        let (aim, chroma) = f.print.aim();
        Self {
            base: f.base().map(|v| v as f32),
            gamma: f.gamma().map(|v| v as f32),
            presat: if f.stock.mono() { 1.0 } else { (1.0 + f.presat / 100.0).clamp(0.0, 3.0) as f32 },
            slope: (2.0f64).powf(f.exposure).clamp(1e-3, 64.0) as f32,
            // A negative stores its film gamma here (γ = FILM_GAMMA / grade). A slide stores grade
            // and raises the positive to that power. `film_k` branches on the negative flag.
            power: if f.stock.negative() { (FILM_GAMMA / f.grade.clamp(0.2, 6.0)) as f32 } else { f.grade.clamp(0.05, 6.0) as f32 },
            offset: f.black.clamp(-0.5, 1.0) as f32,
            shoulder: f.shoulder.max(0.0) as f32,
            fog,
            white,
            axis: f.axis().map(|v| if v.is_finite() && v > 0.05 { v as f32 } else { 1.0 }),
            dye: if f.stock.mono() { [0.0; 3] } else { f.scan.dye().map(|v| v as f32) },
            // Black-and-white stays one channel. Print and temperature are hidden for these stocks,
            // so a Portrait aim or a Warm look left over from a color frame must not tint the print.
            // The settings themselves stay, for when the stock changes back.
            aim: if f.stock.mono() { [1.0; 3] } else { aim.map(|v| v as f32) },
            chroma: if f.stock.mono() { 1.0 } else { chroma as f32 },
            balance: if f.stock.mono() { [1.0; 3] } else { band_gain(f.temp, f.tint) },
            shadow: if f.stock.mono() { [1.0; 3] } else { band_gain(f.shadow_temp, f.shadow_tint) },
            mid: if f.stock.mono() { [1.0; 3] } else { band_gain(f.mid_temp, f.mid_tint) },
            high: if f.stock.mono() { [1.0; 3] } else { band_gain(f.high_temp, f.high_tint) },
            negative: f.stock.negative(),
            mono: f.stock.mono(),
            capture: f.capture.gamma() as f32,
            // `d_lo` is the shared black when Auto has not set a white. Once it has, `d_lo` is
            // the density lifted off the shadows.
            lift: if f.shared_endpoints() || !f.d_lo.is_finite() { 0.0 } else { f.d_lo.max(0.0) as f32 },
            pivot: if f.pivot.is_finite() { f.pivot.max(0.0) as f32 } else { 0.0 },
            anchor: if f.anchor.is_finite() { f.anchor.max(0.0) as f32 } else { 0.0 },
        }
    }

    /// `P[1..]` of `film_k` (`P[0]` is the pixel count, added by the dispatcher).
    pub fn words(&self) -> Vec<u32> {
        let mut w = Vec::with_capacity(46);
        let push = |w: &mut Vec<u32>, v: f32| w.push(v.to_bits());
        for v in self.base {
            push(&mut w, v);
        }
        for v in self.gamma {
            push(&mut w, v);
        }
        push(&mut w, self.presat);
        push(&mut w, self.slope);
        push(&mut w, self.power);
        push(&mut w, self.offset);
        push(&mut w, self.shoulder);
        for v in self.balance {
            push(&mut w, v);
        }
        for v in self.shadow {
            push(&mut w, v);
        }
        for v in self.mid {
            push(&mut w, v);
        }
        for v in self.high {
            push(&mut w, v);
        }
        let mut flags = 0u32;
        if self.negative {
            flags |= 1;
        }
        if self.mono {
            flags |= 2;
        }
        w.push(flags);
        push(&mut w, self.capture);
        // Shared endpoints, kept so a kernel that has not been rebalanced still has a span.
        // `film_k` prints with fog/white below.
        push(&mut w, self.fog[1]);
        push(&mut w, self.white[1]);
        for v in self.fog {
            push(&mut w, v);
        }
        for v in self.white {
            push(&mut w, v);
        }
        for v in self.dye {
            push(&mut w, v);
        }
        for v in self.aim {
            push(&mut w, v);
        }
        push(&mut w, self.chroma);
        for v in self.axis {
            push(&mut w, v);
        }
        push(&mut w, self.lift);
        push(&mut w, self.pivot);
        push(&mut w, self.anchor);
        w
    }

    pub fn apply(&self, mut t: [f32; 3]) -> [f32; 3] {
        t = t.map(|c| capture_channel(c, self.capture));
        if (self.presat - 1.0).abs() > 1e-6 {
            let y = (t[0] + t[1] + t[2]) / 3.0;
            for c in &mut t {
                *c = (y + (*c - y) * self.presat).max(0.0);
            }
        }
        let (base, gamma) = if self.mono {
            let y = luminance_2020(t);
            t = [y, y, y];
            let b = luminance_2020(self.base).max(1e-4);
            ([b, b, b], [self.gamma[1].max(0.05), self.gamma[1].max(0.05), self.gamma[1].max(0.05)])
        } else {
            (self.base.map(|v| v.max(1e-4)), self.gamma.map(|v| v.max(0.05)))
        };
        let mut scene = [0.0f32; 3];
        if self.negative {
            let mut d = [0.0f32; 3];
            for i in 0..3 {
                let n = (t[i] / base[i]).max(EPS);
                d[i] = ((-n.min(1.0e6).ln() / LN10).max(0.0) * gamma[i]).min(4.0);
            }
            if !self.mono {
                d = printing_density(d, self.dye);
            }
            // One curve white (the brightest channel). A near-neutral bright end is gained per
            // channel so it prints at 1, and that gain fades in toward the white. Equal whites,
            // a saturated bright end, and black-and-white leave the gain at 1. Density past the
            // white stays above 1.
            let shared = self.white.into_iter().fold(0.0f32, f32::max).max(0.05);
            let film_gamma = self.power.max(0.05);
            let mut straight = [0.0f32; 3];
            for i in 0..3 {
                let axis = self.axis[i].max(0.05);
                straight[i] = ((d[i] - self.fog[i]).max(0.0)) / axis;
                if self.lift > 1e-4 {
                    straight[i] = (straight[i] - self.lift).max(0.0);
                }
            }
            let straight_y = 0.2627 * straight[0] + 0.6780 * straight[1] + 0.0593 * straight[2];
            let t = if self.mono { 0.0 } else { smooth(shared * 0.62, shared * 0.92, straight_y) };
            for i in 0..3 {
                let mut v = light_from_density(straight[i], shared, film_gamma);
                if t > 0.0 {
                    let channel = self.white[i].clamp(shared - 0.45, shared).max(0.05);
                    let at = light_from_density(channel, shared, film_gamma).max(1e-4);
                    v *= 1.0 + (1.0 / at - 1.0) * t;
                    v = v.clamp(0.0, HEADROOM);
                }
                scene[i] = v;
            }
            for i in 0..3 {
                scene[i] *= self.aim[i].max(0.0);
            }
            if (self.chroma - 1.0).abs() > 1e-4 {
                let y = (scene[0] + scene[1] + scene[2]) / 3.0;
                for c in &mut scene {
                    *c = (y + (*c - y) * self.chroma).max(0.0);
                }
            }
            // Hold the middle. Exposure was solved at one grade; another grade must not slide
            // that density off middle grey. The same factor on every channel leaves the hue.
            if self.pivot > 1e-4 && self.anchor > 1e-5 {
                let now = light_from_density(self.pivot, shared, film_gamma).max(1e-5);
                let hold = (self.anchor / now).clamp(0.02, 64.0);
                for c in &mut scene {
                    *c *= hold;
                }
            }
        } else {
            for i in 0..3 {
                let n = (t[i] / base[i]).max(EPS);
                scene[i] = n.powf(gamma[i]);
            }
        }
        for i in 0..3 {
            let mut v = (scene[i] * self.slope + self.offset).max(0.0);
            // Slide contrast. A negative's contrast is the film gamma already applied.
            if !self.negative && (self.power - 1.0).abs() > 1e-4 {
                v = v.powf(self.power);
            }
            scene[i] = v;
        }
        let y = luminance_2020(scene);
        let sh = 1.0 - smooth(0.02, 0.35, y);
        let hi = smooth(0.55, 0.95, y);
        let mid = (1.0 - sh - hi).max(0.0);
        for i in 0..3 {
            let band = sh * self.shadow[i] + mid * self.mid[i] + hi * self.high[i];
            let mut v = scene[i] * (self.balance[i] * band).max(0.0);
            if self.shoulder > 1e-4 && v > self.shoulder {
                let e = v - self.shoulder;
                v = self.shoulder + e / (1.0 + e / self.shoulder);
            }
            scene[i] = v;
        }
        scene
    }
}

/// Scene light of one straightened density. `white` maps to 1. Density above it is left above 1.
fn light_from_density(straight: f32, white: f32, gamma: f32) -> f32 {
    let g = gamma.max(0.05);
    let w = white.max(0.05);
    let scale = 10f32.powf((w / g).min(8.0)) - 1.0;
    if scale < 1e-4 {
        return 0.0;
    }
    let e = (straight.max(0.0) / g).min(8.0);
    ((10f32.powf(e) - 1.0) / scale).clamp(0.0, HEADROOM)
}

fn endpoints(f: &Film) -> ([f32; 3], [f32; 3]) {
    if f.shared_endpoints() {
        let lo = f.d_lo as f32;
        let hi = (f.d_hi.max(f.d_lo + 0.05)) as f32;
        ([lo, lo, lo], [hi, hi, hi])
    } else {
        let fog = f.fog().map(|v| v as f32);
        let src = f.white();
        let white = [0, 1, 2].map(|i| (src[i] as f32).max(fog[i] + 0.05));
        (fog, white)
    }
}

/// Integral density to analytical (printing) density. `dye` is
/// `(cyan in green, magenta in blue, cyan in blue)`.
///
/// Hanson, with yellow's side absorption and magenta's red absorption taken as zero:
/// `C = Dr`, `M = Dg − cg·C`, `Y = Db − cb·C − mb·M`. Zero dye leaves the integral density
/// alone. An equal-density neutral is not preserved: the gray axis puts that back, and the
/// correction is what saturated colours needed. Zero stays zero, so the mask still prints black.
fn printing_density(d: [f32; 3], dye: [f32; 3]) -> [f32; 3] {
    let (cg, mb, cb) = (dye[0], dye[1], dye[2]);
    if cg == 0.0 && mb == 0.0 && cb == 0.0 {
        return d;
    }
    let c = d[0];
    let m = (d[1] - cg * c).max(0.0);
    let y = (d[2] - cb * c - mb * m).max(0.0);
    [c, m, y]
}

/// ±0.40 density at the end of the slider (about 2.5×), so a hard frame can be finished.
/// Positive temperature adds red and removes blue. Positive tint removes green.
fn band_gain(temp: f64, tint: f64) -> [f32; 3] {
    let t = (temp / 100.0).clamp(-1.0, 1.0) as f32;
    let n = (tint / 100.0).clamp(-1.0, 1.0) as f32;
    let warm = 10f32.powf(0.40 * t);
    let green = 10f32.powf(-0.40 * n);
    [warm.clamp(0.15, 6.0), green.clamp(0.15, 6.0), (1.0 / warm).clamp(0.15, 6.0)]
}

fn smooth(e0: f32, e1: f32, x: f32) -> f32 {
    let span = e1 - e0;
    if span.abs() < 1e-6 {
        return if x >= e1 { 1.0 } else { 0.0 };
    }
    let t = ((x - e0) / span).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Render-cache key. Disabled film hashes as 0, because it does not change pixels.
pub fn cache_key(f: &Film) -> u64 {
    if !f.enabled {
        return 0;
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for w in FilmKernel::from_film(f).words() {
        h ^= w as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
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

/// `img = M · img`, negatives clamped to 0. The same multiply as `wb_k`.
pub fn apply_matrix(img: &mut Rgb32f, m: &[[f32; 3]; 3]) {
    let w = img.width;
    crate::for_rows(&mut img.data, w, |_, row| {
        for p in row.iter_mut() {
            let c = *p;
            *p = [
                m[0][0] * c[0] + m[0][1] * c[1] + m[0][2] * c[2],
                m[1][0] * c[0] + m[1][1] * c[1] + m[1][2] * c[2],
                m[2][0] * c[0] + m[2][1] * c[1] + m[2][2] * c[2],
            ]
            .map(|v| if v.is_finite() { v.max(0.0) } else { 0.0 });
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
/// Same order as the scene-linear stage. Density is measured in camera RGB when the file has
/// its own matrix, and the positive is returned to Rec.2020 before white balance. A disabled
/// stage leaves `img` unchanged.
pub fn apply_before_white_balance(img: &mut Rgb32f, info: &SourceInfo, film: &Film) {
    let scanner = scanner_space(info).filter(|_| film.enabled);
    if let Some(space) = &scanner {
        apply_matrix(img, &space.to_camera);
    }
    apply(img, film);
    if let Some(space) = &scanner {
        apply_matrix(img, &space.to_working);
    }
}

/// The image an automatic adjustment reads.
///
/// Disabled film returns `img`. Otherwise this is the print, in the working space, before white
/// balance: the same step as the render. Auto tone, white balance, and the black-and-white mix
/// use it, so they do not solve the orange mask.
pub fn for_auto<'a>(img: &'a Rgb32f, info: &SourceInfo, film: &Film) -> std::borrow::Cow<'a, Rgb32f> {
    if !film.enabled {
        return std::borrow::Cow::Borrowed(img);
    }
    let mut out = img.clone();
    apply_before_white_balance(&mut out, info, film);
    std::borrow::Cow::Owned(out)
}

/// The scan encoding, as [`FilmKernel::apply`] removes it. Linear is unchanged. Gamma 1.8 and 2.2
/// raise each stored channel to that power, so a measurement and the print see one transmission.
pub fn transmission(sample: [f64; 3], capture: FilmCapture) -> [f64; 3] {
    let g = capture.gamma() as f32;
    sample.map(|v| capture_channel(v as f32, g) as f64)
}

fn capture_channel(c: f32, gamma: f32) -> f32 {
    if !c.is_finite() || c < 0.0 {
        return 0.0;
    }
    if gamma > 1.001 { c.powf(gamma) } else { c }
}

fn linearize_capture(img: &Rgb32f, capture: FilmCapture) -> Option<Rgb32f> {
    let g = capture.gamma() as f32;
    if g <= 1.001 {
        return None;
    }
    Some(Rgb32f::from_fn(img.width, img.height, |x, y| img.get(x, y).map(|c| capture_channel(c, g))))
}

/// Density of a 5×5 neighborhood, after the scan encoding, the mask, and the dye correction.
pub fn density_at(img: &Rgb32f, x: f64, y: f64, film: &Film) -> [f64; 3] {
    let p = transmission(sample(img, x, y).unwrap_or([0.0; 3]), film.capture);
    density_rgb(p.map(|v| v as f32), &film.base(), &film.gamma(), !film.stock.mono(), film.scan)
}

/// Median of a 5×5 neighborhood. `x` and `y` are normalized to the oriented image.
///
/// Gray and neutral clicks want this local tone. A border click uses [`sample_border`].
pub fn sample(img: &Rgb32f, x: f64, y: f64) -> Option<[f64; 3]> {
    if img.width == 0 || img.height == 0 {
        return None;
    }
    let cx = (x.clamp(0.0, 1.0) * img.width as f64) as isize;
    let cy = (y.clamp(0.0, 1.0) * img.height as f64) as isize;
    let mut cols = [[0.0f64; 25]; 3];
    let mut n = 0usize;
    for dy in -2..=2 {
        for dx in -2..=2 {
            let pixel = img.get_clamped(cx + dx, cy + dy);
            for (channel, sample) in cols.iter_mut().zip(pixel) {
                if let Some(slot) = channel.get_mut(n) {
                    *slot = sample as f64;
                }
            }
            n = n.saturating_add(1);
        }
    }
    Some(cols.map(|mut col| percentile_f64(&mut col, 0.50).unwrap_or(0.0)))
}

/// Blank film of this scan. The click only matters when no rebate is there.
///
/// The flat band against the holder is the color. The bright ring is the fallback. A grain speck,
/// the holder, and a click a few pixels into the picture all return that one color.
pub fn sample_border(img: &Rgb32f, x: f64, y: f64) -> Option<[f64; 3]> {
    if let Some(color) = plateau_border(img).color {
        return Some(color);
    }
    // The bright ring, on the area average. A click does not flood the full scan.
    rebate_hit(&preview(img)).and_then(|hit| hit.color).or_else(|| sample(img, x, y))
}

/// Per-channel contrast that makes the picked neutrals neutral, green held at `green`.
pub fn gamma_from_picks(base: [f64; 3], picks: &[([f64; 3], f64)], green: f64) -> [f64; 3] {
    let green = green.clamp(0.05, 4.0);
    let mut acc = [0.0f64; 3];
    let mut w = [0.0f64; 3];
    for (pix, weight) in picks {
        let weight = if weight.is_finite() && *weight > 0.0 { *weight } else { continue };
        let mut n = [0.0f64; 3];
        for i in 0..3 {
            let b = base[i].max(1e-6);
            n[i] = (pix[i] / b).clamp(1e-6, 1.0e6);
        }
        let lng = n[1].ln();
        if lng.abs() < 1e-3 {
            continue;
        }
        for i in 0..3 {
            let lni = n[i].ln();
            if lni.abs() < 1e-3 {
                continue;
            }
            let g = (green * lng / lni).clamp(0.05, 4.0);
            if let (Some(a), Some(ww)) = (acc.get_mut(i), w.get_mut(i)) {
                *a += g * weight;
                *ww += weight;
            }
        }
    }
    let mut out = [green; 3];
    for i in 0..3 {
        if let (Some(a), Some(ww)) = (acc.get(i), w.get(i))
            && *ww > 0.0
        {
            out[i] = a / ww;
        }
    }
    out
}

/// What one frame measured, before a roll shares color.
#[derive(Clone, Debug)]
pub struct FrameStats {
    pub base: [f64; 3],
    pub gamma: [f64; 3],
    pub exposure: f64,
    pub rebate_level: f64,
    pub dominant: f64,
    pub bins: Vec<f32>,
    pub rebate_black: bool,
    /// The base came from blank film in this scan. A miss keeps the previous base and must not
    /// move the roll's shared mask color.
    pub mask_measured: bool,
    pub d_lo: f64,
    pub d_hi: f64,
    pub fog: [f64; 3],
    pub axis: [f64; 3],
    pub white: [f64; 3],
    pub mid_a: f64,
    pub mid_b: f64,
    pub grade: f64,
    /// Paper shoulder written by Convert. Zero when the bright end already prints at 1.
    pub shoulder: f64,
    /// Straightened density the contrast slider holds, and its scene light at [`Self::grade`].
    pub pivot: f64,
    pub anchor: f64,
}

#[derive(Clone, Debug)]
pub struct Analysis {
    pub film: Film,
    pub stats: FrameStats,
    /// Set when a frame could not be found. `film.frame` is the one that was already there.
    pub frame_error: Option<String>,
}

/// Measure `img` with `film`'s frame, stock, and intent, and turn the stage on.
pub fn analyze_image(img: &Rgb32f, film: &Film) -> Analysis {
    let mut film = film.clone();
    // A thin negative is orange inside the picture too, so the rebate flood swallows
    // the frame. The sprocket and clear-film gate still knows where the picture is.
    let frame_error = match detect_frame(img, film.stock).or_else(|| detect_gate(img, film.stock)) {
        Some(frame) => {
            film.frame = frame;
            None
        }
        None => {
            Some("No blank film found around this picture. Include the rebate in the scan, or click Border on film that is in the frame.".to_string())
        }
    };
    let stats = measure(img, &film);
    film.set_base(stats.base);
    film.set_gamma(stats.gamma);
    film.d_lo = stats.d_lo;
    film.d_hi = stats.d_hi;
    film.set_fog(stats.fog);
    film.set_axis(stats.axis);
    film.set_white(stats.white);
    film.mid_a = stats.mid_a;
    film.mid_b = stats.mid_b;
    film.grade = stats.grade;
    film.exposure = stats.exposure;
    film.pivot = stats.pivot;
    film.anchor = stats.anchor;
    if film.stock.negative() {
        film.shoulder = stats.shoulder;
    }
    film.enabled = true;
    Analysis { film, stats, frame_error }
}

/// Convert a raster the user already cropped. The crop is the picture: nothing outside it is
/// read, and a missing rebate is not an error. Blank film still inside the crop is the base.
/// With none there, the base starts neutral, so a mask measured from the discarded surround
/// cannot tint the print.
pub fn analyze_picture(img: &Rgb32f, film: &Film) -> Analysis {
    let mut film = film.clone();
    film.frame = FilmFrame::default();
    film.set_base([1.0, 1.0, 1.0]);
    let stats = measure(img, &film);
    film.set_base(stats.base);
    film.set_gamma(stats.gamma);
    film.d_lo = stats.d_lo;
    film.d_hi = stats.d_hi;
    film.set_fog(stats.fog);
    film.set_axis(stats.axis);
    film.set_white(stats.white);
    film.mid_a = stats.mid_a;
    film.mid_b = stats.mid_b;
    film.grade = stats.grade;
    film.exposure = stats.exposure;
    film.pivot = stats.pivot;
    film.anchor = stats.anchor;
    if film.stock.negative() {
        film.shoulder = stats.shoulder;
    }
    film.enabled = true;
    Analysis { film, stats, frame_error: None }
}

/// The pixels inside `crop`, in crop coordinates. An uncropped scan is returned unchanged.
/// An angled crop is sampled nearest-pixel, so a pixel outside the crop is not blended in.
pub fn within_crop(img: &Rgb32f, crop: CropGeometry) -> Rgb32f {
    if crop.is_identity() || img.width == 0 || img.height == 0 {
        return img.clone();
    }
    let w = img.width as f64;
    let h = img.height as f64;
    let finite = [crop.rect.x0, crop.rect.y0, crop.rect.x1, crop.rect.y1, crop.angle].iter().all(|v| v.is_finite());
    if !finite {
        return img.clone();
    }
    let r = crop.rect_px(w, h);
    let (rw, rh) = (r.width(), r.height());
    if !rw.is_finite() || !rh.is_finite() || rw < 1.0 || rh < 1.0 {
        return img.clone();
    }
    if crop.angle.abs() < 0.05 {
        let x0 = r.x0.round().clamp(0.0, (w - 1.0).max(0.0)) as usize;
        let y0 = r.y0.round().clamp(0.0, (h - 1.0).max(0.0)) as usize;
        let x1 = r.x1.round().clamp(x0 as f64 + 1.0, w) as usize;
        let y1 = r.y1.round().clamp(y0 as f64 + 1.0, h) as usize;
        let (ow, oh) = (x1 - x0, y1 - y0);
        let mut out = Rgb32f::new(ow, oh);
        for y in y0..y1 {
            for x in x0..x1 {
                let Some(p) = img.data.get(y.saturating_mul(img.width).saturating_add(x)) else { continue };
                let dx = x - x0;
                let dy = y - y0;
                if let Some(slot) = out.data.get_mut(dy.saturating_mul(ow).saturating_add(dx)) {
                    *slot = *p;
                }
            }
        }
        return out;
    }
    let out_w = (rw.round() as usize).clamp(1, img.width.max(1));
    let out_h = (rh.round() as usize).clamp(1, img.height.max(1));
    let map = crop.output_to_source(w, h, out_w as f64, out_h as f64);
    let mut out = Rgb32f::new(out_w, out_h);
    for y in 0..out_h {
        for x in 0..out_w {
            let p = map.apply(Point::new(x as f64 + 0.5, y as f64 + 0.5));
            if !p.x.is_finite() || !p.y.is_finite() {
                continue;
            }
            let sx = p.x.floor();
            let sy = p.y.floor();
            if sx < 0.0 || sy < 0.0 || sx >= w || sy >= h {
                continue;
            }
            let (sx, sy) = (sx as usize, sy as usize);
            let Some(px) = img.data.get(sy.saturating_mul(img.width).saturating_add(sx)) else { continue };
            if let Some(slot) = out.data.get_mut(y.saturating_mul(out_w).saturating_add(x)) {
                *slot = *px;
            }
        }
    }
    out
}

/// Density histogram and rebate readout for the current frame, without writing settings.
pub fn inspect(img: &Rgb32f, film: &Film) -> FrameStats {
    measure(img, film)
}

/// Film connected to the image edge. The same colour inside the picture is left out.
fn rebate_mask(img: &Rgb32f, stock: FilmStock) -> Vec<bool> {
    let w = img.width;
    let h = img.height;
    let Some(n) = w.checked_mul(h) else {
        return Vec::new();
    };
    let mut rebate = vec![false; n];
    if w < 2 || h < 2 {
        return rebate;
    }
    let mut queue = std::collections::VecDeque::new();
    for y in 0..h {
        for x in 0..w {
            if x != 0 && y != 0 && x + 1 != w && y + 1 != h {
                continue;
            }
            let i = y * w + x;
            if img.data.get(i).is_some_and(|p| is_outside(*p, stock, true)) {
                if let Some(slot) = rebate.get_mut(i) {
                    *slot = true;
                }
                queue.push_back(i);
            }
        }
    }
    while let Some(i) = queue.pop_front() {
        let x = i % w;
        let y = i / w;
        for (dx, dy) in [(-1isize, 0), (1, 0), (0, -1), (0, 1)] {
            let nx = x as isize + dx;
            let ny = y as isize + dy;
            if nx < 0 || ny < 0 || nx >= w as isize || ny >= h as isize {
                continue;
            }
            let j = ny as usize * w + nx as usize;
            if rebate.get(j).copied().unwrap_or(true) {
                continue;
            }
            let rim = in_rim(nx.max(0) as usize, ny.max(0) as usize, w, h);
            if img.data.get(j).is_some_and(|p| is_outside(*p, stock, rim))
                && let Some(slot) = rebate.get_mut(j)
            {
                *slot = true;
                queue.push_back(j);
            }
        }
    }
    rebate
}

fn is_holder(p: [f32; 3]) -> bool {
    let y = luminance_2020(p);
    if !(0.02..0.42).contains(&y) {
        return false;
    }
    let max = p[0].max(p[1]).max(p[2]).max(1e-6);
    let min = p[0].min(p[1]).min(p[2]);
    (max - min) / max < 0.22
}

#[derive(Clone, Copy, PartialEq)]
enum Strip {
    Outside,
    Rebate,
    Picture,
}

fn strip_kind(p: [f32; 3], stock: FilmStock) -> Strip {
    let rebate = match stock {
        FilmStock::ColorNeg => is_mask_orange(p) || is_pale_rebate(p),
        FilmStock::BwNeg => luminance_2020(p) > 0.4 && !is_light_table(p),
        FilmStock::ColorPos | FilmStock::BwPos => false,
    };
    if rebate {
        Strip::Rebate
    } else if is_holder(p) || is_light_table(p) || luminance_2020(p) < 0.06 {
        Strip::Outside
    } else {
        Strip::Picture
    }
}

/// Median color of the middle of one row or column. The ends of a picture row are still
/// rebate, and the middle is the picture, so the median is not the border.
fn line_median(img: &Rgb32f, horizontal: bool, index: usize) -> Option<[f32; 3]> {
    let (w, h) = (img.width, img.height);
    let len = if horizontal { w } else { h };
    if len < 8 {
        return None;
    }
    let start = len / 8;
    let end = len.saturating_sub(start).max(start.saturating_add(1));
    let mut px = Vec::with_capacity(end.saturating_sub(start));
    for t in start..end {
        let (x, y) = if horizontal { (t, index) } else { (index, t) };
        if let Some(p) = img.data.get(y.saturating_mul(w).saturating_add(x)) {
            px.push(*p);
        }
    }
    if px.len() < 4 {
        return None;
    }
    Some(median_color(&px).map(|v| v as f32))
}

/// How many strips of holder and rebate sit outside the picture. `from_end` walks the far edge.
fn edge_inset(classes: &[Strip], from_end: bool) -> Option<usize> {
    let n = classes.len();
    let limit = (n.saturating_mul(45) / 100).max(2).min(n);
    let mut rebate = 0usize;
    for step in 0..limit {
        let i = if from_end { n - 1 - step } else { step };
        match classes.get(i).copied().unwrap_or(Strip::Picture) {
            Strip::Outside => {}
            Strip::Rebate => rebate += 1,
            Strip::Picture => {
                if rebate >= 1 {
                    return Some(step);
                }
                return None;
            }
        }
    }
    None
}

/// Holder, lamp, or crushed black. Orange film is not this, so the flood stops at the rebate.
fn is_outside_margin(p: [f32; 3]) -> bool {
    if orange_family(p) {
        return false;
    }
    if is_light_table(p) || is_clipped_lamp(p) {
        return true;
    }
    let y = luminance_2020(p);
    if !(y.is_finite() && y < 0.50) {
        return false;
    }
    if y < 0.07 {
        return true;
    }
    let max = p[0].max(p[1]).max(p[2]).max(1e-6);
    let min = p[0].min(p[1]).min(p[2]);
    (max - min) / max < 0.35
}

fn orange_family(p: [f32; 3]) -> bool {
    is_mask_orange(p) || is_clear_film(p) || is_pale_rebate(p)
}

/// Bright orange is blank film. The same hue darker than that is the picture.
fn is_bright_rebate(p: [f32; 3], level: f32) -> bool {
    if !orange_family(p) {
        return false;
    }
    let y = luminance_2020(p);
    y.is_finite() && y >= level * 0.80 && y <= level * 1.20
}

/// Dark plastic and the lamp, grown from the image edge. A dense patch inside the rebate is not
/// connected, so it stays part of the picture.
fn flood_margin(img: &Rgb32f) -> Vec<bool> {
    let (w, h) = (img.width, img.height);
    let n = w.saturating_mul(h);
    let mut mask = vec![false; n];
    if n == 0 || img.data.len() < n {
        return mask;
    }
    let mut queue = std::collections::VecDeque::new();
    let consider = |x: usize, y: usize, mask: &mut [bool], queue: &mut std::collections::VecDeque<usize>| {
        let i = y.saturating_mul(w).saturating_add(x);
        if mask.get(i).copied().unwrap_or(true) {
            return;
        }
        if img.data.get(i).is_some_and(|p| is_outside_margin(*p))
            && let Some(slot) = mask.get_mut(i)
        {
            *slot = true;
            queue.push_back(i);
        }
    };
    for x in 0..w {
        consider(x, 0, &mut mask, &mut queue);
        if h > 1 {
            consider(x, h - 1, &mut mask, &mut queue);
        }
    }
    for y in 1..h.saturating_sub(1) {
        consider(0, y, &mut mask, &mut queue);
        if w > 1 {
            consider(w - 1, y, &mut mask, &mut queue);
        }
    }
    while let Some(i) = queue.pop_front() {
        let x = i % w;
        let y = i / w;
        let next =
            [(x.wrapping_sub(1), y, x > 0), (x.saturating_add(1), y, x + 1 < w), (x, y.wrapping_sub(1), y > 0), (x, y.saturating_add(1), y + 1 < h)];
        for (nx, ny, ok) in next {
            if !ok {
                continue;
            }
            consider(nx, ny, &mut mask, &mut queue);
        }
    }
    mask
}

/// Luminance of the blank film. The 90th percentile sits in the rebate when the picture is the
/// darker orange, and steps down when that percentile landed on a thin brighter rim.
fn luma_mode(ys: &[f64]) -> Option<f64> {
    if ys.len() < 24 {
        return None;
    }
    let mut sorted = ys.to_vec();
    let hi = percentile_f64(&mut sorted, 0.90)?;
    if !(hi.is_finite() && hi > 0.05) {
        return None;
    }
    let mut upper = 0u32;
    let mut lower = 0u32;
    let mut lower_sum = 0.0f64;
    for y in ys {
        if !y.is_finite() {
            continue;
        }
        if *y >= hi * 0.90 && *y <= hi * 1.15 {
            upper = upper.saturating_add(1);
        } else if *y >= hi * 0.62 && *y < hi * 0.90 {
            lower = lower.saturating_add(1);
            lower_sum += *y;
        }
    }
    if lower > upper && lower >= 8 { Some(lower_sum / f64::from(lower)) } else { Some(hi) }
}

/// Median color of the orange pile. Grain and a washed rim sit off the pile and drop out.
fn mode_color(samples: &[[f32; 3]]) -> Option<[f64; 3]> {
    let ys: Vec<f64> = samples.iter().copied().map(|p| luminance_2020(p) as f64).filter(|y| y.is_finite()).collect();
    let mode = luma_mode(&ys)?;
    let band: Vec<[f32; 3]> = samples
        .iter()
        .copied()
        .filter(|p| {
            let y = luminance_2020(*p) as f64;
            y >= mode * 0.88 && y <= mode * 1.12
        })
        .collect();
    let chosen = if band.len() >= 8 { &band } else { samples };
    (chosen.len() >= 8).then(|| median_color(&stronger_orange(chosen)))
}

fn rebate_level(img: &Rgb32f, margin: &[bool]) -> Option<f32> {
    let mut ys = Vec::new();
    for (i, p) in img.data.iter().enumerate() {
        if margin.get(i).copied().unwrap_or(false) || !orange_family(*p) {
            continue;
        }
        let y = luminance_2020(*p);
        if y.is_finite() {
            ys.push(y as f64);
        }
    }
    let level = luma_mode(&ys)?;
    (level > 0.05).then_some(level as f32)
}

fn line_kind(img: &Rgb32f, margin: &[bool], level: f32, horizontal: bool, index: usize) -> Strip {
    let (w, h) = (img.width, img.height);
    let len = if horizontal { w } else { h };
    let mut kept = Vec::new();
    for t in 0..len {
        let (x, y) = if horizontal { (t, index) } else { (index, t) };
        let i = y.saturating_mul(w).saturating_add(x);
        if margin.get(i).copied().unwrap_or(false) {
            continue;
        }
        if let Some(p) = img.data.get(i) {
            kept.push(*p);
        }
    }
    // A holder row is almost all plastic. A thin negative in that holder is the rest.
    if kept.len() < 4 || kept.len().saturating_mul(15) < len {
        return Strip::Outside;
    }
    let med = median_color(&kept).map(|v| v as f32);
    if is_bright_rebate(med, level) {
        return Strip::Rebate;
    }
    // A washed rim is orange and brighter than the film. It is not the picture: the walk has to
    // cross it to reach the rebate. Darker orange is the picture.
    if orange_family(med) && luminance_2020(med) > level * 1.20 { Strip::Outside } else { Strip::Picture }
}

#[derive(Clone, Copy, Debug)]
struct EdgeWalk {
    inset: usize,
    rebate: bool,
}

fn walk_edge(classes: &[Strip], from_end: bool) -> Option<EdgeWalk> {
    let n = classes.len();
    let limit = (n.saturating_mul(72) / 100).max(2).min(n);
    let mut rebate = 0usize;
    let mut outside = 0usize;
    for step in 0..limit {
        let i = if from_end { n - 1 - step } else { step };
        match classes.get(i).copied().unwrap_or(Strip::Picture) {
            Strip::Outside => outside += 1,
            Strip::Rebate => rebate += 1,
            Strip::Picture => {
                if step > 0 && (rebate >= 1 || outside >= 2) {
                    return Some(EdgeWalk { inset: step, rebate: rebate >= 1 });
                }
                return None;
            }
        }
    }
    None
}

struct RebateHit {
    frame: FilmFrame,
    /// Stored scan color of the rebate. The same value for every border click.
    color: Option<[f64; 3]>,
}

fn frame_from_edges(
    top: Option<EdgeWalk>,
    bottom: Option<EdgeWalk>,
    left: Option<EdgeWalk>,
    right: Option<EdgeWalk>,
    w: usize,
    h: usize,
) -> Option<FilmFrame> {
    let edges = [top, bottom, left, right];
    let found = edges.iter().filter(|e| e.is_some()).count();
    let rebate_edges = edges.iter().filter(|e| e.is_some_and(|e| e.rebate)).count();
    if found < 3 || rebate_edges < 2 || w == 0 || h == 0 {
        return None;
    }
    let inset = |e: Option<EdgeWalk>| e.map(|e| e.inset).unwrap_or(0);
    let top = inset(top);
    let left = inset(left);
    let bottom = h.saturating_sub(inset(bottom));
    let right = w.saturating_sub(inset(right));
    if bottom <= top.saturating_add(4) || right <= left.saturating_add(4) {
        return None;
    }
    let area = (right - left) as f64 / w as f64 * ((bottom - top) as f64 / h as f64);
    if !(0.04..0.97).contains(&area) {
        return None;
    }
    Some(FilmFrame {
        corners: [
            [left as f64 / w as f64, top as f64 / h as f64],
            [right as f64 / w as f64, top as f64 / h as f64],
            [right as f64 / w as f64, bottom as f64 / h as f64],
            [left as f64 / w as f64, bottom as f64 / h as f64],
        ],
    })
}

/// The picture inside the bright rebate, and that rebate's color. The holder is not part of either.
fn rebate_hit(img: &Rgb32f) -> Option<RebateHit> {
    if img.width < 16 || img.height < 16 || img.data.len() < img.width.saturating_mul(img.height) {
        return None;
    }
    let margin = flood_margin(img);
    let level = rebate_level(img, &margin)?;
    let (w, h) = (img.width, img.height);
    let mut rows = Vec::with_capacity(h);
    for y in 0..h {
        rows.push(line_kind(img, &margin, level, true, y));
    }
    let mut cols = Vec::with_capacity(w);
    for x in 0..w {
        cols.push(line_kind(img, &margin, level, false, x));
    }
    let frame = frame_from_edges(walk_edge(&rows, false), walk_edge(&rows, true), walk_edge(&cols, false), walk_edge(&cols, true), w, h)?;
    let mut samples = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let i = y.saturating_mul(w).saturating_add(x);
            if margin.get(i).copied().unwrap_or(false) {
                continue;
            }
            let Some(p) = img.data.get(i) else { continue };
            if !is_bright_rebate(*p, level) {
                continue;
            }
            let nx = (x as f64 + 0.5) / w as f64;
            let ny = (y as f64 + 0.5) / h as f64;
            if !inside_frame(&frame, nx, ny) {
                samples.push(*p);
            }
        }
    }
    Some(RebateHit { frame, color: mode_color(&samples) })
}

/// The picture quad inside a dark holder and the rebate. `None` when an edge has no rebate.
fn rebate_insets(img: &Rgb32f, stock: FilmStock) -> Option<FilmFrame> {
    if !matches!(stock, FilmStock::ColorNeg | FilmStock::BwNeg) || img.width < 16 || img.height < 16 {
        return None;
    }
    let (w, h) = (img.width, img.height);
    let mut rows = Vec::with_capacity(h);
    for y in 0..h {
        let kind = line_median(img, true, y).map(|p| strip_kind(p, stock)).unwrap_or(Strip::Picture);
        rows.push(kind);
    }
    let mut cols = Vec::with_capacity(w);
    for x in 0..w {
        let kind = line_median(img, false, x).map(|p| strip_kind(p, stock)).unwrap_or(Strip::Picture);
        cols.push(kind);
    }
    let top = edge_inset(&rows, false)?;
    let bottom_border = edge_inset(&rows, true)?;
    let left = edge_inset(&cols, false)?;
    let right_border = edge_inset(&cols, true)?;
    let bottom = h.saturating_sub(bottom_border);
    let right = w.saturating_sub(right_border);
    if bottom <= top.saturating_add(4) || right <= left.saturating_add(4) {
        return None;
    }
    let area = (right - left) as f64 / w as f64 * ((bottom - top) as f64 / h as f64);
    if !(0.2..0.96).contains(&area) {
        return None;
    }
    Some(FilmFrame {
        corners: [
            [left as f64 / w as f64, top as f64 / h as f64],
            [right as f64 / w as f64, top as f64 / h as f64],
            [right as f64 / w as f64, bottom as f64 / h as f64],
            [left as f64 / w as f64, bottom as f64 / h as f64],
        ],
    })
}

#[derive(Clone, Copy, Debug)]
enum Side {
    Top,
    Bottom,
    Left,
    Right,
}

struct Plateau {
    frame: Option<FilmFrame>,
    /// Stored scan color of the flat rebate. The same value for every border click.
    color: Option<[f64; 3]>,
}

struct Band {
    /// Pixels from the outer edge to the first picture pixel.
    inset: usize,
    pixels: Vec<[f32; 3]>,
}

/// The flat blank film just inside the holder or the lamp.
///
/// A bright sky can match the rebate's orange, so a brightness test never sees a picture. The
/// rebate is the run that starts at the holder and stays one color, and it ends where that color
/// changes. The click is not an input.
fn plateau_border(img: &Rgb32f) -> Plateau {
    let none = Plateau { frame: None, color: None };
    let (w, h) = (img.width, img.height);
    if w < 16 || h < 16 || img.data.len() < w.saturating_mul(h) {
        return none;
    }
    let mut samples = Vec::new();
    let mut top = None;
    let mut bottom = None;
    let mut left = None;
    let mut right = None;
    for side in [Side::Top, Side::Bottom, Side::Left, Side::Right] {
        let n = match side {
            Side::Top | Side::Bottom => w,
            Side::Left | Side::Right => h,
        };
        let start = n / 8;
        let end = n.saturating_sub(start);
        if end <= start {
            continue;
        }
        let step = ((end - start) / 28).max(1);
        let span = match side {
            Side::Top | Side::Bottom => h,
            Side::Left | Side::Right => w,
        };
        let mut insets = Vec::new();
        let mut side_samples = Vec::new();
        let mut tried = 0usize;
        for t in (start..end).step_by(step) {
            tried = tried.saturating_add(1);
            let Some(band) = border_band(&inward_line(img, side, t), span) else { continue };
            insets.push(band.inset);
            side_samples.extend(band.pixels);
        }
        // A patch inside the picture hits a few lines at different depths. A real edge repeats.
        if insets_agree(&insets, span, tried) {
            let hit = median_inset(&mut insets).map(|inset| EdgeWalk { inset, rebate: true });
            samples.extend(side_samples);
            match side {
                Side::Top => top = hit,
                Side::Bottom => bottom = hit,
                Side::Left => left = hit,
                Side::Right => right = hit,
            }
        }
    }
    let color = (samples.len() >= 16).then(|| median_color(&stronger_orange(&samples)));
    Plateau { frame: frame_from_edges(top, bottom, left, right, w, h), color }
}

fn median_inset(v: &mut [usize]) -> Option<usize> {
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    v.get(v.len() / 2).copied()
}

/// The stops are one edge when the middle half of them sit close together. A handful of lines
/// only count when that edge is still in the outer rim; an edge most of the side agrees on can
/// sit further in.
fn insets_agree(insets: &[usize], span: usize, tried: usize) -> bool {
    if insets.len() < 4 || span == 0 {
        return false;
    }
    let mut sorted = insets.to_vec();
    sorted.sort_unstable();
    let lo = sorted.get(sorted.len() / 4).copied().unwrap_or(0);
    let hi_at = sorted.len().saturating_mul(3) / 4;
    let hi = sorted.get(hi_at.min(sorted.len() - 1)).copied().unwrap_or(lo);
    if hi.saturating_sub(lo).saturating_mul(5) >= span {
        return false;
    }
    let mid = sorted.get(sorted.len() / 2).copied().unwrap_or(0);
    let majority = insets.len().saturating_mul(2) >= tried.max(1);
    majority || mid.saturating_mul(100) <= span.saturating_mul(14)
}

fn inward_line(img: &Rgb32f, side: Side, t: usize) -> Vec<[f32; 3]> {
    let (w, h) = (img.width, img.height);
    let vertical = matches!(side, Side::Top | Side::Bottom);
    let len = if vertical { h } else { w };
    let along = if vertical { w } else { h };
    if t >= along || len < 4 || img.data.len() < w.saturating_mul(h) {
        return Vec::new();
    }
    let from_end = matches!(side, Side::Bottom | Side::Right);
    let limit = (len.saturating_mul(70) / 100).max(4).min(len);
    let mut out = Vec::with_capacity(limit);
    for step in 0..limit {
        let i = if from_end { len - 1 - step } else { step };
        let (x, y) = if vertical { (t, i) } else { (i, t) };
        let idx = y.saturating_mul(w).saturating_add(x);
        if let Some(p) = img.data.get(idx).copied() {
            out.push(p);
        }
    }
    out
}

/// One side's rebate: the first stable orange run after the holder. A washed rim in front of a
/// lamp gives way to the longer, more orange run behind it. `span` is the full side, so a long
/// walk across the picture cannot turn an interior patch into the border.
fn border_band(line: &[[f32; 3]], span: usize) -> Option<Band> {
    if line.len() < 6 {
        return None;
    }
    let mut i = 0usize;
    let mut from_lamp = false;
    while i < line.len() {
        let p = line.get(i).copied()?;
        if !is_outside_margin(p) {
            break;
        }
        if is_light_table(p) || is_clipped_lamp(p) {
            from_lamp = true;
        }
        i = i.saturating_add(1);
    }
    let skipped = i;
    // The holder is a rim. Blank film that only appears after a third of the frame is the picture.
    if skipped.saturating_mul(100) > span.saturating_mul(30) {
        return None;
    }
    let rest = line.get(skipped..)?;
    let first = stable_run(rest)?;
    let mut chosen = first;
    let mut origin = skipped;
    if from_lamp && washed_score(&chosen.pixels) > 0.15 {
        let first_end = chosen.inset;
        if let Some(second) = rest.get(first_end..).and_then(stable_run)
            && deeper_orange(&chosen.pixels, &second.pixels)
        {
            origin = skipped.saturating_add(first_end);
            chosen = second;
        }
    }
    Some(Band { inset: origin.saturating_add(chosen.inset), pixels: chosen.pixels })
}

fn washed_score(px: &[[f32; 3]]) -> f32 {
    let m = median_color(px);
    orange_score([m[0] as f32, m[1] as f32, m[2] as f32])
}

/// The run behind a washed rim is the film when it is more orange and at least as wide.
fn deeper_orange(rim: &[[f32; 3]], film: &[[f32; 3]]) -> bool {
    if film.len() < rim.len() || rim.is_empty() {
        return false;
    }
    let rim_score = washed_score(rim);
    let film_score = washed_score(film);
    if !(rim_score > film_score * 1.35 && film_score > 0.0) {
        return false;
    }
    let rim_y = luminance_2020(median_color(rim).map(|v| v as f32)) as f64;
    let film_y = luminance_2020(median_color(film).map(|v| v as f32)) as f64;
    film_y > rim_y * 0.45
}

fn outer_level(line: &[[f32; 3]]) -> Option<f64> {
    // The first orange pixels against the holder. A brighter picture further in must not set
    // this, or the rebate reads as too dark to be film and the side is dropped.
    let mut ys = Vec::new();
    for p in line.iter().take(12) {
        if is_outside_margin(*p) || !orange_family(*p) {
            continue;
        }
        let y = luminance_2020(*p) as f64;
        if y.is_finite() {
            ys.push(y);
        }
    }
    if ys.len() < 4 {
        return None;
    }
    let level = percentile_f64(&mut ys, 0.50)?;
    (level > 0.05).then_some(level)
}

fn med3(line: &[[f32; 3]], i: usize) -> [f32; 3] {
    let n = line.len();
    let Some(center) = line.get(i).copied() else { return [0.0; 3] };
    if n == 0 {
        return center;
    }
    let i0 = i.saturating_sub(1);
    let i2 = (i + 1).min(n - 1);
    let a = line.get(i0).copied().unwrap_or(center);
    let c = line.get(i2).copied().unwrap_or(center);
    let mut out = [0.0f32; 3];
    for ch in 0..3 {
        let (lo, hi) = if a[ch] < c[ch] { (a[ch], c[ch]) } else { (c[ch], a[ch]) };
        let b = center[ch];
        out[ch] = if b < lo {
            lo
        } else if b > hi {
            hi
        } else {
            b
        };
    }
    out
}

fn near_ref(p: [f32; 3], reference: [f64; 3]) -> bool {
    for c in 0..3 {
        let a = p[c] as f64;
        let b = reference[c];
        if !a.is_finite() || !b.is_finite() {
            return false;
        }
        let scale = a.max(b).max(0.05);
        if (a - b).abs() / scale > 0.08 {
            return false;
        }
    }
    true
}

/// A run of one color that actually ends. A field with no inner edge is not a rebate.
fn stable_run(line: &[[f32; 3]]) -> Option<Band> {
    let level = outer_level(line)?;
    let mut kept = Vec::new();
    let mut misses = 0usize;
    let mut started = false;
    let mut inset = 0usize;
    let mut origin: Option<[f64; 3]> = None;
    let start_by = (line.len() / 5).max(8);
    for (i, raw) in line.iter().copied().enumerate() {
        let y_raw = luminance_2020(raw) as f64;
        if !started {
            if i > start_by {
                return None;
            }
            if orange_family(raw) && y_raw >= level * 0.82 && !is_outside_margin(raw) {
                started = true;
                kept.push(raw);
                inset = i.saturating_add(1);
            }
            continue;
        }
        if origin.is_none() && kept.len() >= 5 {
            origin = Some(median_color(&kept));
        }
        let reference = if let Some(locked) = origin { locked } else { median_color(&kept) };
        let sample = med3(line, i);
        let y = luminance_2020(sample) as f64;
        let ok = orange_family(sample) && y >= level * 0.72 && near_ref(sample, reference);
        if ok {
            kept.push(sample);
            misses = 0;
            inset = i.saturating_add(1);
        } else {
            misses = misses.saturating_add(1);
            if misses >= 4 {
                break;
            }
        }
    }
    if misses < 4 || kept.len() < 4 {
        return None;
    }
    let med = median_color(&kept);
    if !orange_family([med[0] as f32, med[1] as f32, med[2] as f32]) {
        return None;
    }
    Some(Band { inset, pixels: kept })
}

/// Fit a quad to the rebate. `None` when the border is missing or the fit swallows the picture.
pub fn detect_frame(img: &Rgb32f, stock: FilmStock) -> Option<FilmFrame> {
    if img.width < 8 || img.height < 8 {
        return None;
    }
    // The quad is fit on the area average, so a dusty pixel is not an edge and the full scan is
    // not walked once per row. The bright ring inside a holder wins. When the picture is as
    // orange as that ring, the flat band against the holder is the edge. Base color is read
    // from the original pixels.
    let small = preview(img);
    if stock == FilmStock::ColorNeg {
        if let Some(frame) = rebate_hit(&small).map(|hit| hit.frame) {
            return Some(frame);
        }
        if let Some(frame) = plateau_border(&small).frame {
            return Some(frame);
        }
    }
    if let Some(frame) = rebate_insets(&small, stock) {
        return Some(frame);
    }
    let rebate = rebate_mask(&small, stock);
    let n = rebate.len();
    if n == 0 {
        return None;
    }
    let (w, h) = (small.width, small.height);
    let count = rebate.iter().filter(|on| **on).count();
    let frac = count as f64 / n as f64;
    if !(0.005..0.80).contains(&frac) {
        return None;
    }
    let mut pts = Vec::new();
    for (i, flag) in rebate.iter().enumerate() {
        if *flag {
            continue;
        }
        pts.push((i % w, i / w));
    }
    let frame = oriented_box(&pts, w, h)?;
    let area = quad_area(&frame.corners);
    (0.05..0.98).contains(&area).then_some(frame)
}

/// The picture quad as a crop, so the blank film stays outside the picture.
/// `w` and `h` are the oriented image in pixels. `None` when the quad is the whole scan
/// or the corners do not make a rectangle.
pub fn crop_for_frame(frame: &FilmFrame, w: f64, h: f64) -> Option<CropGeometry> {
    if !(w.is_finite() && h.is_finite()) || w < 1.0 || h < 1.0 || frame.is_full() {
        return None;
    }
    let px: Vec<Point> = frame.corners.iter().map(|c| Point::new(c[0] * w, c[1] * h)).filter(|p| p.x.is_finite() && p.y.is_finite()).collect();
    let (Some(p0), Some(p1)) = (px.first(), px.get(1)) else {
        return None;
    };
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
    let mut minx = f64::MAX;
    let mut miny = f64::MAX;
    let mut maxx = f64::MIN;
    let mut maxy = f64::MIN;
    for p in &px {
        let q = into.apply(*p);
        minx = minx.min(q.x);
        miny = miny.min(q.y);
        maxx = maxx.max(q.x);
        maxy = maxy.max(q.y);
    }
    if maxx - minx < 1.0 || maxy - miny < 1.0 {
        return None;
    }
    let rect = Rect::new(minx / w, miny / h, maxx / w, maxy / h);
    Some(CropGeometry { rect, angle }.constrained(w, h))
}

/// The picture inside a strip, past clear film, sprocket holes, and the printing on the rebate.
///
/// The rebate flood follows orange, and a bright negative is orange too, so that flood
/// swallows the picture and the crop has nothing to cut. Clear film is a flat bright
/// band; sprocket holes are brighter still. The picture is the tone inside them.
pub fn detect_gate(img: &Rgb32f, stock: FilmStock) -> Option<FilmFrame> {
    if !stock.negative() || img.width < 16 || img.height < 16 {
        return None;
    }
    let small = preview(img);
    let (w, h) = (small.width, small.height);
    let n = w.checked_mul(h)?;
    let mut luma = Vec::with_capacity(n);
    for p in &small.data {
        luma.push(luminance_2020(*p));
    }
    let film = clear_film_level(&luma, w, h)?;
    let mut row_out = Vec::with_capacity(h);
    for y in 0..h {
        let start = y.saturating_mul(w);
        let line = luma.get(start..start.saturating_add(w)).unwrap_or(&[]);
        let border = y.saturating_mul(100) < h.saturating_mul(15) || y.saturating_add(1).saturating_mul(100) > h.saturating_mul(85);
        row_out.push(margin_line(line, film, y < h / 25 || y + h / 25 >= h, border));
    }
    let top = content_from(&row_out, false)?;
    let bottom = content_from(&row_out, true)?;
    if bottom <= top + 4 {
        return None;
    }
    let mut col_out = Vec::with_capacity(w);
    for x in 0..w {
        let mut line = Vec::with_capacity(bottom - top + 1);
        for y in top..=bottom {
            if let Some(v) = luma.get(y.saturating_mul(w).saturating_add(x)) {
                line.push(*v);
            }
        }
        let border = x.saturating_mul(100) < w.saturating_mul(15) || x.saturating_add(1).saturating_mul(100) > w.saturating_mul(85);
        col_out.push(margin_line(&line, film, x < w / 25 || x + w / 25 >= w, border));
    }
    let left = content_from(&col_out, false)?;
    let right = content_from(&col_out, true)?;
    if right <= left + 4 {
        return None;
    }
    let touches = top < 2 && left < 2 && bottom + 2 >= h && right + 2 >= w;
    if touches {
        return None;
    }
    let fw = w as f64;
    let fh = h as f64;
    let frame = FilmFrame {
        corners: [
            [left as f64 / fw, top as f64 / fh],
            [right as f64 / fw, top as f64 / fh],
            [right as f64 / fw, bottom as f64 / fh],
            [left as f64 / fw, bottom as f64 / fh],
        ],
    };
    let area = quad_area(&frame.corners);
    (0.15..0.98).contains(&area).then_some(frame)
}

fn content_from(out: &[bool], reverse: bool) -> Option<usize> {
    let n = out.len();
    if n == 0 {
        return None;
    }
    let mut i = if reverse { n - 1 } else { 0 };
    let end = if reverse { 0 } else { n - 1 };
    let step: isize = if reverse { -1 } else { 1 };
    loop {
        let outside = out.get(i).copied().unwrap_or(true);
        if !outside {
            // A one- or two-pixel rim between the dark edge and the clear film is not the picture.
            let mut j = i;
            let mut run = 0usize;
            let mut margin_after = false;
            for _ in 0..6 {
                if out.get(j).copied().unwrap_or(true) {
                    margin_after = true;
                    break;
                }
                run += 1;
                if j == end {
                    break;
                }
                j = j.saturating_add_signed(step);
            }
            if margin_after && run <= 2 {
                i = j;
                continue;
            }
            return Some(i);
        }
        if i == end {
            return None;
        }
        i = i.saturating_add_signed(step);
    }
}

fn clear_film_level(luma: &[f32], w: usize, h: usize) -> Option<f32> {
    // Between sprocket holes the film is the row's median: the holes are the bright minority.
    let mut between_holes = Vec::new();
    for y in 0..h {
        let start = y.saturating_mul(w);
        let line = luma.get(start..start.saturating_add(w)).unwrap_or(&[]);
        let Some(stats) = line_stats(line) else {
            continue;
        };
        let (median, bright) = (stats.median, stats.bright);
        if bright > 0.12 && median > 0.15 && median < 0.9 {
            between_holes.push(median);
        }
    }
    if between_holes.len() >= 3 {
        between_holes.sort_by(|a, b| a.total_cmp(b));
        return between_holes.get(between_holes.len() / 2).copied();
    }
    let mut samples = Vec::new();
    let band_y = (h / 12).max(1);
    for y in 0..h {
        if y > band_y && y + band_y < h {
            continue;
        }
        let start = y.saturating_mul(w);
        let line = luma.get(start..start.saturating_add(w)).unwrap_or(&[]);
        let Some(stats) = line_stats(line) else {
            continue;
        };
        let (median, std, bright) = (stats.median, stats.std, stats.bright);
        if bright < 0.08 && std < 0.08 && median > 0.2 {
            samples.push(median);
        }
    }
    if samples.len() < 2 {
        return None;
    }
    samples.sort_by(|a, b| a.total_cmp(b));
    samples.get(samples.len() / 2).copied()
}

struct LineStats {
    median: f32,
    std: f32,
    bright: f32,
    /// 35th percentile. Printed edge text can pull the bottom of the line down; the picture pulls this too.
    p35: f32,
}

fn margin_line(line: &[f32], film: f32, edge: bool, border: bool) -> bool {
    let Some(stats) = line_stats(line) else {
        return true;
    };
    if stats.bright > 0.12 || (edge && stats.median < 0.08) {
        return true;
    }
    if (stats.median - film).abs() >= 0.05 {
        return false;
    }
    if stats.std < 0.08 {
        return true;
    }
    // Edge printing is a dark minority on clear film, and only in the rebate. Past that, a line
    // that is no longer flat is the picture, even when most of it is still bright.
    border && (stats.p35 - film).abs() < 0.05 && dark_share(line, film) < 0.40
}

fn dark_share(line: &[f32], film: f32) -> f32 {
    if line.is_empty() {
        return 0.0;
    }
    let dark = line.iter().filter(|v| film - **v > 0.12).count();
    dark as f32 / line.len() as f32
}

fn line_stats(line: &[f32]) -> Option<LineStats> {
    if line.is_empty() {
        return None;
    }
    let mut sorted = line.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let mid = sorted.len() / 2;
    let p35_at = sorted.len().saturating_mul(35) / 100;
    let median = sorted.get(mid).copied()?;
    let p35 = sorted.get(p35_at).copied().unwrap_or(median);
    let n = line.len() as f32;
    let mean = line.iter().sum::<f32>() / n;
    let var = line
        .iter()
        .map(|v| {
            let d = v - mean;
            d * d
        })
        .sum::<f32>()
        / n;
    let bright = line.iter().filter(|v| **v > 0.85).count() as f32 / n;
    Some(LineStats { median, std: var.sqrt(), bright, p35 })
}

fn in_rim(x: usize, y: usize, w: usize, h: usize) -> bool {
    let mx = (w / 7).max(1);
    let my = (h / 7).max(1);
    x < mx || y < my || x + mx >= w || y + my >= h
}

/// Bare light around a strip: bright and almost neutral. It is not the film.
fn is_light_table(p: [f32; 3]) -> bool {
    let max = p[0].max(p[1]).max(p[2]);
    let min = p[0].min(p[1]).min(p[2]);
    if max < 0.72 {
        return false;
    }
    let chroma = if max > 1e-6 { (max - min) / max } else { 0.0 };
    chroma < 0.18
}

/// A backlight that has run out of headroom. It has no mask color left. A rebate that camera
/// white balance has lifted toward white is dimmer than this and is still the film.
fn is_clipped_lamp(p: [f32; 3]) -> bool {
    let max = p[0].max(p[1]).max(p[2]);
    let min = p[0].min(p[1]).min(p[2]);
    if max < 0.94 || !max.is_finite() {
        return false;
    }
    (max - min) / max < 0.08
}

/// Unexposed color negative: red well above green, green above blue, and not a white lamp.
fn is_mask_orange(p: [f32; 3]) -> bool {
    if is_light_table(p) {
        return false;
    }
    let (r, g, b) = (p[0], p[1], p[2]);
    r > 0.18 && r > g * 1.25 && r > b * 1.8 && g + 0.015 >= b
}

/// Clear film, not the bright rim where the orange mixes with the lamp. That rim is brighter
/// and much bluer, and using it as the base turns the print blue.
fn is_clear_film(p: [f32; 3]) -> bool {
    if is_light_table(p) {
        return false;
    }
    let (r, g, b) = (p[0], p[1], p[2]);
    r > 0.22 && r > g * 1.35 && r > b * 2.5 && g + 0.02 >= b
}

/// Blank film after the strong-orange test has failed: old stock, a faded mask, or a raw whose
/// camera white balance has already lifted the orange toward white. It stays bright, and blue
/// stays near red. A yellow field, a gray midtone, and the clipped lamp are not this.
fn is_pale_rebate(p: [f32; 3]) -> bool {
    if is_clipped_lamp(p) || is_mask_orange(p) {
        return false;
    }
    let y = luminance_2020(p);
    if !(0.45..0.93).contains(&y) {
        return false;
    }
    let (r, g, b) = (p[0], p[1], p[2]);
    let max = r.max(g).max(b).max(1e-6);
    let min = r.min(g).min(b);
    let chroma = (max - min) / max;
    chroma <= 0.42 && b > r * 0.45 && g + 0.08 >= b && r + 0.08 >= g
}

const BORDER_BLOWN: &str = "that spot is blown out — click the blank film, not the white lamp";
const BORDER_DARK: &str = "that spot is too dark — click the blank film, not the picture";

/// Why a border click cannot be the base. A clipped lamp has no mask color left. Anything
/// darker than the scan's own bright film is a different refusal: on a dark scan the floor
/// follows `bright` (see [`bright_level`]), and on a bright scan it stays at the old fixed level.
pub fn border_error(stock: FilmStock, sample: [f64; 3], bright: f64) -> Option<&'static str> {
    let p = sample.map(|v| v as f32);
    if !p.iter().all(|c| c.is_finite()) {
        return Some(BORDER_BLOWN);
    }
    let (fixed, min_abs) = match stock {
        FilmStock::ColorNeg => (0.15f64, 0.04f64),
        FilmStock::BwNeg => (0.25, 0.06),
        FilmStock::ColorPos | FilmStock::BwPos => return None,
    };
    if is_clipped_lamp(p) {
        return Some(BORDER_BLOWN);
    }
    let bright = if bright.is_finite() { bright.max(0.0) } else { fixed };
    let floor = (bright * 0.55).max(min_abs).min(fixed);
    if (luminance_2020(p) as f64) < floor { Some(BORDER_DARK) } else { None }
}

/// A border click on a bright scan. The base is whatever the blank film measures as, including
/// a pale or near-white rebate.
pub fn accepts_border(stock: FilmStock, sample: [f64; 3]) -> bool {
    border_error(stock, sample, 1.0).is_none()
}

/// Luminance of this scan's own bright film, ignoring a clipped lamp. The 98th percentile,
/// so a few hot pixels do not set the floor.
pub fn bright_level(img: &Rgb32f, capture: FilmCapture) -> f64 {
    let mut hist = [0u32; 64];
    let mut n = 0u32;
    let g = capture.gamma() as f32;
    for p in &img.data {
        let seen = p.map(|c| capture_channel(c, g));
        if is_clipped_lamp(seen) {
            continue;
        }
        let y = luminance_2020(seen).clamp(0.0, 1.0);
        let bin = ((y * 63.0) as usize).min(63);
        if let Some(slot) = hist.get_mut(bin) {
            *slot += 1;
            n = n.saturating_add(1);
        }
    }
    if n == 0 {
        return 0.15;
    }
    let target = ((n as f64) * 0.98).round().max(1.0) as u32;
    let mut acc = 0u32;
    for (i, c) in hist.iter().enumerate() {
        acc = acc.saturating_add(*c);
        if acc >= target {
            return i as f64 / 63.0;
        }
    }
    1.0
}

fn is_outside(p: [f32; 3], stock: FilmStock, rim: bool) -> bool {
    let y = luminance_2020(p);
    match stock {
        FilmStock::ColorNeg => is_mask_orange(p) || is_light_table(p) || is_pale_rebate(p) || (rim && y < 0.05),
        FilmStock::BwNeg => (y > 0.45 && !is_light_table(p)) || is_light_table(p) || (rim && y < 0.05),
        FilmStock::ColorPos | FilmStock::BwPos => y < 0.06,
    }
}

fn preview(img: &Rgb32f) -> Rgb32f {
    let long = img.width.max(img.height);
    if long <= 256 || img.width == 0 || img.height == 0 {
        return img.clone();
    }
    // Area average, not the one source pixel under the sample. Grain on a rebate is the same
    // orange as the film once the block is averaged, and a single dusty pixel is not a border.
    let k = 256.0 / long as f64;
    let w = ((img.width as f64 * k).round() as usize).clamp(1, img.width);
    let h = ((img.height as f64 * k).round() as usize).clamp(1, img.height);
    Rgb32f::from_fn(w, h, |x, y| block_mean(img, x, y, w, h))
}

fn block_mean(img: &Rgb32f, x: usize, y: usize, w: usize, h: usize) -> [f32; 3] {
    let x0 = x.saturating_mul(img.width) / w.max(1);
    let y0 = y.saturating_mul(img.height) / h.max(1);
    let x1 = (x.saturating_add(1).saturating_mul(img.width) / w.max(1)).max(x0.saturating_add(1)).min(img.width);
    let y1 = (y.saturating_add(1).saturating_mul(img.height) / h.max(1)).max(y0.saturating_add(1)).min(img.height);
    let mut s = [0.0f64; 3];
    let mut n = 0u32;
    for yy in y0..y1 {
        let row = yy.saturating_mul(img.width);
        for xx in x0..x1 {
            let Some(p) = img.data.get(row.saturating_add(xx)) else { continue };
            for c in 0..3 {
                s[c] += p[c] as f64;
            }
            n = n.saturating_add(1);
        }
    }
    if n == 0 {
        return [0.0; 3];
    }
    let inv = 1.0 / f64::from(n);
    s.map(|v| (v * inv) as f32)
}

fn oriented_box(pts: &[(usize, usize)], w: usize, h: usize) -> Option<FilmFrame> {
    if pts.len() < 16 || w == 0 || h == 0 {
        return None;
    }
    let (fw, fh) = (w as f64, h as f64);
    let mut mx = 0.0;
    let mut my = 0.0;
    for (x, y) in pts {
        mx += *x as f64;
        my += *y as f64;
    }
    let n = pts.len() as f64;
    mx /= n;
    my /= n;
    let mut cxx = 0.0;
    let mut cxy = 0.0;
    let mut cyy = 0.0;
    for (x, y) in pts {
        let dx = *x as f64 - mx;
        let dy = *y as f64 - my;
        cxx += dx * dx;
        cxy += dx * dy;
        cyy += dy * dy;
    }
    cxx /= n;
    cxy /= n;
    cyy /= n;
    let (v1, v2) = axes(cxx, cxy, cyy);
    let mut umin = f64::MAX;
    let mut umax = f64::MIN;
    let mut vmin = f64::MAX;
    let mut vmax = f64::MIN;
    for (x, y) in pts {
        let dx = *x as f64 - mx;
        let dy = *y as f64 - my;
        let u = dx * v1[0] + dy * v1[1];
        let v = dx * v2[0] + dy * v2[1];
        umin = umin.min(u);
        umax = umax.max(u);
        vmin = vmin.min(v);
        vmax = vmax.max(v);
    }
    let corner = |u: f64, v: f64| {
        let x = (mx + u * v1[0] + v * v2[0]) / fw;
        let y = (my + u * v1[1] + v * v2[1]) / fh;
        [x.clamp(-0.05, 1.05), y.clamp(-0.05, 1.05)]
    };
    Some(FilmFrame { corners: [corner(umin, vmin), corner(umax, vmin), corner(umax, vmax), corner(umin, vmax)] })
}

fn axes(cxx: f64, cxy: f64, cyy: f64) -> ([f64; 2], [f64; 2]) {
    if cxy.abs() < 1e-8 {
        return if cxx >= cyy { ([1.0, 0.0], [0.0, 1.0]) } else { ([0.0, 1.0], [-1.0, 0.0]) };
    }
    let tr = cxx + cyy;
    let det = cxx * cyy - cxy * cxy;
    let disc = (tr * tr * 0.25 - det).max(0.0).sqrt();
    let l1 = tr * 0.5 + disc;
    let mut v1 = [l1 - cyy, cxy];
    let len = (v1[0] * v1[0] + v1[1] * v1[1]).sqrt();
    if len < 1e-12 {
        return ([1.0, 0.0], [0.0, 1.0]);
    }
    v1[0] /= len;
    v1[1] /= len;
    (v1, [-v1[1], v1[0]])
}

fn quad_area(c: &[[f64; 2]; 4]) -> f64 {
    let mut a = 0.0;
    for i in 0..4 {
        let j = (i + 1) % 4;
        let (Some(p), Some(q)) = (c.get(i), c.get(j)) else { continue };
        a += p[0] * q[1] - q[0] * p[1];
    }
    a.abs() * 0.5
}

/// The frame [`Film::inset`] leaves for the picture. The mask is not trimmed.
fn analysis_frame(film: &Film) -> FilmFrame {
    let margin = if film.inset.is_finite() { (film.inset / 100.0).clamp(0.0, 0.4) } else { 0.0 };
    inset_frame(&film.frame, margin)
}

/// Pull each corner toward the frame's center so a fraction `margin` of the frame is dropped
/// on every side. Zero leaves the quad alone. A rectangle loses `margin` of its width and height.
fn inset_frame(frame: &FilmFrame, margin: f64) -> FilmFrame {
    if !margin.is_finite() || margin <= 1e-6 {
        return *frame;
    }
    let scale = 1.0 - 2.0 * margin.clamp(0.0, 0.45);
    let (mut cx, mut cy) = (0.0, 0.0);
    for p in &frame.corners {
        cx += p[0];
        cy += p[1];
    }
    cx /= 4.0;
    cy /= 4.0;
    let mut out = *frame;
    for (i, p) in frame.corners.iter().enumerate() {
        if let Some(slot) = out.corners.get_mut(i) {
            *slot = [cx + (p[0] - cx) * scale, cy + (p[1] - cy) * scale];
        }
    }
    out
}

fn interior_pixels(img: &Rgb32f, frame: &FilmFrame) -> Vec<[f32; 3]> {
    let mut interior = Vec::new();
    if img.width == 0 || img.height == 0 {
        return interior;
    }
    let w = img.width as f64;
    let h = img.height.max(1) as f64;
    for (i, p) in img.data.iter().enumerate() {
        let x = (i % img.width) as f64 / w;
        let y = (i / img.width) as f64 / h;
        if inside_frame(frame, x + 0.5 / w, y + 0.5 / h) {
            interior.push(*p);
        }
    }
    interior
}

/// True when `x`, `y` (normalized) lie inside the frame.
pub fn inside_frame(frame: &FilmFrame, x: f64, y: f64) -> bool {
    let c = &frame.corners;
    let mut inside = false;
    let mut j = 3usize;
    for i in 0..4 {
        let (Some(p), Some(q)) = (c.get(i), c.get(j)) else { return false };
        let (xi, yi) = (p[0], p[1]);
        let (xj, yj) = (q[0], q[1]);
        if (yi > y) != (yj > y) {
            let den = yj - yi;
            if den.abs() > 1e-12 {
                let xhit = (xj - xi) * (y - yi) / den + xi;
                if x < xhit {
                    inside = !inside;
                }
            }
        }
        j = i;
    }
    inside
}

fn measure(img: &Rgb32f, film: &Film) -> FrameStats {
    let empty = FrameStats {
        base: film.base(),
        gamma: film.gamma(),
        exposure: film.exposure,
        rebate_level: 0.0,
        dominant: 0.0,
        bins: vec![0.0; 32],
        rebate_black: false,
        mask_measured: false,
        d_lo: film.d_lo,
        d_hi: film.d_hi,
        fog: film.fog(),
        axis: film.axis(),
        white: film.white(),
        mid_a: film.mid_a,
        mid_b: film.mid_b,
        grade: film.grade,
        shoulder: film.shoulder,
        pivot: film.pivot,
        anchor: film.anchor,
    };
    if img.width == 0 || img.height == 0 {
        return empty;
    }
    let linear = linearize_capture(img, film.capture);
    let linear = linear.as_ref().unwrap_or(img);
    // Margin trims the picture. The mask stays on film outside the detected frame.
    let interior = interior_pixels(linear, &analysis_frame(film));
    let (base, rebate_black, mask_measured) = mask_base(img, film.capture, &film.frame, film.stock, &interior, film.base());
    let rebate_level = luminance_2020(base.map(|v| v as f32)) as f64;
    // A margin that leaves no picture must not replace the print with an empty measurement.
    if interior.len() < 8 {
        return FrameStats { base, rebate_level, rebate_black, mask_measured, ..empty };
    }
    let (gamma, dominant) = match film.intent {
        FilmIntent::Stock => (film.gamma(), dominant_fraction(&interior, &base)),
        FilmIntent::Scene => scene_gamma(&interior, &base, film.gamma()),
    };
    let placed = if film.stock.negative() {
        place_negative(&interior, &base, &gamma, film.grade, true, film.stock == FilmStock::ColorNeg, film.scan, None)
    } else {
        Placement::slide(film.grade, exposure_for(&interior, &base, &gamma, film.stock))
    };
    let bins = density_bins(&interior, if film.stock.negative() { &placed.base } else { &base });
    FrameStats {
        gamma,
        exposure: placed.exposure,
        rebate_level,
        dominant,
        bins,
        rebate_black,
        mask_measured,
        d_lo: placed.d_lo,
        d_hi: placed.d_hi,
        fog: placed.fog,
        axis: placed.axis,
        white: placed.white,
        mid_a: placed.mid_a,
        mid_b: placed.mid_b,
        grade: placed.grade,
        shoulder: placed.shoulder,
        pivot: placed.pivot,
        anchor: placed.anchor,
        base: if film.stock.negative() { placed.base } else { base },
    }
}

/// How many pixels Margin leaves for the picture. Under 8, placement is left as it was.
pub fn trimmed_sample_count(img: &Rgb32f, film: &Film) -> usize {
    let linear = linearize_capture(img, film.capture);
    let img = linear.as_ref().unwrap_or(img);
    interior_pixels(img, &analysis_frame(film)).len()
}

/// Print placement for the base already stored on `film` (a border pick keeps that base).
pub fn place(img: &Rgb32f, film: &Film) -> Placement {
    place_inner(img, film, None)
}

/// Same as [`place`], but the gray axis already on `film` is kept. A gray click uses this.
pub fn place_keeping_axis(img: &Rgb32f, film: &Film) -> Placement {
    place_inner(img, film, Some((film.fog(), film.axis())))
}

fn place_inner(img: &Rgb32f, film: &Film, locked: Option<([f64; 3], [f64; 3])>) -> Placement {
    let linear = linearize_capture(img, film.capture);
    let img = linear.as_ref().unwrap_or(img);
    let interior = interior_pixels(img, &analysis_frame(film));
    if film.stock.negative() {
        place_negative(&interior, &film.base(), &film.gamma(), film.grade, false, film.stock == FilmStock::ColorNeg, film.scan, locked)
    } else {
        Placement::slide(film.grade, exposure_for(&interior, &film.base(), &film.gamma(), film.stock))
    }
}

/// Where the print puts black, white, and the midtone.
#[derive(Clone, Copy, Debug)]
pub struct Placement {
    pub d_lo: f64,
    pub d_hi: f64,
    pub fog: [f64; 3],
    pub axis: [f64; 3],
    pub white: [f64; 3],
    pub grade: f64,
    pub exposure: f64,
    pub shoulder: f64,
    pub mid_a: f64,
    pub mid_b: f64,
    /// Straightened density contrast holds still, and its scene light at [`Self::grade`].
    pub pivot: f64,
    pub anchor: f64,
    /// Black point the print used. The rebate, unless the picture itself is clearer than that
    /// rebate: then it is the clear end of the picture, so a thin subject is a shadow instead of
    /// one black.
    pub base: [f64; 3],
}

impl Placement {
    fn slide(grade: f64, exposure: f64) -> Self {
        Self {
            d_lo: 0.0,
            d_hi: 1.0,
            fog: [0.0; 3],
            axis: [1.0; 3],
            white: [0.0; 3],
            grade,
            exposure,
            shoulder: 0.0,
            mid_a: 0.0,
            mid_b: 0.0,
            pivot: 0.0,
            anchor: 0.0,
            base: [1.0; 3],
        }
    }
}

fn mask_base(
    img: &Rgb32f,
    capture: FilmCapture,
    frame: &FilmFrame,
    stock: FilmStock,
    interior: &[[f32; 3]],
    fallback: [f64; 3],
) -> ([f64; 3], bool, bool) {
    if stock == FilmStock::ColorNeg {
        // The flat band next to the holder, not every bright orange pixel. A sky that matches the
        // rebate stays in the picture. Per-pixel orange below keeps a checkerboard rebate orange;
        // a block mean would mix the junk columns in.
        if let Some(stored) = plateau_border(img).color {
            return (transmission(stored, capture), true, true);
        }
        let (clear, orange, pale) = color_border_lists(img, capture, frame);
        let chosen = mode_color(&clear).or_else(|| mode_color(&orange)).or_else(|| mode_color(&pale));
        return (chosen.unwrap_or(fallback), true, chosen.is_some());
    }
    // Black and white: the bright film outside the frame. A color negative is measured per pixel
    // above, so a dark holder in the same block cannot mix into the orange.
    let edge = if frame.is_full() { Some(rebate_mask(img, stock)) } else { None };
    let mut outside_bw = Vec::new();
    for (seen, measured) in border_areas(img, capture, |i| pixel_on_border(i, img, frame, edge.as_deref())) {
        if stock == FilmStock::BwNeg && !is_light_table(seen) && luminance_2020(seen) > 0.35 {
            outside_bw.push(measured);
        }
    }
    if stock == FilmStock::BwNeg
        && let Some(b) = bright_tail(&outside_bw, 0.90)
    {
        return (b, true, true);
    }
    if matches!(stock, FilmStock::ColorPos | FilmStock::BwPos)
        && let Some(b) = bright_tail(interior, 0.98)
    {
        return (b, false, false);
    }
    bright_tail(interior, 0.97).map(|b| (b, false, false)).unwrap_or(([1.0, 1.0, 1.0], false, false))
}

/// Orange pixels on the rebate, one pixel at a time. A block mean would mix the black holder
/// into the film and the base would follow whichever side of the border the block landed on.
fn color_border_lists(img: &Rgb32f, capture: FilmCapture, frame: &FilmFrame) -> (Vec<[f32; 3]>, Vec<[f32; 3]>, Vec<[f32; 3]>) {
    let edge = if frame.is_full() { Some(rebate_mask(img, FilmStock::ColorNeg)) } else { None };
    let mut clear = Vec::new();
    let mut orange = Vec::new();
    let mut pale = Vec::new();
    if img.width == 0 {
        return (clear, orange, pale);
    }
    if let Some(edge) = &edge {
        let n = edge.iter().filter(|on| **on).count();
        // The orange flood filled the picture. There is no separate rebate in this crop.
        if n.saturating_mul(2) > edge.len() {
            return (clear, orange, pale);
        }
    }
    for (i, px) in img.data.iter().enumerate() {
        if !pixel_on_border(i, img, frame, edge.as_deref()) || is_outside_margin(*px) {
            continue;
        }
        let measured = transmission([px[0] as f64, px[1] as f64, px[2] as f64], capture).map(|v| v as f32);
        if is_mask_orange(*px) {
            orange.push(measured);
            if is_clear_film(*px) {
                clear.push(measured);
            }
        } else if is_pale_rebate(*px) {
            pale.push(measured);
        }
    }
    (clear, orange, pale)
}

fn pixel_on_border(i: usize, img: &Rgb32f, frame: &FilmFrame, edge: Option<&[bool]>) -> bool {
    if let Some(edge) = edge {
        return edge.get(i).copied().unwrap_or(false);
    }
    let w = img.width;
    let h = img.height.max(1);
    if w == 0 {
        return false;
    }
    let x = (i % w) as f64 / w as f64;
    let y = (i / w) as f64 / h as f64;
    !inside_frame(frame, x + 0.5 / w as f64, y + 0.5 / h as f64)
}

#[derive(Clone, Copy)]
struct AreaSum {
    stored: [f64; 3],
    linear: [f64; 3],
    n: u32,
}

/// Blocks of blank film for a black-and-white rebate. Small scans stay one pixel, so a clean
/// test image does not mix the lamp into the rebate. A large scan averages each block first.
fn area_cell(img: &Rgb32f) -> usize {
    let long = img.width.max(img.height);
    if long < 192 { 1 } else { (long / 48).clamp(2, 32) }
}

/// `(stored mean, transmission mean)` of each block that holds border pixels.
fn border_areas(img: &Rgb32f, capture: FilmCapture, border: impl Fn(usize) -> bool) -> Vec<([f32; 3], [f32; 3])> {
    let w = img.width;
    if w == 0 || img.height == 0 || img.data.is_empty() {
        return Vec::new();
    }
    let cell = area_cell(img);
    let cw = w.div_ceil(cell).max(1);
    let ch = img.height.div_ceil(cell).max(1);
    let ncells = cw.saturating_mul(ch);
    if ncells == 0 || ncells > img.data.len().saturating_mul(2).max(1) {
        return Vec::new();
    }
    let mut acc = vec![AreaSum { stored: [0.0; 3], linear: [0.0; 3], n: 0 }; ncells];
    for (i, px) in img.data.iter().enumerate() {
        if !border(i) {
            continue;
        }
        let slot = (i / w / cell).saturating_mul(cw).saturating_add((i % w) / cell);
        let Some(a) = acc.get_mut(slot) else { continue };
        let lin = transmission([px[0] as f64, px[1] as f64, px[2] as f64], capture);
        for c in 0..3 {
            a.stored[c] += px[c] as f64;
            a.linear[c] += lin[c];
        }
        a.n = a.n.saturating_add(1);
    }
    let mut out = Vec::new();
    for a in &acc {
        if a.n == 0 {
            continue;
        }
        let inv = 1.0 / f64::from(a.n);
        out.push((a.stored.map(|v| (v * inv) as f32), a.linear.map(|v| (v * inv) as f32)));
    }
    out
}

/// Blue and green against red. Lower is a stronger orange.
fn orange_score(p: [f32; 3]) -> f32 {
    let r = p[0].max(1e-3);
    (p[2] / r) * (p[1] / r)
}

fn stronger_orange(band: &[[f32; 3]]) -> Vec<[f32; 3]> {
    if band.len() < 8 {
        return band.to_vec();
    }
    let mut scores: Vec<f64> = band.iter().copied().map(|p| orange_score(p) as f64).collect();
    let low = percentile_f64(&mut scores, 0.40).unwrap_or(0.0);
    let high = percentile_f64(&mut scores, 0.60).unwrap_or(0.0);
    if !(high > low * 2.0 && low > 1e-6) {
        return band.to_vec();
    }
    let cut = low * 1.05;
    let orange: Vec<[f32; 3]> = band.iter().copied().filter(|p| (orange_score(*p) as f64) <= cut).collect();
    // A washed lamp beside real film is about half the border, and the orange half is the film.
    // A grain clump is more orange too, and it is the minority. Keeping that minority paints the
    // print with the speck.
    if orange.len() >= band.len() / 2 && orange.len() >= 8 { orange } else { band.to_vec() }
}

fn median_color(px: &[[f32; 3]]) -> [f64; 3] {
    let mut out = [0.0f64; 3];
    for c in 0..3 {
        let mut col: Vec<f64> = px.iter().map(|p| p[c] as f64).collect();
        out[c] = percentile_f64(&mut col, 0.50).unwrap_or(0.0).max(1e-4);
    }
    out
}

fn bright_tail(px: &[[f32; 3]], start_frac: f64) -> Option<[f64; 3]> {
    let mut kept: Vec<[f32; 3]> = px.iter().copied().filter(|p| !is_light_table(*p)).collect();
    if kept.is_empty() {
        return None;
    }
    kept.sort_by(|a, b| luminance_2020(*a).partial_cmp(&luminance_2020(*b)).unwrap_or(std::cmp::Ordering::Equal));
    let start = ((kept.len() as f64) * start_frac).round() as usize;
    let tail = &kept[start.min(kept.len().saturating_sub(1))..];
    let n = tail.len().max(1) as f64;
    let mut s = [0.0f64; 3];
    for p in tail {
        for c in 0..3 {
            s[c] += p[c] as f64;
        }
    }
    Some(s.map(|v| (v / n).max(1e-4)))
}

fn density_rgb(p: [f32; 3], base: &[f64; 3], gamma: &[f64; 3], color: bool, scan: FilmScan) -> [f64; 3] {
    let d = [0, 1, 2].map(|i| {
        let b = base.get(i).copied().unwrap_or(1.0).max(1e-6);
        let g = gamma.get(i).copied().unwrap_or(1.0).clamp(0.05, 6.0);
        let n = (p[i] as f64 / b).clamp(1e-6, 1.0e6);
        ((-n.ln() / LN10 as f64).max(0.0) * g).min(4.0)
    });
    if color { printing_density(d.map(|v| v as f32), scan.dye().map(|v| v as f32)).map(|v| v as f64) } else { d }
}

fn y2020(c: [f64; 3]) -> f64 {
    0.2627 * c[0] + 0.6780 * c[1] + 0.0593 * c[2]
}

/// Map the interior's density onto a print. The mask stays black. Exposure is the scale that
/// puts the middle of the picture on [`crate::tone::GREY`]. Contrast is the slope that puts the
/// body of the picture on the paper; `fit_grade` solves it, and a reprint keeps the grade it was
/// given. The pivot is the density that grade holds still. On a colour negative a near-neutral
/// bright end is gained per channel, fading in toward that end. A saturated bright end is left alone.
fn place_negative(
    interior: &[[f32; 3]],
    base: &[f64; 3],
    gamma: &[f64; 3],
    grade: f64,
    fit_grade: bool,
    color: bool,
    scan: FilmScan,
    locked: Option<([f64; 3], [f64; 3])>,
) -> Placement {
    let grade = if grade.is_finite() { grade.clamp(0.2, 6.0) } else { 1.0 };
    // A thin subject, or a brighter center than the edge the rebate was measured on, sits above
    // the rebate. Clamping there turns the whole shadow into one black and the grain into dots.
    let base = if color { clear_end_base(interior, *base) } else { *base };
    let identity = Placement {
        d_lo: 0.0,
        d_hi: 1.0,
        fog: [0.0; 3],
        axis: [1.0; 3],
        white: [1.0; 3],
        grade,
        exposure: 0.0,
        shoulder: 0.0,
        mid_a: 0.0,
        mid_b: 0.0,
        pivot: 0.0,
        anchor: 0.0,
        base,
    };
    if interior.len() < 8 {
        return identity;
    }
    let dens: Vec<[f64; 3]> = interior.iter().map(|p| density_rgb(*p, &base, gamma, color, scan)).collect();
    let ys: Vec<f64> = dens.iter().copied().map(y2020).collect();
    let mut for_hi = ys.clone();
    let mut rough_hi = percentile_f64(&mut for_hi, 0.995).unwrap_or(1.0);
    if rough_hi < 0.15 {
        rough_hi = 0.15;
    }
    // Clear film that leaked into the frame is density ~0. It must not become the gray axis.
    // The exposure median keeps a thin body that sits just above zero: dropping that body is
    // what printed a thin negative black. A separate spike of exact zeros is not the middle.
    let floor = (rough_hi * 0.04).max(0.02);
    let axis_sample: Vec<[f64; 3]> = dens.iter().copied().zip(ys.iter()).filter(|(_, y)| **y > floor).map(|(d, _)| d).collect();
    let axis_sample = if axis_sample.len() < 8 { dens.clone() } else { axis_sample };
    let (fog, axis) = if let Some(locked) = locked {
        locked
    } else if color {
        gray_axis(&axis_sample)
    } else {
        ([0.0; 3], [1.0; 3])
    };
    let mut corrected = Vec::with_capacity(dens.len());
    for d in &dens {
        corrected.push(straighten(*d, fog, axis));
    }
    // A dense picture never reaches clear film. The empty density under it is not a shadow:
    // leaving it there prints the whole frame gray. The toe stays, so the darkest grain is not a cliff.
    let lift = shadow_lift(&corrected);
    if lift > 0.0 {
        for d in &mut corrected {
            for c in d.iter_mut() {
                *c = (*c - lift).max(0.0);
            }
        }
    }
    let mut hi_src: Vec<f64> = corrected.iter().copied().map(y2020).collect();
    let d_hi = percentile_f64(&mut hi_src, 0.99).unwrap_or(1.0).max(0.15);
    let white = if color { balanced_white(&corrected, d_hi) } else { [d_hi, d_hi, d_hi] };
    let grade = if fit_grade { grade_for(&corrected, white) } else { grade };
    let (med, _, peak) = luminance_ends(&corrected, white, grade, &mut hi_src);
    // One scale. It puts the middle on middle grey. The bright end is wherever the grade left it.
    let exposure = (f64::from(crate::tone::GREY) / med).log2().clamp(-6.0, 6.0);
    let placed_peak = peak * (2.0f64).powf(exposure);
    let shoulder = if fit_grade { paper_shoulder(placed_peak) } else { 0.0 };
    let slope = (2.0f64).powf(exposure);
    let mut pivot_ys: Vec<f64> = corrected.iter().copied().map(y2020).collect();
    let pivot = density_median(&mut pivot_ys).max(0.0);
    let shared = white.into_iter().fold(0.0, f64::max).max(0.05);
    let anchor = density_to_light(pivot, shared, FILM_GAMMA / grade);
    let mut printed = Vec::with_capacity(corrected.len());
    for d in &corrected {
        let h = balanced_light(*d, white, FILM_GAMMA / grade);
        printed.push(h.map(|v| (v * slope).max(0.0)));
    }
    let (mid_a, mid_b) = midtone_ab(&printed);
    Placement { d_lo: lift, d_hi, fog, axis, white, grade, exposure, shoulder, mid_a, mid_b, pivot, anchor, base }
}

/// Black point for a colour negative. The rebate, unless a large part of the picture is clearer
/// than that rebate. The clear end of the picture is then the black point, and it is a real
/// colour from that end, not one channel from a lamp and another from the subject.
fn clear_end_base(interior: &[[f32; 3]], rebate: [f64; 3]) -> [f64; 3] {
    if interior.len() < 32 {
        return rebate;
    }
    let rebate_y = y2020(rebate);
    if !(rebate_y.is_finite() && rebate_y > 1e-4) {
        return rebate;
    }
    let film: Vec<[f32; 3]> = interior.iter().copied().filter(|p| !is_light_table(*p) && !is_clipped_lamp(*p)).collect();
    if film.len() < 32 {
        return rebate;
    }
    let mut clear = 0usize;
    for p in &film {
        let y = y2020([p[0] as f64, p[1] as f64, p[2] as f64]);
        if y > rebate_y * 1.005 {
            clear += 1;
        }
    }
    // Grain on a normal negative crosses the rebate. A thin subject is a quarter of the picture.
    if clear.saturating_mul(4) < film.len() {
        return rebate;
    }
    let mut ys: Vec<f64> = film.iter().map(|p| y2020([p[0] as f64, p[1] as f64, p[2] as f64])).collect();
    let Some(tip_y) = percentile_f64(&mut ys, 0.97) else { return rebate };
    if !(tip_y.is_finite() && tip_y > rebate_y) {
        return rebate;
    }
    let band: Vec<[f32; 3]> = film
        .iter()
        .copied()
        .filter(|p| {
            let y = y2020([p[0] as f64, p[1] as f64, p[2] as f64]);
            y >= tip_y * 0.97 && y <= tip_y * 1.04
        })
        .collect();
    let color = if band.len() >= 8 {
        median_color(&band)
    } else {
        let mut tip = rebate;
        for c in 0..3 {
            let mut col: Vec<f64> = film.iter().map(|p| p[c] as f64).collect();
            if let Some(v) = percentile_f64(&mut col, 0.97) {
                tip[c] = v;
            }
        }
        tip
    };
    let mut out = rebate;
    for c in 0..3 {
        let cap = rebate[c] * 4.0;
        // A hair clearer than the clear film. The subject that *is* that film then has a toe,
        // so exposure can lift it. Sitting the base on the subject itself clamps it to black.
        if rebate[c] > 1e-6 && color[c] > rebate[c] && cap.is_finite() {
            out[c] = (color[c] * 1.06).min(cap);
        }
    }
    out
}

/// Contrast for the picture in hand. A body with its own bright end (the 90th percentile sits
/// above the middle) is placed on the paper, and a brighter speck may pass 1. A body that is one
/// tone, with the only separation in a small bright patch, keeps that patch as the bright end:
/// stretching the flat body would pin the grade and drop the patch to grey. A harder grade raises
/// the ratio. The search stops at the grade limits.
fn grade_for(corrected: &[[f64; 3]], white: [f64; 3]) -> f64 {
    let mut dens: Vec<f64> = corrected.iter().copied().map(y2020).collect();
    let d50 = percentile_f64(&mut dens, 0.50).unwrap_or(0.0);
    let d90 = percentile_f64(&mut dens, 0.90).unwrap_or(d50);
    let body = d90 > d50 + 0.06;
    let grey = f64::from(crate::tone::GREY);
    let target = if body { BODY_PRINT / grey } else { 1.0 / grey };
    // The kernel caps 10^(D/γ) at 10^8. Past that the bright end stacks on one value and a
    // harder grade looks flatter, so the search must stay on the straight line.
    let d_hi = white.into_iter().fold(0.0_f64, f64::max).max(0.15);
    let grade_max = (FILM_GAMMA * 6.0 / d_hi).clamp(0.2, 6.0);
    let mut ys = Vec::with_capacity(corrected.len());
    let ratio_at = |grade: f64, ys: &mut Vec<f64>| {
        let (med, p90, p98) = luminance_ends(corrected, white, grade, ys);
        (if body { p90 } else { p98 }) / med
    };
    if ratio_at(0.2, &mut ys) >= target {
        return 0.2;
    }
    if ratio_at(grade_max, &mut ys) <= target {
        return grade_max;
    }
    let (mut lo, mut hi) = (0.2, grade_max);
    for _ in 0..24 {
        let mid = 0.5 * (lo + hi);
        if ratio_at(mid, &mut ys) > target {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    0.5 * (lo + hi)
}

/// Median, 90th percentile and 98th percentile of print luminance at `grade`, before exposure.
fn luminance_ends(corrected: &[[f64; 3]], white: [f64; 3], grade: f64, ys: &mut Vec<f64>) -> (f64, f64, f64) {
    let gamma = FILM_GAMMA / grade.clamp(0.2, 6.0);
    ys.clear();
    for d in corrected {
        ys.push(y2020(balanced_light(*d, white, gamma)));
    }
    let med = exposure_median(ys);
    let body = percentile_f64(ys, 0.90).unwrap_or(med).max(med);
    let peak = percentile_f64(ys, 0.98).unwrap_or(body).max(body);
    (med, body, peak)
}

/// Density removed under a picture that sits far above clear film. Zero when the frame already
/// reaches down toward the mask, so a thin subject is not clipped to a black cliff.
fn shadow_lift(corrected: &[[f64; 3]]) -> f64 {
    if corrected.len() < 16 {
        return 0.0;
    }
    let mut ys: Vec<f64> = corrected.iter().copied().map(y2020).collect();
    let p05 = percentile_f64(&mut ys, 0.05).unwrap_or(0.0);
    let p50 = percentile_f64(&mut ys, 0.50).unwrap_or(p05);
    let hi = percentile_f64(&mut ys, 0.99).unwrap_or(p05).max(1e-3);
    if !(p05.is_finite() && hi.is_finite()) || p05 <= SHADOW_TOE || p05 / hi < SHADOW_GAP {
        return 0.0;
    }
    let cap = (p50 - 0.15).max(0.0);
    (p05 - SHADOW_TOE).clamp(0.0, cap)
}

/// Median straightened density. An exact-zero spike is unexposed film, same rule as
/// [`exposure_median`], so the pivot sits on the picture.
fn density_median(ys: &mut [f64]) -> f64 {
    if ys.is_empty() {
        return 0.0;
    }
    ys.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let zeros = ys.iter().take_while(|y| **y < 1e-4).count();
    if zeros.saturating_mul(2) > ys.len() && ys.len() - zeros >= 8 {
        percentile_f64(&mut ys[zeros..], 0.50).unwrap_or(0.0)
    } else {
        percentile_f64(ys, 0.50).unwrap_or(0.0)
    }
}

/// Middle of the print. An exact-zero spike with a real picture above it is unexposed film, not
/// the middle: fitting it pins contrast at the softest grade and exposure at the clamp, and the
/// grain prints as dots. A body that is only just above zero stays, so a thin negative is not
/// printed from its dense specks alone.
fn exposure_median(ys: &mut [f64]) -> f64 {
    let fallback = f64::from(crate::tone::GREY);
    if ys.is_empty() {
        return fallback;
    }
    let all = percentile_f64(ys, 0.50).unwrap_or(fallback).max(1e-5);
    if ys.len() < 16 {
        return all;
    }
    let zeros = ys.iter().take_while(|y| **y < 1e-4).count();
    if zeros.saturating_mul(2) <= ys.len() {
        return all;
    }
    let body = &mut ys[zeros..];
    if body.len() < 8 {
        return all;
    }
    let med = percentile_f64(body, 0.50).unwrap_or(all);
    if med > 0.02 { med.max(1e-5) } else { all }
}

/// Shoulder that prints `peak` at 1 and leaves every value below the knee alone.
///
/// The kernel's roll-off is `s + (v − s) / (1 + (v − s) / s)`, which equals `2s − s²/v` and
/// passes through `(peak, 1)` at `s = peak − √(peak(peak − 1))`. That knee stays above middle
/// grey for any peak above 1, so the exposure scale is unchanged.
fn paper_shoulder(peak: f64) -> f64 {
    if !peak.is_finite() || peak <= 1.02 {
        return 0.0;
    }
    let s = peak - (peak * (peak - 1.0)).sqrt();
    if s.is_finite() { s.clamp(0.0, 1.5) } else { 0.0 }
}

/// Channel offsets of the bright end, added to `d_hi`. A neutral end stays `[d_hi, d_hi, d_hi]`.
fn balanced_white(corrected: &[[f64; 3]], d_hi: f64) -> [f64; 3] {
    let mut ys: Vec<f64> = corrected.iter().copied().map(y2020).collect();
    let cut = percentile_f64(&mut ys, 0.99).unwrap_or(d_hi);
    let mut s = [0.0; 3];
    let mut n = 0.0;
    for d in corrected {
        if y2020(*d) + 1e-12 >= cut {
            for c in 0..3 {
                s[c] += d[c];
            }
            n += 1.0;
        }
    }
    if n < 4.0 {
        return [d_hi, d_hi, d_hi];
    }
    let mean = s.map(|v| v / n);
    let y = y2020(mean).max(1e-4);
    let spread = mean[0].max(mean[1]).max(mean[2]) - mean[0].min(mean[1]).min(mean[2]);
    // A saturated bright end is the picture (a blue sky). A small cast is the white point.
    if spread / y > 0.25 {
        return [d_hi, d_hi, d_hi];
    }
    mean.map(|v| d_hi + (v - y))
}

/// Scene light of a straightened density. Same white and the same fade as [`FilmKernel::apply`].
fn balanced_light(straight: [f64; 3], white: [f64; 3], gamma: f64) -> [f64; 3] {
    let shared = white.into_iter().fold(0.0, f64::max).max(0.05);
    let t = smooth(shared as f32 * 0.62, shared as f32 * 0.92, y2020(straight) as f32) as f64;
    [0, 1, 2].map(|i| {
        let channel = white[i].clamp(shared - 0.45, shared).max(0.05);
        let v = density_to_light(straight[i], shared, gamma);
        let at = density_to_light(channel, shared, gamma).max(1e-4);
        (v * (1.0 + (1.0 / at - 1.0) * t)).clamp(0.0, f64::from(HEADROOM))
    })
}

fn density_to_light(straight: f64, white: f64, gamma: f64) -> f64 {
    let g = if gamma.is_finite() { gamma.max(0.05) } else { FILM_GAMMA };
    let w = if white.is_finite() { white.max(0.05) } else { 1.0 };
    let scale = 10f64.powf((w / g).min(8.0)) - 1.0;
    if scale < 1e-6 {
        return 0.0;
    }
    let e = (straight.max(0.0) / g).min(8.0);
    ((10f64.powf(e) - 1.0) / scale).clamp(0.0, f64::from(HEADROOM))
}

fn straighten(d: [f64; 3], fog: [f64; 3], axis: [f64; 3]) -> [f64; 3] {
    [0, 1, 2].map(|i| ((d[i] - fog[i]).max(0.0) / axis[i].max(0.05)).min(4.0))
}

/// `D_c = fog_c + axis_c * D_green` for a cast whose sign holds from the 20th to the 80th
/// percentile. The slope is a least-squares line through bin medians of that span, not the two
/// ends alone. A hue that is only in the shadows, or only in the highlights, is left alone, and
/// a cloud that is already neutral is not given a noisy slope. Fog is never negative, so the
/// mask stays black.
fn gray_axis(picture: &[[f64; 3]]) -> ([f64; 3], [f64; 3]) {
    let mut fog = [0.0; 3];
    let mut axis = [1.0; 3];
    if picture.len() < 16 {
        return (fog, axis);
    }
    let mut order = picture.to_vec();
    order.sort_by(|a, b| a[1].partial_cmp(&b[1]).unwrap_or(std::cmp::Ordering::Equal));
    let lo = window_median(&order, 0.20);
    let hi = window_median(&order, 0.80);
    let gspan = hi[1] - lo[1];
    if gspan < 0.05 {
        return (fog, axis);
    }
    let bins = bin_medians(&order);
    for c in [0, 2] {
        let d_lo = lo[c] - lo[1];
        let d_hi = hi[c] - hi[1];
        let both_small = d_lo.abs() <= 0.03 && d_hi.abs() <= 0.03;
        let same = d_lo.signum() == d_hi.signum() && d_lo.abs() > 0.03 && d_hi.abs() > 0.03;
        if both_small || !same {
            continue;
        }
        let (mut g, mut f) = fit_line(&bins, c);
        if !g.is_finite() || !f.is_finite() {
            continue;
        }
        if f < 0.0 {
            f = 0.0;
            g = origin_gain(&bins, c);
        }
        // Clamping a slope that does not fit the two ends paints the other end the opposite color.
        // A cast that fades out is not the axis: keep only the constant both ends share.
        if !(0.25..=3.2).contains(&g) || f > 0.8 {
            let shared = if d_lo > 0.0 && d_hi > 0.0 { d_lo.min(d_hi) } else { 0.0 };
            fog[c] = shared.clamp(0.0, 0.8);
            axis[c] = 1.0;
            continue;
        }
        fog[c] = f;
        axis[c] = g;
    }
    (fog, axis)
}

/// Median colour in each of 16 slices of the central 80% of green density.
fn bin_medians(sorted: &[[f64; 3]]) -> Vec<[f64; 3]> {
    let n = sorted.len();
    let start = ((n as f64) * 0.10).round() as usize;
    let end = ((n as f64) * 0.90).round() as usize;
    let band = sorted.get(start.min(n.saturating_sub(1))..end.max(start + 1).min(n)).unwrap_or(sorted);
    let mut out = Vec::new();
    let bins = 16usize;
    for i in 0..bins {
        let a = band.len() * i / bins;
        let b = band.len() * (i + 1) / bins;
        if b.saturating_sub(a) < 4 {
            continue;
        }
        let Some(slice) = band.get(a..b) else { continue };
        let mut win = slice.to_vec();
        out.push(median_rgb(&mut win));
    }
    out
}

/// Least squares `D_c = intercept + slope · D_green` on bin medians.
fn fit_line(bins: &[[f64; 3]], channel: usize) -> (f64, f64) {
    if bins.len() < 2 {
        return (1.0, 0.0);
    }
    let n = bins.len() as f64;
    let (mut mg, mut mc) = (0.0, 0.0);
    for p in bins {
        mg += p[1];
        mc += p.get(channel).copied().unwrap_or(0.0);
    }
    mg /= n;
    mc /= n;
    let (mut var, mut cov) = (0.0, 0.0);
    for p in bins {
        let dg = p[1] - mg;
        var += dg * dg;
        cov += dg * (p.get(channel).copied().unwrap_or(0.0) - mc);
    }
    if var < 1e-8 {
        return (1.0, 0.0);
    }
    let g = cov / var;
    (g, mc - g * mg)
}

fn window_median(sorted: &[[f64; 3]], frac: f64) -> [f64; 3] {
    let n = sorted.len();
    let i = ((n - 1) as f64 * frac).round() as usize;
    let rad = (n / 20).max(4);
    let a = i.saturating_sub(rad);
    let b = (i + rad + 1).min(n);
    let mut win = sorted.get(a..b).unwrap_or(sorted).to_vec();
    median_rgb(&mut win)
}

fn origin_gain(bins: &[[f64; 3]], channel: usize) -> f64 {
    let mut num = 0.0;
    let mut den = 0.0;
    for p in bins {
        let c = p.get(channel).copied().unwrap_or(0.0);
        num += c * p[1];
        den += p[1] * p[1];
    }
    if den > 1e-6 { num / den } else { 1.0 }
}

fn median_rgb(px: &mut [[f64; 3]]) -> [f64; 3] {
    let mut out = [0.0; 3];
    for c in 0..3 {
        let mut col: Vec<f64> = px.iter().map(|p| p[c]).collect();
        out[c] = percentile_f64(&mut col, 0.50).unwrap_or(0.0);
    }
    out
}

fn midtone_ab(printed: &[[f64; 3]]) -> (f64, f64) {
    if printed.is_empty() {
        return (0.0, 0.0);
    }
    let ys: Vec<f64> = printed.iter().copied().map(y2020).collect();
    let mut for_mid = ys.clone();
    let mid = percentile_f64(&mut for_mid, 0.50).unwrap_or(0.18);
    let mut acc_a = 0.0;
    let mut acc_b = 0.0;
    let mut n = 0.0;
    for (rgb, y) in printed.iter().copied().zip(ys.iter()) {
        if (y - mid).abs() > 0.06 {
            continue;
        }
        let (a, b) = lab_ab(rgb);
        acc_a += a;
        acc_b += b;
        n += 1.0;
    }
    if n >= 8.0 {
        return (acc_a / n, acc_b / n);
    }
    let mut best = printed.first().copied().unwrap_or([mid, mid, mid]);
    let mut best_d = f64::MAX;
    for (rgb, y) in printed.iter().copied().zip(ys.iter()) {
        let d = (y - mid).abs();
        if d < best_d {
            best_d = d;
            best = rgb;
        }
    }
    lab_ab(best)
}

fn lab_ab(rgb: [f64; 3]) -> (f64, f64) {
    let xyz = lightcraft_color::REC2020.to_xyz().apply(rgb);
    let f = |t: f64| {
        if t > 0.008856 { t.cbrt() } else { (7.787037 * t) + (16.0 / 116.0) }
    };
    let fx = f(xyz[0] / 0.95047);
    let fy = f(xyz[1]);
    let fz = f(xyz[2] / 1.08883);
    (500.0 * (fx - fy), 200.0 * (fy - fz))
}

/// The cast Auto removed, at a mid green density, and the printed midtone's a* b*.
/// Positive green is red density short of green (the print was green). Positive blue is extra
/// blue density. `a*` and `b*` are the midtone after that, not a live reading of the trims.
pub fn cast_readout(f: &Film) -> String {
    let d = 0.45;
    let green = -(f.fog_r + (f.axis_r - 1.0) * d);
    let blue = f.fog_b + (f.axis_b - 1.0) * d;
    format!("cast green {green:+.2}, blue {blue:+.2} · a* {:+.1} b* {:+.1}", f.mid_a, f.mid_b)
}

fn scene_gamma(interior: &[[f32; 3]], base: &[f64; 3], current: [f64; 3]) -> ([f64; 3], f64) {
    let dominant = dominant_fraction(interior, base);
    if dominant > 0.75 || interior.len() < 8 {
        return (current, dominant);
    }
    let mut lows = Vec::new();
    for p in interior {
        let n = norm_pix(*p, base);
        let chroma = chroma_of(n);
        if chroma < 0.08 {
            lows.push((n, luminance_2020(*p)));
        }
    }
    if lows.len() < 8 {
        return (current, dominant);
    }
    lows.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    let third = (lows.len() / 3).max(1);
    let shadow = mean_pix(&lows[..third]);
    let highlight = mean_pix(&lows[lows.len() - third..]);
    let gamma = gamma_from_picks(*base, &[(shadow, 1.0), (highlight, 1.0)], current[1]);
    (gamma, dominant)
}

fn dominant_fraction(interior: &[[f32; 3]], base: &[f64; 3]) -> f64 {
    if interior.is_empty() {
        return 0.0;
    }
    let mut hot = 0.0;
    for p in interior {
        if chroma_of(norm_pix(*p, base)) > 0.25 {
            hot += 1.0;
        }
    }
    hot / interior.len() as f64
}

fn norm_pix(p: [f32; 3], base: &[f64; 3]) -> [f64; 3] {
    [0, 1, 2].map(|i| {
        let b = base.get(i).copied().unwrap_or(1.0).max(1e-6);
        (p[i] as f64 / b).clamp(1e-6, 1.0e6)
    })
}

fn chroma_of(n: [f64; 3]) -> f64 {
    let max = n[0].max(n[1]).max(n[2]);
    let min = n[0].min(n[1]).min(n[2]);
    if max <= 1e-6 { 0.0 } else { (max - min) / max }
}

fn mean_pix(pts: &[([f64; 3], f32)]) -> [f64; 3] {
    if pts.is_empty() {
        return [1.0, 1.0, 1.0];
    }
    let n = pts.len() as f64;
    let mut s = [0.0; 3];
    for (p, _) in pts {
        for i in 0..3 {
            s[i] += p[i];
        }
    }
    s.map(|v| v / n)
}

fn exposure_for(interior: &[[f32; 3]], base: &[f64; 3], gamma: &[f64; 3], stock: FilmStock) -> f64 {
    if interior.is_empty() {
        return 0.0;
    }
    let negative = stock.negative();
    let mut scenes = Vec::with_capacity(interior.len());
    for p in interior {
        let mut scene = [0.0f64; 3];
        for i in 0..3 {
            let b = base.get(i).copied().unwrap_or(1.0).max(1e-6);
            let g = gamma.get(i).copied().unwrap_or(1.0).clamp(0.05, 4.0);
            let n = (p[i] as f64 / b).clamp(1e-6, 1.0e6);
            scene[i] = if negative {
                let d = (-n.ln() / LN10 as f64).clamp(-2.0, 4.0);
                (10f64.powf(g * d) - 1.0).max(0.0)
            } else {
                n.powf(g)
            };
        }
        let y = 0.2627 * scene[0] + 0.6780 * scene[1] + 0.0593 * scene[2];
        scenes.push(y);
    }
    let med = percentile_f64(&mut scenes, 0.50).unwrap_or(0.18);
    if med < 1e-5 {
        return 0.0;
    }
    (0.18 / med).log2().clamp(-6.0, 6.0)
}

fn density_bins(interior: &[[f32; 3]], base: &[f64; 3]) -> Vec<f32> {
    let mut bins = vec![0.0f32; 32];
    if interior.is_empty() {
        return bins;
    }
    for p in interior {
        let n = norm_pix(*p, base);
        let y = 0.2627 * n[0] + 0.6780 * n[1] + 0.0593 * n[2];
        let d = (-y.max(1e-6).ln() / LN10 as f64).clamp(0.0, 2.5);
        let i = ((d / 2.5) * 31.0).round() as usize;
        if let Some(slot) = bins.get_mut(i.min(31)) {
            *slot += 1.0;
        }
    }
    let max = bins.iter().copied().fold(0.0f32, f32::max);
    if max > 0.0 {
        for b in &mut bins {
            *b /= max;
        }
    }
    bins
}

fn percentile_f64(v: &mut [f64], p: f32) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let i = ((v.len() - 1) as f32 * p.clamp(0.0, 1.0)).round() as usize;
    v.get(i.min(v.len() - 1)).copied()
}

/// One frame after a roll has shared mask color and contrast. Exposure stays per frame.
/// `drift` is how many stops this frame's rebate sits from the roll's median (a shutter change).
#[derive(Clone, Debug)]
pub struct SharedFrame {
    pub base: [f64; 3],
    pub gamma: [f64; 3],
    pub exposure: f64,
    pub fog: [f64; 3],
    /// Copied as-is. Density is already above each frame's own rebate, so the slope is not scaled.
    pub axis: [f64; 3],
    pub drift: f64,
}

/// Share mask color and contrast across a roll. Each frame keeps its own rebate level, so a
/// different shutter does not repaint the roll, and its own exposure.
pub fn share_roll(frames: &[FrameStats]) -> Vec<SharedFrame> {
    if frames.is_empty() {
        return Vec::new();
    }
    let measured: Vec<_> = frames.iter().filter(|f| f.mask_measured).collect();
    let mut levels: Vec<f64> = measured.iter().map(|f| f.rebate_level.max(1e-6)).collect();
    let median_level = percentile_f64(&mut levels, 0.50).unwrap_or(1.0).max(1e-6);
    let ratios: Vec<[f64; 3]> = measured
        .iter()
        .map(|f| {
            let g = f.base[1].max(1e-6);
            [f.base[0] / g, 1.0, f.base[2] / g]
        })
        .collect();
    let ratio = median_ratio(&ratios);
    let gamma = median_gamma(frames);
    let fog = median_channels(frames, |f| f.fog);
    let axis = median_channels(frames, |f| f.axis);
    frames
        .iter()
        .map(|f| {
            let g = f.base[1].max(1e-6);
            SharedFrame {
                base: if f.mask_measured { [ratio[0] * g, g, ratio[2] * g] } else { f.base },
                gamma,
                exposure: f.exposure,
                fog,
                axis,
                drift: if f.mask_measured { (f.rebate_level.max(1e-6) / median_level).log2() } else { 0.0 },
            }
        })
        .collect()
}

fn median_ratio(ratios: &[[f64; 3]]) -> [f64; 3] {
    let mut out = [1.0, 1.0, 1.0];
    for c in 0..3 {
        let mut v: Vec<f64> = ratios.iter().map(|r| r[c]).collect();
        out[c] = percentile_f64(&mut v, 0.50).unwrap_or(1.0);
    }
    out
}

fn median_channels(frames: &[FrameStats], get: impl Fn(&FrameStats) -> [f64; 3]) -> [f64; 3] {
    let mut out = [0.0; 3];
    for c in 0..3 {
        let mut v: Vec<f64> = frames.iter().map(|f| get(f)[c]).collect();
        out[c] = percentile_f64(&mut v, 0.50).unwrap_or(if c == 1 { 0.0 } else { 1.0 });
    }
    out
}

fn median_gamma(frames: &[FrameStats]) -> [f64; 3] {
    let mut out = [1.0, 1.0, 1.0];
    for c in 0..3 {
        let mut v: Vec<f64> = frames.iter().map(|f| f.gamma[c]).collect();
        out[c] = percentile_f64(&mut v, 0.50).unwrap_or(1.0);
    }
    out
}

/// Stops of rebate drift past which a roll still converts, and reports the frame.
pub const DRIFT_WARN: f64 = 0.5;

#[cfg(test)]
mod tests {
    use super::*;
    use lightcraft_develop::{FilmCapture, FilmScan};

    /// −1..1, stable for a given pixel. Not a film measurement.
    fn rebate_grain(x: usize, y: usize, channel: usize) -> f32 {
        let n = x.wrapping_mul(374761393) ^ y.wrapping_mul(668265263) ^ channel.wrapping_mul(2246822519);
        let n = n.wrapping_mul(3266489917);
        let u = (n >> 16) & 0xffff;
        u as f32 / 32768.0 - 1.0
    }

    #[test]
    fn margin_drops_the_frame_edge_and_keeps_the_mask_and_grade() {
        let base = [0.90f64, 0.42, 0.16];
        let mut film = Film { enabled: true, scan: FilmScan::Flat, grade: 1.4, ..Film::default() };
        film.set_base(base);
        let leak = [0.95f32, 0.20, 0.05];
        let picture = base.map(|v| (v * 0.35) as f32);
        let img = Rgb32f::from_fn(40, 40, |x, y| if x < 8 || y < 8 || x >= 32 || y >= 32 { leak } else { picture });
        let open = place(&img, &film);
        film.inset = 25.0;
        let tight = place(&img, &film);
        assert!((open.grade - 1.4).abs() < 1e-9 && (tight.grade - open.grade).abs() < 1e-9, "margin replaced contrast");
        let moved = (open.exposure - tight.exposure).abs() > 0.05
            || (open.d_hi - tight.d_hi).abs() > 0.02
            || (open.mid_a - tight.mid_a).abs() > 1.0
            || (open.mid_b - tight.mid_b).abs() > 1.0
            || open.fog != tight.fog
            || open.axis != tight.axis;
        assert!(moved, "margin left the print unchanged: {open:?} vs {tight:?}");
        let mut full = film.clone();
        full.inset = 0.0;
        let mask_open = super::measure(&img, &full).base;
        let mask_tight = super::measure(&img, &film).base;
        assert!(
            (mask_open[0] - mask_tight[0]).abs() < 1e-5 && (mask_open[2] - mask_tight[2]).abs() < 1e-5,
            "margin moved the mask {mask_open:?} {mask_tight:?}"
        );
        let tiny = Rgb32f::filled(10, 10, picture);
        let mut kept = film.clone();
        kept.inset = 40.0;
        kept.exposure = 1.25;
        let stats = super::measure(&tiny, &kept);
        assert!((stats.exposure - 1.25).abs() < 1e-9, "a margin with no picture left reset the print");
    }

    #[test]
    fn the_mask_is_black_and_a_neutral_stays_neutral() {
        let base = [0.80, 0.50, 0.25];
        let mut f = Film { enabled: true, scan: FilmScan::Flat, ..Film::default() };
        f.set_base(base);
        let k = FilmKernel::from_film(&f);
        let mask = k.apply(base.map(|v| v as f32));
        assert!(mask.iter().all(|c| c.abs() < 1e-5), "clear film {mask:?}");
        let neutral = k.apply(base.map(|v| (v * 0.5) as f32));
        let spread = neutral[0].max(neutral[1]).max(neutral[2]) - neutral[0].min(neutral[1]).min(neutral[2]);
        assert!(spread < 1e-4, "neutral {neutral:?}");
        // Half the base is density −log10(0.5). Scene light is 10^(D/γ) − 1, not the density itself.
        let density = -(0.5f32).log10();
        let gamma = FILM_GAMMA as f32;
        let scale = 10f32.powf(1.0 / gamma) - 1.0;
        let expect = (10f32.powf(density / gamma) - 1.0) / scale;
        assert!((neutral[1] - expect).abs() < 1e-4, "{neutral:?} expected {expect}");
        let dense = k.apply(base.map(|v| (v * 0.25) as f32));
        assert!(dense[1] > neutral[1], "a denser negative is a brighter positive");
        // Density past the reference white stays above the white, for the scene tone curve.
        let at_white = k.apply(base.map(|v| (v * 10f64.powf(-1.0)) as f32));
        let above = k.apply(base.map(|v| (v * 10f64.powf(-1.4)) as f32));
        let y = |p: [f32; 3]| 0.2627 * p[0] + 0.6780 * p[1] + 0.0593 * p[2];
        assert!(y(above) > y(at_white) + 0.05, "highlights are not clipped {above:?} vs {at_white:?}");
    }

    #[test]
    fn zero_cast_and_a_disabled_stage_leave_the_pixel() {
        let mut f = Film { enabled: true, ..Film::default() };
        f.set_base([0.8, 0.5, 0.25]);
        let k = FilmKernel::from_film(&f);
        assert_eq!(k.balance, [1.0, 1.0, 1.0]);
        assert_eq!(k.shadow, [1.0, 1.0, 1.0]);
        let px = [0.4f32, 0.25, 0.125];
        let once = k.apply(px);
        f.temp = 0.0;
        f.tint = 0.0;
        assert!((FilmKernel::from_film(&f).apply(px)[0] - once[0]).abs() < 1e-6);
        let mut img = Rgb32f::filled(2, 2, px);
        let before = img.data.clone();
        apply(&mut img, &Film::default());
        assert_eq!(img.data, before);
    }

    #[test]
    fn slides_do_not_invert_and_black_and_white_is_one_channel() {
        let mut f = Film { enabled: true, stock: FilmStock::ColorPos, ..Film::default() };
        f.set_base([1.0, 1.0, 1.0]);
        let k = FilmKernel::from_film(&f);
        let bright = k.apply([0.8, 0.8, 0.8]);
        let dark = k.apply([0.2, 0.2, 0.2]);
        assert!(bright[1] > dark[1], "a slide stays a positive");
        f.stock = FilmStock::BwNeg;
        f.set_base([0.9, 0.9, 0.9]);
        let g = FilmKernel::from_film(&f).apply([0.2, 0.5, 0.8]);
        assert!((g[0] - g[1]).abs() < 1e-5 && (g[1] - g[2]).abs() < 1e-5, "{g:?}");
        // A paper aim left on from a color frame is not shown for black-and-white, and must not tint it.
        f.print = lightcraft_develop::FilmPrint::Portrait;
        f.temp = 40.0;
        f.tint = 30.0;
        f.presat = 50.0;
        f.shadow_temp = -20.0;
        let paper = FilmKernel::from_film(&f);
        assert_eq!(paper.balance, [1.0, 1.0, 1.0]);
        assert_eq!(paper.presat, 1.0);
        let printed = paper.apply([0.2, 0.5, 0.8]);
        assert!(
            (printed[0] - printed[1]).abs() < 1e-5 && (printed[1] - printed[2]).abs() < 1e-5,
            "a leftover color look tinted black and white {printed:?}"
        );
        assert!((f.temp - 40.0).abs() < 1e-9, "the color trim stays on the settings");
    }

    #[test]
    fn the_kernel_word_layout_matches_film_k() {
        let mut f = Film { enabled: true, stock: FilmStock::BwNeg, capture: FilmCapture::Gamma22, ..Film::default() };
        f.set_base([0.8, 0.5, 0.25]);
        let k = FilmKernel::from_film(&f);
        let w = k.words();
        assert_eq!(w.len(), 46);
        assert_eq!(w[0], (0.8f32).to_bits());
        assert_eq!(w[23], 1 | 2, "negative and mono flags");
        assert_eq!(f32::from_bits(w[24]), 2.2);
        assert_eq!(f32::from_bits(w[25]), 0.0);
        assert_eq!(f32::from_bits(w[26]), 1.0);
    }

    #[test]
    fn a_picture_quad_becomes_a_crop_and_a_full_scan_does_not() {
        use lightcraft_geom::Affine;
        let full = FilmFrame::default();
        assert!(crop_for_frame(&full, 200.0, 100.0).is_none());
        let inset = FilmFrame { corners: [[0.10, 0.20], [0.80, 0.20], [0.80, 0.90], [0.10, 0.90]] };
        let crop = crop_for_frame(&inset, 200.0, 100.0).expect("inset");
        assert!(crop.angle.abs() < 1e-6, "{}", crop.angle);
        assert!((crop.rect.x0 - 0.10).abs() < 1e-6 && (crop.rect.y0 - 0.20).abs() < 1e-6);
        assert!((crop.rect.x1 - 0.80).abs() < 1e-6 && (crop.rect.y1 - 0.90).abs() < 1e-6);

        let (w, h) = (400.0, 300.0);
        let turn = Affine::rotate_about(10.0_f64.to_radians(), lightcraft_geom::Point::new(w * 0.5, h * 0.5));
        let src = [[0.15, 0.20], [0.85, 0.20], [0.85, 0.80], [0.15, 0.80]];
        let mut corners = [[0.0; 2]; 4];
        for (i, c) in src.iter().enumerate() {
            let q = turn.apply(lightcraft_geom::Point::new(c[0] * w, c[1] * h));
            corners[i] = [q.x / w, q.y / h];
        }
        let turned = crop_for_frame(&FilmFrame { corners }, w, h).expect("turned");
        assert!((turned.angle.abs() - 10.0).abs() < 0.2, "{}", turned.angle);
        let into = Affine::rotate_about(turned.angle.to_radians(), lightcraft_geom::Point::new(w * 0.5, h * 0.5));
        for c in corners {
            let q = into.apply(lightcraft_geom::Point::new(c[0] * w, c[1] * h));
            let x = q.x / w;
            let y = q.y / h;
            assert!(x >= turned.rect.x0 - 0.02 && x <= turned.rect.x1 + 0.02, "{x} not in {:?}", turned.rect);
            assert!(y >= turned.rect.y0 - 0.02 && y <= turned.rect.y1 + 0.02, "{y} not in {:?}", turned.rect);
        }
    }

    #[test]
    fn a_film_strip_crops_inside_the_sprockets() {
        let (w, h) = (120usize, 90usize);
        let film = [0.80f32, 0.35, 0.12];
        let img = Rgb32f::from_fn(w, h, |x, y| {
            let hole_row = (4..18).contains(&y) || (72..86).contains(&y);
            let hole = hole_row && (x % 12) < 5 && x > 4 && x + 4 < w;
            if hole {
                [0.98, 0.98, 0.96]
            } else if x < 10 || y < 24 || x >= 110 || y >= 66 {
                if x < 2 || y < 2 || x + 2 >= w || y + 2 >= h { [0.02, 0.01, 0.0] } else { film }
            } else {
                let t = ((x + y) % 7) as f32 / 7.0;
                [0.12 + t * 0.2, 0.06 + t * 0.08, 0.03]
            }
        });
        let frame = detect_gate(&img, FilmStock::ColorNeg).expect("gate");
        let top = frame.corners[0][1];
        let bottom = frame.corners[2][1];
        let left = frame.corners[0][0];
        let right = frame.corners[1][0];
        assert!(top > 0.18 && top < 0.32, "top {top}");
        assert!(bottom > 0.68 && bottom < 0.80, "bottom {bottom}");
        assert!(left > 0.04 && left < 0.14, "left {left}");
        assert!(right > 0.85 && right < 0.96, "right {right}");
    }

    #[test]
    fn edge_printing_stays_outside_the_picture_and_a_real_edge_does_not() {
        let (w, h) = (120usize, 90usize);
        let film_px = [0.80f32, 0.35, 0.12];
        let ink = [0.04f32, 0.02, 0.01];
        let img = Rgb32f::from_fn(w, h, |x, y| {
            let hole_row = (4..18).contains(&y) || (72..86).contains(&y);
            let hole = hole_row && (x % 12) < 5 && x > 4 && x + 4 < w;
            // Frame numbers on the rebate: a dark minority of an otherwise clear line.
            let printing = ((19..23).contains(&y) && x % 3 == 0) || ((5..9).contains(&x) && y % 3 == 0 && (24..66).contains(&y));
            if hole {
                [0.98, 0.98, 0.96]
            } else if printing {
                ink
            } else if x < 10 || y < 24 || x >= 110 || y >= 66 {
                if x < 2 || y < 2 || x + 2 >= w || y + 2 >= h { [0.02, 0.01, 0.0] } else { film_px }
            } else {
                let t = ((x + y) % 7) as f32 / 7.0;
                [0.12 + t * 0.2, 0.06 + t * 0.08, 0.03]
            }
        });
        let frame = detect_gate(&img, FilmStock::ColorNeg).expect("gate");
        assert!(frame.corners[0][1] > 0.18 && frame.corners[0][1] < 0.32, "top {:?}", frame.corners[0][1]);
        assert!(frame.corners[0][0] > 0.04 && frame.corners[0][0] < 0.14, "left {:?}", frame.corners[0][0]);

        // A picture that has actually left the rebate, even in the outer band, is not printing.
        let img = Rgb32f::from_fn(100, 80, |x, y| {
            let rebate = x < 8 || y < 8 || x >= 92 || y >= 72;
            if rebate {
                if (5..8).contains(&y) && x % 3 == 0 { ink } else { film_px }
            } else {
                let t = ((x * 3 + y) % 5) as f32 / 5.0;
                [0.28 + t * 0.15, 0.14 + t * 0.08, 0.05]
            }
        });
        let frame = detect_gate(&img, FilmStock::ColorNeg).expect("picture");
        let top = frame.corners[0][1];
        assert!(top > 0.06 && top < 0.16, "the picture's own edge was cropped away {top}");
    }

    #[test]
    fn a_rebate_becomes_a_frame_and_a_blank_scan_does_not() {
        let img = Rgb32f::from_fn(80, 60, |x, y| {
            if x < 6 || y < 6 || x >= 74 || y >= 54 {
                [0.90, 0.45, 0.16]
            } else if x < 24 && y < 20 {
                [0.15, 0.25, 0.85]
            } else {
                [0.30, 0.28, 0.22]
            }
        });
        let frame = detect_frame(&img, FilmStock::ColorNeg).expect("orange border");
        assert!(inside_frame(&frame, 0.25, 0.30), "the blue sky stays inside the frame {frame:?}");
        assert!(frame.corners.iter().map(|c| c[0]).fold(f64::MAX, f64::min) > 0.03);
        assert!(detect_frame(&Rgb32f::filled(40, 40, [0.4, 0.4, 0.4]), FilmStock::ColorNeg).is_none());
    }

    #[test]
    fn a_yellow_field_is_not_pulled_to_gray() {
        let img = Rgb32f::from_fn(48, 36, |x, y| if x < 4 || y < 4 || x >= 44 || y >= 32 { [0.85, 0.42, 0.14] } else { [0.70, 0.62, 0.08] });
        let mut film = Film { intent: FilmIntent::Scene, ..Film::default() };
        film.frame = detect_frame(&img, FilmStock::ColorNeg).unwrap_or_default();
        let stats = measure(&img, &film);
        assert!(stats.dominant > 0.75, "dominant {}", stats.dominant);
        assert!((stats.gamma[0] - 1.0).abs() < 1e-6 && (stats.gamma[2] - 1.0).abs() < 1e-6, "{:?}", stats.gamma);
    }

    #[test]
    fn two_neutrals_set_the_channel_contrast() {
        let base = [0.8, 0.5, 0.2];
        // Both picks are neutral under one channel contrast, so a single gamma can hold them.
        let gamma_true = [1.2, 1.0, 0.8];
        let at = |density: f64| -> [f64; 3] {
            [0, 1, 2].map(|c| {
                let n = 10f64.powf(-density / gamma_true[c]);
                n * base[c]
            })
        };
        let shadow = at(0.30);
        let highlight = at(0.70);
        let gamma = gamma_from_picks(base, &[(shadow, 1.0), (highlight, 1.0)], 1.0);
        let mut f = Film { enabled: true, scan: FilmScan::Flat, ..Film::default() };
        f.set_base(base);
        f.set_gamma(gamma);
        let k = FilmKernel::from_film(&f);
        for pix in [shadow, highlight] {
            let s = k.apply(pix.map(|v| v as f32));
            let spread = s[0].max(s[1]).max(s[2]) - s[0].min(s[1]).min(s[2]);
            assert!(spread < 1e-3, "pick {pix:?} -> {s:?}");
        }
    }

    #[test]
    fn hanson_pulls_magenta_out_of_blue_and_the_axis_prints_gray() {
        let dye = FilmScan::Camera.dye().map(|v| v as f32);
        let neutral = printing_density([0.60, 0.60, 0.60], dye);
        // C = 0.60, M = 0.60 − 0.12·0.60, Y lower still. An equal integral density is not analytical.
        assert!(neutral[1] < 0.55 && neutral[2] < neutral[1], "analytical {neutral:?}");
        let leaked = printing_density([0.40, 0.80, 0.60], dye);
        assert!(leaked[2] < 0.50, "magenta dye's blue absorption comes out {leaked:?}");
        assert_eq!(printing_density([0.2, 0.2, 0.2], [0.0; 3]), [0.2, 0.2, 0.2]);

        let orange = [0.80f32, 0.38, 0.14];
        let img = Rgb32f::from_fn(90, 60, |x, y| {
            if x < 8 || y < 8 || x >= 82 || y >= 52 {
                orange
            } else {
                let d = 0.20 + 0.80 * (x - 8) as f32 / 74.0;
                orange.map(|c| c * 10f32.powf(-d))
            }
        });
        let balanced = analyze_image(&img, &Film::default());
        let k = FilmKernel::from_film(&balanced.film);
        let mid = k.apply(orange.map(|c| c * 10f32.powf(-0.55)));
        let spread = mid[0].max(mid[1]).max(mid[2]) - mid[0].min(mid[1]).min(mid[2]);
        assert!(spread < 0.06, "a neutral scale prints gray after the dye correction {mid:?} axis {:?}", balanced.film.axis());
        let y = |p: [f32; 3]| 0.2627 * p[0] + 0.6780 * p[1] + 0.0593 * p[2];
        let white_d = balanced.film.d_hi as f32;
        let at = k.apply(orange.map(|c| c * 10f32.powf(-white_d)));
        let above = k.apply(orange.map(|c| c * 10f32.powf(-(white_d + 0.30))));
        assert!(y(above) > y(at) + 0.02, "density above the white is not clipped {above:?} vs {at:?}");
    }

    #[test]
    fn a_cyan_cast_across_the_picture_is_balanced_and_a_green_shadow_is_not() {
        let orange = [0.80f32, 0.38, 0.14];
        let cast = Rgb32f::from_fn(90, 60, |x, y| {
            if x < 8 || y < 8 || x >= 82 || y >= 52 {
                orange
            } else {
                let d = 0.18 + 0.75 * (x - 8) as f32 / 74.0;
                let density = [d, d + 0.28, d + 0.22];
                [0, 1, 2].map(|i| orange[i] * 10f32.powf(-density[i]))
            }
        });
        let balanced = analyze_image(&cast, &Film::default());
        let k = FilmKernel::from_film(&balanced.film);
        let mid = [0.55f32, 0.83, 0.77];
        let pix = [0, 1, 2].map(|i| orange[i] * 10f32.powf(-mid[i]));
        let s = k.apply(pix);
        let spread = s[0].max(s[1]).max(s[2]) - s[0].min(s[1]).min(s[2]);
        assert!(spread < 0.08, "a cast through the whole scale prints gray {s:?} fog {:?}", balanced.film.fog());
        let mask = k.apply(orange);
        let mask_y = 0.2627 * mask[0] + 0.6780 * mask[1] + 0.0593 * mask[2];
        assert!(mask_y < 0.08, "the mask stays black {mask:?}");

        // Green only in the thin (dark) half. The bright half is neutral, so this is the picture, not the film.
        let scene = Rgb32f::from_fn(90, 60, |x, y| {
            if x < 8 || y < 8 || x >= 82 || y >= 52 {
                orange
            } else {
                let d = 0.18 + 0.75 * (x - 8) as f32 / 74.0;
                let density = if x < 45 { [d, d + 0.35, d] } else { [d, d, d] };
                [0, 1, 2].map(|i| orange[i] * 10f32.powf(-density[i]))
            }
        });
        let kept = FilmKernel::from_film(&analyze_image(&scene, &Film::default()).film);
        let shadow = [0, 1, 2].map(|i| orange[i] * 10f32.powf(-[0.30, 0.65, 0.30][i]));
        let g = kept.apply(shadow);
        assert!(g[1] > g[0] + 0.04, "a green shadow stays green {g:?}");
    }

    #[test]
    fn a_slope_cast_is_neutral_and_a_blue_highlight_stays() {
        let orange = [0.80f32, 0.38, 0.14];
        let slope = Rgb32f::from_fn(90, 60, |x, y| {
            if x < 8 || y < 8 || x >= 82 || y >= 52 {
                orange
            } else {
                let d = 0.2 + 0.8 * (x - 8) as f32 / 74.0;
                let density = [d * 0.62, d, d * 1.45];
                [0, 1, 2].map(|i| orange[i] * 10f32.powf(-density[i]))
            }
        });
        let once = analyze_image(&slope, &Film::default());
        let twice = analyze_image(&slope, &once.film);
        assert!((once.film.axis()[2] - twice.film.axis()[2]).abs() < 0.05, "a second auto holds the slope");
        let k = FilmKernel::from_film(&once.film);
        let at = |d: f32| {
            let density = [d * 0.62, d, d * 1.45];
            k.apply([0, 1, 2].map(|i| orange[i] * 10f32.powf(-density[i])))
        };
        for d in [0.35, 0.7] {
            let s = at(d);
            let spread = s[0].max(s[1]).max(s[2]) - s[0].min(s[1]).min(s[2]);
            assert!(spread < 0.1, "slope at {d} prints gray {s:?} axis {:?}", once.film.axis());
        }
        let mask = k.apply(orange);
        let mask_y = 0.2627 * mask[0] + 0.6780 * mask[1] + 0.0593 * mask[2];
        assert!(mask_y < 0.08, "mask stays black {mask:?}");

        let sky = Rgb32f::from_fn(90, 60, |x, y| {
            if x < 8 || y < 8 || x >= 82 || y >= 52 {
                orange
            } else {
                let d = 0.2 + 0.8 * (x - 8) as f32 / 74.0;
                let density = if x > 55 { [d, d, d + 0.4] } else { [d, d, d] };
                [0, 1, 2].map(|i| orange[i] * 10f32.powf(-density[i]))
            }
        });
        let kept = FilmKernel::from_film(&analyze_image(&sky, &Film::default()).film);
        let hi = [0.85f32, 0.85, 1.25];
        let pix = kept.apply([0, 1, 2].map(|i| orange[i] * 10f32.powf(-hi[i])));
        assert!(pix[2] > pix[1] + 0.04, "a blue highlight stays blue {pix:?}");
    }

    #[test]
    fn a_paler_orange_does_not_set_the_base() {
        let strong = [0.82f32, 0.36, 0.12];
        let pale = [0.74f32, 0.44, 0.26];
        assert!(is_clear_film(strong) && is_clear_film(pale));
        let img = Rgb32f::from_fn(
            80,
            60,
            |x, y| {
                if x < 8 || y < 8 || x >= 72 || y >= 52 { if x < 40 { strong } else { pale } } else { [0.12, 0.12, 0.12] }
            },
        );
        let film = Film { enabled: true, ..Film::default() };
        let stats = measure(&img, &film);
        let ratio = stats.base[2] / stats.base[0].max(1e-6);
        let mid = ((pale[2] / pale[0]) as f64 + (strong[2] / strong[0]) as f64) * 0.5;
        assert!(ratio < mid, "base {:?} should follow the stronger orange, not {pale:?}", stats.base);
    }

    #[test]
    fn a_hot_pixel_does_not_set_the_orange_base() {
        let film_px = [0.80f32, 0.38, 0.14];
        // Brighter than the film, still clear film, and less orange. The old floor was 85% of the
        // single brightest pixel, which drops the real rebate and keeps the spike.
        let hot = [0.97f32, 0.55, 0.28];
        assert!(is_clear_film(film_px) && is_clear_film(hot));
        assert!(luminance_2020(film_px) < luminance_2020(hot) * 0.85);
        let img = Rgb32f::from_fn(80, 60, |x, y| {
            let border = x < 8 || y < 8 || x >= 72 || y >= 52;
            if !border {
                return [0.20, 0.22, 0.24];
            }
            if x.wrapping_mul(3).wrapping_add(y.wrapping_mul(5)) % 11 == 0 { hot } else { film_px }
        });
        let analysis = analyze_image(&img, &Film::default());
        assert!(analysis.stats.mask_measured, "the noisy rebate was not a border");
        let b = analysis.film.base();
        for c in 0..3 {
            assert!((b[c] - film_px[c] as f64).abs() < 0.03, "channel {c}: base {b:?} followed the hot pixel {hot:?}");
        }
    }

    #[test]
    fn a_border_click_reads_the_film_around_a_grain_clump() {
        let film_px = [0.80f32, 0.38, 0.14];
        let grain = [0.98f32, 0.62, 0.30];
        assert!(is_clear_film(grain));
        let img = Rgb32f::from_fn(240, 180, |x, y| {
            let border = x < 28 || y < 24 || x >= 212 || y >= 156;
            if !border {
                return [0.18, 0.20, 0.22];
            }
            if (30..42).contains(&x) && (4..16).contains(&y) { grain } else { film_px }
        });
        let s = sample_border(&img, 36.0 / 240.0, 10.0 / 180.0).expect("border");
        for c in 0..3 {
            assert!((s[c] - film_px[c] as f64).abs() < 0.05, "channel {c}: {s:?} followed the grain {grain:?}");
        }
        let local = sample(&img, 36.0 / 240.0, 10.0 / 180.0).expect("local");
        assert!((local[0] - grain[0] as f64).abs() < 0.08, "the 5×5 under the clump should still be the grain, got {local:?}");
    }

    #[test]
    fn a_dark_holder_does_not_hide_the_rebate() {
        let holder = [0.14f32, 0.13, 0.12];
        let orange = [0.80f32, 0.38, 0.14];
        assert!(is_holder(holder) && !is_mask_orange(holder));
        let img = Rgb32f::from_fn(100, 80, |x, y| {
            if x < 8 || y < 8 || x >= 92 || y >= 72 {
                holder
            } else if x < 16 || y < 14 || x >= 84 || y >= 66 {
                orange
            } else {
                [0.16, 0.18, 0.22]
            }
        });
        let frame = detect_frame(&img, FilmStock::ColorNeg).expect("rebate inside the holder");
        assert!(inside_frame(&frame, 0.5, 0.5), "picture {frame:?}");
        assert!(!inside_frame(&frame, 0.02, 0.5), "holder was the picture {frame:?}");
        assert!(!inside_frame(&frame, 0.12, 0.5), "rebate was the picture {frame:?}");
        let analysis = analyze_image(&img, &Film::default());
        assert!(analysis.frame_error.is_none(), "{:?}", analysis.frame_error);
        let b = analysis.film.base();
        for c in 0..3 {
            assert!((b[c] - orange[c] as f64).abs() < 0.06, "channel {c}: base {b:?}");
        }
    }

    #[test]
    fn a_small_negative_in_a_black_holder_has_one_border_color() {
        let holder = [0.05f32, 0.045, 0.04];
        let orange = [0.80f32, 0.38, 0.14];
        assert!(is_outside_margin(holder));
        let img = Rgb32f::from_fn(180, 140, |x, y| {
            let in_film = (60..120).contains(&x) && (48..100).contains(&y);
            if !in_film {
                return holder;
            }
            let rebate = x < 68 || y < 56 || x >= 112 || y >= 92;
            if rebate { orange } else { [0.22, 0.16, 0.10] }
        });
        let frame = detect_frame(&img, FilmStock::ColorNeg).expect("the holder hid the rebate");
        assert!(inside_frame(&frame, 0.5, 0.5), "picture {frame:?}");
        assert!(!inside_frame(&frame, 0.05, 0.05), "holder was the picture {frame:?}");
        let left = frame.corners.iter().map(|c| c[0]).fold(f64::MAX, f64::min);
        assert!((left - 68.0 / 180.0).abs() < 0.04, "left edge {left} frame {frame:?}");
        let on_rebate = sample_border(&img, 64.0 / 180.0, 52.0 / 140.0).expect("rebate");
        let on_holder = sample_border(&img, 20.0 / 180.0, 20.0 / 140.0).expect("holder");
        let in_picture = sample_border(&img, 90.0 / 180.0, 70.0 / 140.0).expect("picture");
        for (name, s) in [("rebate", on_rebate), ("holder", on_holder), ("picture", in_picture)] {
            for c in 0..3 {
                assert!((s[c] - orange[c] as f64).abs() < 0.05, "{name} channel {c}: {s:?}");
            }
        }
        assert!(
            (on_rebate[0] - on_holder[0]).abs() < 0.02 && (on_rebate[2] - in_picture[2]).abs() < 0.02,
            "clicks diverged {on_rebate:?} {on_holder:?} {in_picture:?}"
        );
    }

    #[test]
    fn a_bright_sky_beside_the_rebate_is_still_one_border() {
        // The picture's sky is almost as bright as the rebate, and the building is only a little
        // darker. A brightness test calls both of them blank film. The rebate is the flat band
        // against the holder, and every click on that scan is that one color.
        let holder = [0.04f32, 0.02, 0.01];
        let rebate = [0.996f32, 0.706, 0.475];
        let building = [0.973f32, 0.659, 0.424];
        let sky = [1.0f32, 0.639, 0.251];
        let grain = [0.99f32, 0.40, 0.05];
        let (w, h) = (240usize, 160usize);
        let img = Rgb32f::from_fn(w, h, |x, y| {
            if y < 16 || y + 8 >= h || x < 4 || x + 16 >= w {
                return holder;
            }
            if x == 8 && y == 22 {
                return grain;
            }
            if y < 28 || y + 16 >= h || x < 12 || x + 26 >= w {
                return rebate;
            }
            if x < w / 2 { building } else { sky }
        });
        assert!(rebate_hit(&img).is_none(), "the sky was classified as a separate picture");
        let frame = detect_frame(&img, FilmStock::ColorNeg).expect("the holder hid a bright negative");
        assert!(inside_frame(&frame, 0.30, 0.50), "building {frame:?}");
        assert!(inside_frame(&frame, 0.72, 0.50), "sky {frame:?}");
        assert!(!inside_frame(&frame, 0.01, 0.50), "holder {frame:?}");
        assert!(!inside_frame(&frame, 8.0 / w as f64, 22.0 / h as f64), "rebate {frame:?}");
        let on_grain = sample_border(&img, 8.0 / w as f64, 22.0 / h as f64).expect("grain");
        let on_holder = sample_border(&img, 2.0 / w as f64, 8.0 / h as f64).expect("holder");
        let on_sky = sample_border(&img, 0.72, 0.50).expect("sky");
        let on_building = sample_border(&img, 0.30, 0.50).expect("building");
        for (name, s) in [("grain", on_grain), ("holder", on_holder), ("sky", on_sky), ("building", on_building)] {
            for c in 0..3 {
                assert!((s[c] - rebate[c] as f64).abs() < 0.05, "{name} channel {c}: {s:?} vs {rebate:?}");
            }
        }
        assert!(
            (on_grain[0] - on_sky[0]).abs() < 0.02 && (on_holder[2] - on_building[2]).abs() < 0.02,
            "clicks diverged {on_grain:?} {on_holder:?} {on_sky:?} {on_building:?}"
        );
        let b = analyze_image(&img, &Film::default()).film.base();
        for c in 0..3 {
            assert!((b[c] - rebate[c] as f64).abs() < 0.05, "auto base channel {c}: {b:?}");
        }
    }

    #[test]
    fn a_border_sample_is_the_area_not_one_hot_pixel() {
        let film_px = [0.80f32, 0.38, 0.14];
        let mut img = Rgb32f::filled(20, 20, film_px);
        img.set(10, 10, [0.99, 0.70, 0.40]);
        let s = sample(&img, 0.5, 0.5).expect("sample");
        for c in 0..3 {
            assert!((s[c] - film_px[c] as f64).abs() < 0.005, "channel {c}: {s:?}");
        }
    }

    #[test]
    fn grain_on_the_rebate_averages_to_the_film() {
        let film_px = [0.80f32, 0.38, 0.14];
        // Long side above 256 so the frame is fit to block averages, and above 192 so the base is
        // a block median. A point sample of this grain is often not orange.
        let (w, h) = (768usize, 576usize);
        let img = Rgb32f::from_fn(w, h, |x, y| {
            let border = x < 48 || y < 40 || x + 48 >= w || y + 40 >= h;
            if !border {
                return [0.18, 0.20, 0.22];
            }
            if x.wrapping_mul(7).wrapping_add(y.wrapping_mul(3)) % 17 == 0 {
                return [0.99, 0.70, 0.36];
            }
            let mut p = film_px;
            for c in 0..3 {
                p[c] = (p[c] + 0.22 * rebate_grain(x, y, c)).clamp(0.02, 1.2);
            }
            p
        });
        let analysis = analyze_image(&img, &Film::default());
        assert!(analysis.frame_error.is_none(), "{:?}", analysis.frame_error);
        assert!(analysis.stats.mask_measured);
        let b = analysis.film.base();
        for c in 0..3 {
            assert!((b[c] - film_px[c] as f64).abs() < 0.05, "channel {c}: base {b:?} is not the rebate {film_px:?}");
        }
        let y = |p: [f32; 3]| 0.2627 * p[0] + 0.6780 * p[1] + 0.0593 * p[2];
        let mask = FilmKernel::from_film(&analysis.film).apply(film_px);
        assert!(y(mask) < 0.08, "the film does not print black {mask:?} base {b:?}");
    }

    #[test]
    fn a_noisy_edge_is_found_from_the_area_average() {
        // Point samples land on the even columns, which are not orange. The area of each block is.
        let (w, h) = (512usize, 384usize);
        let orange = [0.80f32, 0.38, 0.14];
        let junk = [0.20f32, 0.22, 0.18];
        assert!(!is_mask_orange(junk) && !is_pale_rebate(junk));
        let img = Rgb32f::from_fn(w, h, |x, y| {
            let border = x < 48 || y < 36 || x + 48 >= w || y + 36 >= h;
            if !border {
                [0.16, 0.18, 0.20]
            } else if x % 2 == 0 {
                junk
            } else {
                orange
            }
        });
        let frame = detect_frame(&img, FilmStock::ColorNeg).expect("the averaged border is orange");
        assert!(inside_frame(&frame, 0.5, 0.5), "the picture dropped out {frame:?}");
        assert!(!inside_frame(&frame, 0.01, 0.5), "the border was taken as the picture {frame:?}");
        let analysis = analyze_image(&img, &Film::default());
        assert!(analysis.stats.mask_measured, "the area of the rebate was not a base");
        let b = analysis.film.base();
        // The junk columns are not film. The base is the orange ones.
        assert!((b[0] - orange[0] as f64).abs() < 0.06, "red {b:?}");
        assert!(b[2] < b[0] * 0.45, "the rebate stayed orange {b:?}");
    }

    #[test]
    fn a_light_table_is_not_the_mask_and_the_print_stays_in_range() {
        let orange = [0.80f32, 0.38, 0.14];
        let img = Rgb32f::from_fn(100, 70, |x, y| {
            if x < 5 || y < 5 || x >= 95 || y >= 65 {
                [0.97, 0.96, 0.95]
            } else if x < 12 || y < 12 || x >= 88 || y >= 58 {
                orange
            } else {
                let d = 0.15 + 0.9 * (x - 12) as f32 / 76.0;
                orange.map(|c| c * 10f32.powf(-d))
            }
        });
        let analysis = analyze_image(&img, &Film::default());
        let b = analysis.film.base();
        assert!(b[2] < 0.35 && b[0] > 0.55, "the base is the orange film, not the lamp {b:?}");
        let k = FilmKernel::from_film(&analysis.film);
        let mid = orange.map(|c| c * 10f32.powf(-0.55));
        let s = k.apply(mid);
        let spread = s[0].max(s[1]).max(s[2]) - s[0].min(s[1]).min(s[2]);
        assert!(spread < 0.08, "a neutral gray stays neutral {s:?}");
        assert!(s.iter().copied().all(|c| c < 1.05), "the midtone is not blown {s:?}");
        let dense = k.apply(orange.map(|c| c * 10f32.powf(-1.0)));
        let thin = k.apply(orange.map(|c| c * 10f32.powf(-0.2)));
        let y = |p: [f32; 3]| 0.2627 * p[0] + 0.6780 * p[1] + 0.0593 * p[2];
        assert!(y(dense) > y(thin), "a denser negative is a brighter positive {dense:?} {thin:?}");
        let again = analyze_image(&img, &analysis.film);
        assert!((again.film.base()[2] - b[2]).abs() < 1e-3, "a second pass keeps the orange base");
    }

    #[test]
    fn the_bright_edge_where_film_meets_the_lamp_is_not_the_base() {
        let orange = [0.80f32, 0.38, 0.14];
        let washed = [0.92f32, 0.62, 0.40];
        let img = Rgb32f::from_fn(100, 70, |x, y| {
            let edge = x < 4 || y < 4 || x >= 96 || y >= 66;
            let wash = x < 8 || y < 8 || x >= 92 || y >= 62;
            let ring = x < 16 || y < 16 || x >= 84 || y >= 54;
            if edge {
                [0.98, 0.97, 0.96]
            } else if wash {
                washed
            } else if ring {
                orange
            } else {
                let d = 0.2 + 0.7 * (x - 16) as f32 / 68.0;
                orange.map(|c| c * 10f32.powf(-d))
            }
        });
        let b = analyze_image(&img, &Film::default()).film.base();
        assert!(b[2] < 0.22 && b[0] > 0.6, "the washed lamp edge is not the base {b:?}");
        assert!(!accepts_border(FilmStock::ColorNeg, [0.97, 0.96, 0.95]));
        assert!(accepts_border(FilmStock::ColorNeg, [0.80, 0.38, 0.14]));
        let dark = [0.10f64, 0.07, 0.05];
        let on_bright = border_error(FilmStock::ColorNeg, dark, 1.0).unwrap_or("");
        assert!(on_bright.contains("too dark"), "{on_bright}");
        assert!(border_error(FilmStock::ColorNeg, dark, 0.12).is_none(), "a dark scan's own film is the base");
        let lamp = border_error(FilmStock::ColorNeg, [0.97, 0.96, 0.95], 0.12).unwrap_or("");
        assert!(lamp.contains("blown out"), "{lamp}");
        let scan = Rgb32f::filled(32, 24, [0.10, 0.07, 0.05]);
        let level = bright_level(&scan, FilmCapture::Linear);
        assert!(border_error(FilmStock::ColorNeg, dark, level).is_none(), "level {level}");
        assert!(border_error(FilmStock::ColorNeg, [0.02, 0.015, 0.01], level).unwrap_or("").contains("too dark"));
        // Camera white balance can lift an orange mask to a pale or near-white rebate.
        assert!(accepts_border(FilmStock::ColorNeg, [0.82, 0.76, 0.70]));
        assert!(accepts_border(FilmStock::ColorNeg, [0.85, 0.70, 0.55]));
    }

    #[test]
    fn a_white_balanced_rebate_is_the_base() {
        let film_px = [0.78f32, 0.74, 0.70];
        let img = Rgb32f::from_fn(80, 60, |x, y| {
            if x < 6 || y < 6 || x >= 74 || y >= 54 {
                film_px
            } else {
                let d = 0.7 + 0.6 * (x - 6) as f32 / 68.0;
                film_px.map(|c| c * 10f32.powf(-d))
            }
        });
        let b = analyze_image(&img, &Film::default()).film.base();
        for c in 0..3 {
            assert!((b[c] - film_px[c] as f64).abs() < 0.04, "base {b:?} should be the pale rebate {film_px:?}");
        }
        assert!(accepts_border(FilmStock::ColorNeg, film_px.map(|v| v as f64)));
    }

    #[test]
    fn a_clipped_lamp_does_not_replace_a_pale_rebate() {
        let film_px = [0.72f32, 0.66, 0.60];
        let img = Rgb32f::from_fn(100, 70, |x, y| {
            if x < 4 || y < 4 || x >= 96 || y >= 66 {
                [0.98, 0.97, 0.96]
            } else if x < 12 || y < 12 || x >= 88 || y >= 58 {
                film_px
            } else {
                let d = 0.7 + 0.6 * (x - 12) as f32 / 76.0;
                film_px.map(|c| c * 10f32.powf(-d))
            }
        });
        let b = analyze_image(&img, &Film::default()).film.base();
        assert!(b[0] < 0.85 && b[2] > 0.45, "the base is the pale film, not the lamp {b:?}");
    }

    #[test]
    fn a_low_key_print_holds_the_highlight_and_keeps_it_neutral() {
        let orange = [0.80f32, 0.38, 0.14];
        let hi_px = [orange[0] * 10f32.powf(-1.25), orange[1] * 10f32.powf(-1.05), orange[2] * 10f32.powf(-1.10)];
        let img = Rgb32f::from_fn(80, 60, |x, y| {
            if x < 6 || y < 6 || x >= 74 || y >= 54 {
                orange
            } else if (62..74).contains(&x) && (42..54).contains(&y) {
                hi_px
            } else {
                orange.map(|c| c * 10f32.powf(-0.20))
            }
        });
        let analysis = analyze_image(&img, &Film { scan: FilmScan::Flat, ..Film::default() });
        let y = |p: [f32; 3]| 0.2627 * p[0] + 0.6780 * p[1] + 0.0593 * p[2];
        let chroma = |p: [f32; 3]| {
            let max = p[0].max(p[1]).max(p[2]).max(1e-6);
            (max - p[0].min(p[1]).min(p[2])) / max
        };
        let hi = FilmKernel::from_film(&analysis.film).apply(hi_px);
        assert!(y(hi) < 1.25 && y(hi) > 0.45, "highlight {hi:?} ev {}", analysis.film.exposure);
        assert!(chroma(hi) < 0.08, "highlight tinted {hi:?} white {:?}", analysis.film.white());
        let mut down = analysis.film.clone();
        down.exposure -= 2.0;
        let hi2 = FilmKernel::from_film(&down).apply(hi_px);
        assert!(chroma(hi2) < chroma(hi) + 0.02, "lowering exposure brought the tint back {hi:?} {hi2:?}");
    }

    #[test]
    fn a_rebate_outside_the_picture_does_not_blow_the_print() {
        let orange = [0.80f32, 0.38, 0.14];
        let img = Rgb32f::from_fn(80, 60, |x, y| {
            if (25..55).contains(&x) && (18..42).contains(&y) {
                let d = 0.25 + 0.7 * (x - 25) as f32 / 30.0;
                orange.map(|c| c * 10f32.powf(-d))
            } else {
                orange
            }
        });
        let film = Film {
            frame: FilmFrame {
                corners: [[25.0 / 80.0, 18.0 / 60.0], [55.0 / 80.0, 18.0 / 60.0], [55.0 / 80.0, 42.0 / 60.0], [25.0 / 80.0, 42.0 / 60.0]],
            },
            ..Film::default()
        };
        let stats = measure(&img, &film);
        assert!(stats.mask_measured);
        let mut film = Film { enabled: true, ..Film::default() };
        film.set_base(stats.base);
        film.d_lo = stats.d_lo;
        film.d_hi = stats.d_hi;
        film.set_fog(stats.fog);
        film.set_axis(stats.axis);
        film.set_white(stats.white);
        film.mid_a = stats.mid_a;
        film.mid_b = stats.mid_b;
        film.grade = stats.grade;
        film.exposure = stats.exposure;
        film.pivot = stats.pivot;
        film.anchor = stats.anchor;
        let k = FilmKernel::from_film(&film);
        let y = |p: [f32; 3]| 0.2627 * p[0] + 0.6780 * p[1] + 0.0593 * p[2];
        let mid = k.apply(orange.map(|c| c * 10f32.powf(-0.55)));
        assert!(y(mid) < 0.55, "the middle of the picture is not blown {mid:?} grade {} ev {}", stats.grade, stats.exposure);
        let mask = k.apply(orange);
        assert!(y(mask) < 0.08, "clear film stays black {mask:?}");
    }

    #[test]
    fn a_roll_keeps_each_frame_s_exposure_and_reports_drift() {
        let a = FrameStats {
            base: [0.80, 0.40, 0.20],
            gamma: [1.0, 1.0, 1.0],
            exposure: 0.2,
            rebate_level: 0.40,
            dominant: 0.0,
            bins: vec![],
            rebate_black: true,
            mask_measured: true,
            d_lo: 0.0,
            d_hi: 1.0,
            fog: [0.0, 0.0, 0.10],
            axis: [0.80, 1.0, 1.20],
            white: [0.0; 3],
            mid_a: 0.0,
            mid_b: 0.0,
            grade: 1.0,
            shoulder: 0.0,
            pivot: 0.0,
            anchor: 0.0,
        };
        let mut b = a.clone();
        b.base = [0.40, 0.20, 0.10];
        b.exposure = -0.4;
        b.rebate_level = 0.20;
        b.fog = [0.0, 0.0, 0.20];
        b.axis = [0.60, 1.0, 1.40];
        let shared = share_roll(&[a, b]);
        assert_eq!(shared.len(), 2);
        let r0 = shared[0].base[0] / shared[0].base[1];
        let r1 = shared[1].base[0] / shared[1].base[1];
        assert!((r0 - r1).abs() < 1e-6, "shared mask color");
        assert!((shared[0].base[1] - 0.40).abs() < 1e-6);
        assert!((shared[1].base[1] - 0.20).abs() < 1e-6, "each frame keeps its rebate level");
        assert!((shared[0].exposure - 0.2).abs() < 1e-9);
        assert!((shared[1].exposure + 0.4).abs() < 1e-9, "each frame keeps its exposure");
        assert_eq!(shared[0].axis, shared[1].axis, "one gray axis for the roll");
        assert_eq!(shared[0].fog, shared[1].fog);
        assert!(shared[1].drift < -DRIFT_WARN, "drift {}", shared[1].drift);
    }

    #[test]
    fn a_frame_without_a_measured_mask_keeps_its_base() {
        let measured = FrameStats {
            base: [0.80, 0.40, 0.20],
            gamma: [1.0, 1.0, 1.0],
            exposure: 0.2,
            rebate_level: 0.40,
            dominant: 0.0,
            bins: vec![],
            rebate_black: true,
            mask_measured: true,
            d_lo: 0.0,
            d_hi: 1.0,
            fog: [0.0; 3],
            axis: [1.0; 3],
            white: [0.0; 3],
            mid_a: 0.0,
            mid_b: 0.0,
            grade: 1.0,
            shoulder: 0.0,
            pivot: 0.0,
            anchor: 0.0,
        };
        let bare = FrameStats { mask_measured: false, base: [0.50, 0.50, 0.50], rebate_level: 0.50, ..measured.clone() };
        let shared = share_roll(&[measured.clone(), bare.clone(), bare.clone()]);
        let ratio = shared[0].base[0] / shared[0].base[1];
        assert!((ratio - 2.0).abs() < 1e-6, "the unmeasured bases must not move the shared ratio, got {ratio}");
        assert_eq!(shared[1].base, [0.50, 0.50, 0.50]);
        assert_eq!(shared[2].base, [0.50, 0.50, 0.50]);
        assert_eq!(shared[1].drift, 0.0);

        let orange = [0.80f32, 0.38, 0.14];
        let picture = [0.12f32, 0.11, 0.10];
        let with_border = Rgb32f::from_fn(48, 36, |x, y| if x < 6 || y < 6 || x >= 42 || y >= 30 { orange } else { picture });
        let tight = Rgb32f::filled(48, 36, picture);
        let mut previous = Film::default();
        previous.set_base([0.55, 0.55, 0.55]);
        assert!(analyze_image(&with_border, &previous).stats.mask_measured);
        let missed = analyze_image(&tight, &previous);
        assert!(!missed.stats.mask_measured);
        assert_eq!(missed.stats.base, [0.55, 0.55, 0.55]);

        let warm = [0.80f32, 0.38, 0.14];
        let orange_inside = Rgb32f::from_fn(48, 36, |x, y| if (20..28).contains(&x) && (14..22).contains(&y) { warm } else { picture });
        let guessed = analyze_image(&orange_inside, &previous);
        assert!(!guessed.stats.mask_measured, "orange in the picture is not a border");
        assert_eq!(guessed.stats.base, [0.55, 0.55, 0.55]);
    }

    #[test]
    fn a_gamma_encoded_scan_measures_the_transmission_the_print_uses() {
        // Pale Y sits in the rebate window in the file and falls out of it after gamma 2.2.
        // Classifying the linearized pixel would keep the old base after the frame was found.
        let orange = [0.80f32, 0.38, 0.14];
        let pale = [0.659f32, 0.621, 0.596];
        assert!(is_pale_rebate(pale));
        let strip = |border: [f32; 3], d0: f32, span: f32| {
            Rgb32f::from_fn(48, 36, |x, y| {
                if x < 6 || y < 6 || x >= 42 || y >= 30 {
                    border
                } else {
                    let d = d0 + span * (x - 6) as f32 / 36.0;
                    border.map(|c| c * 10f32.powf(-d))
                }
            })
        };
        let orange_img = strip(orange, 0.25, 0.6);
        let pale_img = strip(pale, 0.35, 0.5);
        let y = |p: [f32; 3]| 0.2627 * p[0] + 0.6780 * p[1] + 0.0593 * p[2];
        for capture in [FilmCapture::Gamma18, FilmCapture::Gamma22] {
            let seen = pale.map(|c| c.powf(capture.gamma() as f32));
            assert!(!is_pale_rebate(seen) && !is_mask_orange(seen), "{}", capture.label());
            for (border, img) in [(orange, &orange_img), (pale, &pale_img)] {
                let analysis = analyze_image(img, &Film { capture, ..Film::default() });
                if border == pale {
                    assert!(analysis.frame_error.is_none() && !analysis.film.frame.is_full(), "{}", capture.label());
                }
                assert!(analysis.stats.mask_measured, "{border:?} {}", capture.label());
                let base = analysis.film.base();
                let picked = transmission(border.map(|v| v as f64), capture);
                for c in 0..3 {
                    assert!((base[c] - picked[c]).abs() < 0.05, "{:?} base {base:?} transmission {picked:?}", capture);
                }
                let mask = FilmKernel::from_film(&analysis.film).apply(border);
                assert!(y(mask) < 0.08, "{} rebate stays black {mask:?} base {base:?}", capture.label());
            }
        }
        let linear = analyze_image(&orange_img, &Film::default());
        assert!((linear.film.base()[0] - orange[0] as f64).abs() < 0.05, "linear keeps the file value {:?}", linear.film.base());
    }

    #[test]
    fn a_camera_matrix_roundtrips_and_a_rendered_file_has_none() {
        use lightcraft_color::D65;
        use lightcraft_raw::ColorData;
        let tags = ColorData {
            illuminant: [21, 0],
            color_matrix: [Some(lightcraft_color::Mat3([[0.64, -0.12, -0.02], [-0.32, 1.18, 0.10], [-0.04, 0.16, 0.72]])), None],
            ..ColorData::default()
        };
        let info = crate::SourceInfo {
            raw: true,
            camera_color: Some(std::sync::Arc::new(crate::CameraColor { tags, developed_for: D65 })),
            ..crate::SourceInfo::default()
        };
        let space = scanner_space(&info).expect("a real matrix");
        let px = [0.62f32, 0.31, 0.14];
        let mut img = Rgb32f::filled(2, 2, px);
        apply_matrix(&mut img, &space.to_working);
        apply_matrix(&mut img, &space.to_camera);
        for c in 0..3 {
            assert!((img.data[0][c] - px[c]).abs() < 2e-4, "channel {c}: {:?} vs {px:?}", img.data[0]);
        }
        assert!(scanner_space(&crate::SourceInfo::default()).is_none());
    }

    #[test]
    fn a_thin_orange_negative_keeps_the_sprocket_frame_and_stays_above_black() {
        let (w, h) = (120usize, 90usize);
        let film_px = [0.80f32, 0.35, 0.12];
        let img = Rgb32f::from_fn(w, h, |x, y| {
            let hole_row = (4..18).contains(&y) || (72..86).contains(&y);
            let hole = hole_row && (x % 12) < 5 && x > 4 && x + 4 < w;
            if hole {
                [0.98, 0.98, 0.96]
            } else if x < 10 || y < 24 || x >= 110 || y >= 66 {
                if x < 2 || y < 2 || x + 2 >= w || y + 2 >= h { [0.02, 0.01, 0.0] } else { film_px }
            } else {
                // Still the mask colour, so the rebate flood swallows the picture. A patch is denser.
                let d = if (40..70).contains(&x) && (40..55).contains(&y) { 1.15 } else { 0.22 };
                film_px.map(|c| c * 10f32.powf(-d))
            }
        });
        // A flat rebate still has an inner edge when the picture is a step darker. A frame that
        // cannot find that edge leaves the sprocket gate to place the picture.
        if let Some(frame) = detect_frame(&img, FilmStock::ColorNeg) {
            assert!(inside_frame(&frame, 0.5, 0.5), "picture {frame:?}");
            assert!(!inside_frame(&frame, 4.0 / w as f64, 10.0 / h as f64), "rebate was the picture {frame:?}");
        }
        let analysis = analyze_image(&img, &Film::default());
        assert!(analysis.frame_error.is_none(), "{:?}", analysis.frame_error);
        assert!(!analysis.film.frame.is_full(), "the sprocket gate was ignored");
        let k = FilmKernel::from_film(&analysis.film);
        let y = |p: [f32; 3]| 0.2627 * p[0] + 0.6780 * p[1] + 0.0593 * p[2];
        let body = k.apply(film_px.map(|c| c * 10f32.powf(-0.22)));
        let hi = k.apply(film_px.map(|c| c * 10f32.powf(-1.15)));
        assert!(y(body) > 0.08, "the picture printed black {body:?} ev {} grade {}", analysis.film.exposure, analysis.film.grade);
        assert!(y(hi) > y(body), "the dense patch is darker than the field {hi:?} {body:?}");
    }

    #[test]
    fn the_middle_prints_at_middle_grey_and_the_bright_end_stays_on_the_paper() {
        let orange = [0.80f32, 0.38, 0.14];
        let img = Rgb32f::from_fn(90, 60, |x, y| {
            if x < 6 || y < 6 || x >= 84 || y >= 54 {
                orange
            } else {
                let d = 0.12 + 1.2 * (x - 6) as f32 / 77.0;
                orange.map(|c| c * 10f32.powf(-d))
            }
        });
        let analysis = analyze_image(&img, &Film { scan: FilmScan::Flat, ..Film::default() });
        let k = FilmKernel::from_film(&analysis.film);
        let y = |p: [f32; 3]| 0.2627 * p[0] + 0.6780 * p[1] + 0.0593 * p[2];
        let mut ys = Vec::new();
        for (i, p) in img.data.iter().enumerate() {
            let x = (i % 90) as f64 / 90.0;
            let py = (i / 90) as f64 / 60.0;
            if inside_frame(&analysis.film.frame, x, py) {
                ys.push(y(k.apply(*p)));
            }
        }
        assert!(ys.len() > 8, "the frame held nothing");
        ys.sort_by(|a, b| a.total_cmp(b));
        let at = |q: f32| ys[((ys.len() - 1) as f32 * q).round() as usize];
        let (mid, bright) = (at(0.50), at(0.98));
        assert!((mid - 0.18).abs() < 0.06, "middle {mid} ev {} grade {}", analysis.film.exposure, analysis.film.grade);
        assert!(bright < 1.35 && bright > 0.45, "bright end {bright} shoulder {}", analysis.film.shoulder);
    }

    #[test]
    fn a_crop_is_the_whole_measurement_and_the_surround_is_not() {
        let orange = [0.80f32, 0.38, 0.14];
        let junk = [0.15f32, 0.75, 0.20];
        let picture = orange.map(|c| c * 10f32.powf(-0.45));
        let img = Rgb32f::from_fn(80, 40, |x, y| {
            if x < 24 {
                junk
            } else if x >= 76 || !(3..37).contains(&y) {
                orange
            } else {
                picture
            }
        });
        let crop = CropGeometry { rect: Rect::new(24.0 / 80.0, 0.0, 1.0, 1.0), angle: 0.0 };
        let cut = within_crop(&img, crop);
        assert!(cut.width < img.width, "crop kept the surround");
        assert!(cut.data.iter().all(|p| p[1] < 0.5), "the green surround was copied in");
        let film = Film { scan: FilmScan::Flat, ..Film::default() };
        let full = analyze_image(&img, &film);
        let part = analyze_picture(&cut, &film);
        assert!(part.frame_error.is_none(), "a crop with no separate rebate is not an error");
        let y = |p: [f32; 3]| 0.2627 * p[0] + 0.6780 * p[1] + 0.0593 * p[2];
        let blown = y(FilmKernel::from_film(&full.film).apply(picture));
        let placed = y(FilmKernel::from_film(&part.film).apply(picture));
        assert!((placed - 0.18).abs() < 0.08, "the crop did not place the picture {placed}");
        assert!((blown - placed).abs() < 0.05, "the surround moved the print {blown} vs {placed}");
    }

    #[test]
    fn a_thin_subject_keeps_shadow_tone_instead_of_a_black_cliff() {
        let rebate = [0.80f32, 0.36, 0.14];
        // Clearer than the rebate: an underexposed face. The beard is a step denser. The
        // background carries real density. Grain a few percent either side of the face must
        // not jump from black to white.
        let face = [0.88f32, 0.42, 0.17];
        let beard = [0.76f32, 0.33, 0.12];
        let bg = rebate.map(|c| c * 10f32.powf(-0.45));
        let (w, h) = (120usize, 90usize);
        let img = Rgb32f::from_fn(w, h, |x, y| {
            if x < 8 || y < 8 || x + 8 >= w || y + 8 >= h {
                return rebate;
            }
            // The thin subject is most of the frame, the way a backlit portrait is.
            if x >= 96 || y >= 78 {
                bg
            } else if (36..78).contains(&x) && (40..68).contains(&y) {
                beard
            } else {
                face
            }
        });
        let analysis = analyze_image(&img, &Film::default());
        assert!(analysis.film.exposure.abs() < 4.0, "exposure pegged at {}", analysis.film.exposure);
        let k = FilmKernel::from_film(&analysis.film);
        let y = |p: [f32; 3]| 0.2627 * p[0] + 0.6780 * p[1] + 0.0593 * p[2];
        let face_y = y(k.apply(face));
        let beard_y = y(k.apply(beard));
        let bg_y = y(k.apply(bg));
        assert!(bg_y > face_y + 0.08, "background {bg_y} should be lighter than the thin face {face_y}");
        assert!(beard_y > face_y + 0.03, "beard detail collapsed {beard_y} vs face {face_y}");
        assert!(face_y < 0.7, "the thin face blew out {face_y}");
        let up = y(k.apply(face.map(|c| c * 1.04)));
        let down = y(k.apply(face.map(|c| c * 0.96)));
        assert!((up - down).abs() < 0.45, "grain around the face became stipple {up} {down}");
    }

    /// Luminance at three percentiles of the frame, low to high.
    fn print_span(img: &Rgb32f, film: &Film) -> (f32, f32, f32) {
        let k = FilmKernel::from_film(film);
        let y = |p: [f32; 3]| 0.2627 * p[0] + 0.6780 * p[1] + 0.0593 * p[2];
        let mut ys = Vec::new();
        for (i, p) in img.data.iter().enumerate() {
            let x = (i % img.width) as f64 / img.width as f64;
            let py = (i / img.width) as f64 / img.height.max(1) as f64;
            if inside_frame(&film.frame, x, py) {
                ys.push(y(k.apply(*p)));
            }
        }
        assert!(ys.len() > 8, "the frame held nothing");
        ys.sort_by(|a, b| a.total_cmp(b));
        let at = |q: f32| ys[((ys.len() - 1) as f32 * q).round() as usize];
        (at(0.10), at(0.50), at(0.90))
    }

    #[test]
    fn contrast_moves_the_ends_and_holds_the_middle() {
        let orange = [0.80f32, 0.38, 0.14];
        let img = Rgb32f::from_fn(90, 60, |x, y| {
            if x < 6 || y < 6 || x >= 84 || y >= 54 {
                orange
            } else {
                let d = 0.20 + 0.90 * (x - 6) as f32 / 77.0;
                orange.map(|c| c * 10f32.powf(-d))
            }
        });
        let analysis = analyze_image(&img, &Film { scan: FilmScan::Flat, ..Film::default() });
        let (lo, mid, hi) = print_span(&img, &analysis.film);
        assert!((mid - 0.18).abs() < 0.06, "middle {mid}");
        assert!(hi - lo > 0.40, "auto contrast {lo}..{hi}");
        let mut hard = analysis.film.clone();
        hard.grade = (analysis.film.grade * 1.8).clamp(0.2, 6.0);
        let mut soft = analysis.film.clone();
        soft.grade = (analysis.film.grade * 0.55).clamp(0.2, 6.0);
        let (hlo, hmid, hhi) = print_span(&img, &hard);
        let (slo, smid, shi) = print_span(&img, &soft);
        assert!((hmid - mid).abs() < 0.05, "hard grade moved the middle {mid} -> {hmid}");
        assert!((smid - mid).abs() < 0.05, "soft grade moved the middle {mid} -> {smid}");
        assert!(hhi - hlo > hi - lo + 0.04, "harder grade did not add contrast {} vs {}", hhi - hlo, hi - lo);
        assert!(shi - slo + 0.04 < hi - lo, "softer grade did not lose contrast {} vs {}", shi - slo, hi - lo);
    }

    #[test]
    fn a_dense_negative_prints_across_the_scale() {
        for (stock, rebate) in [(FilmStock::BwNeg, [0.82f32, 0.82, 0.82]), (FilmStock::ColorNeg, [0.80, 0.38, 0.14])] {
            let img = Rgb32f::from_fn(90, 60, |x, y| {
                if x < 6 || y < 6 || x >= 84 || y >= 54 {
                    rebate
                } else {
                    let d = 1.05 + 0.30 * (x - 6) as f32 / 77.0;
                    rebate.map(|c| c * 10f32.powf(-d))
                }
            });
            let analysis = analyze_image(&img, &Film { stock, scan: FilmScan::Flat, ..Film::default() });
            let (lo, mid, hi) = print_span(&img, &analysis.film);
            assert!(lo < 0.10, "{stock:?} shadows stayed gray {lo} lift {} grade {}", analysis.film.d_lo, analysis.film.grade);
            assert!(hi > 0.55, "{stock:?} highlights never opened {hi}");
            assert!((mid - 0.18).abs() < 0.08, "{stock:?} middle {mid}");
        }
    }
}
