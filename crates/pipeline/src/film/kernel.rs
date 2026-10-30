//! The per-pixel conversion. `map.wgsl`'s `film_k` is the same function; keep the two in step.

use lightcraft_color::luminance_2020;
use lightcraft_develop::{Film, FilmScan};

/// Straight-line gamma of a colour negative: density per decade of exposure. Contrast 1 prints
/// the negative at this gamma, so one decade of scene light is one decade of print light.
pub const FILM_GAMMA: f64 = 0.60;
/// Scene-linear middle grey. The middle density of the frame prints here.
pub const GREY: f64 = 0.18;
/// Below this scene level the print's black rolls off smoothly instead of clipping, so a shadow a
/// little under the black point is a dark tone, not a hard edge of zeros.
const TOE: f32 = 0.002;
const HEADROOM: f32 = 64.0;
/// Density range kept from the scan. Past clear film is light from the lamp, not the picture.
const D_MIN: f32 = -1.0;
const D_MAX: f32 = 5.0;
const EPS: f32 = 1e-6;
/// Number of `f32`/`u32` words in [`FilmKernel::words`].
pub const WORDS: usize = 45;

/// Parameters of [`FilmKernel::apply`], resolved from [`Film`] once per render.
#[derive(Clone, Copy, Debug)]
pub struct FilmKernel {
    pub base: [f32; 3],
    /// Per-channel contrast. On a negative, a slope on straightened density about the middle,
    /// so the middle stays gray; on a slide, a power on the transmission.
    pub trim: [f32; 3],
    pub negative: bool,
    pub mono: bool,
    pub capture: f32,
    /// Integral to analytical density, row-major ([`dye_matrix`]).
    pub dye: [[f32; 3]; 3],
    /// Gray axis `D_c = fog_c + axis_c · D_green`: offset and slope that straighten each channel.
    pub fog: [f32; 3],
    pub axis: [f32; 3],
    /// `grade`. On a negative, log-slope of scene light per straightened density, relative to
    /// [`FILM_GAMMA`]. On a slide, a power on the positive.
    pub contrast: f32,
    /// Straightened density printed at [`GREY`].
    pub mid: f32,
    /// Straightened density printed black.
    pub black: f32,
    pub gain: f32,
    pub offset: f32,
    /// Gain of the print look, at constant luminance.
    pub aim: [f32; 3],
    /// Saturation of the positive about its luminance: the look's, times the Saturation slider.
    pub chroma: f32,
    pub balance: [f32; 3],
    pub shadow: [f32; 3],
    pub midtone: [f32; 3],
    pub high: [f32; 3],
    pub shoulder: f32,
}

impl FilmKernel {
    pub fn from_film(f: &Film) -> Self {
        let mono = f.stock.mono();
        let finite = |v: f64, d: f64| if v.is_finite() { v } else { d };
        let (aim, look_chroma) = f.print.aim();
        let mid = finite(f.pivot, 0.0).clamp(-1.0, 4.0);
        let saturation = (1.0 + finite(f.presat, 0.0) / 100.0).clamp(0.0, 3.0);
        Self {
            base: f.base().map(|v| finite(v, 1.0).clamp(1e-4, 4.0) as f32),
            trim: if mono { [1.0; 3] } else { f.gamma().map(|v| finite(v, 1.0).clamp(0.05, 4.0) as f32) },
            negative: f.stock.negative(),
            mono,
            capture: f.capture.gamma() as f32,
            dye: if mono { dye_matrix(FilmScan::Flat) } else { dye_matrix(f.scan) },
            fog: if mono { [0.0; 3] } else { f.fog().map(|v| finite(v, 0.0).clamp(-2.0, 2.0) as f32) },
            axis: if mono { [1.0; 3] } else { f.axis().map(|v| finite(v, 1.0).clamp(0.2, 5.0) as f32) },
            contrast: finite(f.grade, 1.0).clamp(0.05, 6.0) as f32,
            mid: mid as f32,
            // The black has to sit under the middle, or the print has no tones between them.
            black: finite(f.d_lo, 0.0).min(mid - 0.05) as f32,
            gain: 2f64.powf(finite(f.exposure, 0.0).clamp(-10.0, 10.0)) as f32,
            offset: finite(f.black, 0.0).clamp(-0.5, 1.0) as f32,
            aim: if mono { [1.0; 3] } else { unit_luminance(aim.map(|v| v as f32)) },
            chroma: if mono { 1.0 } else { (look_chroma * saturation) as f32 },
            balance: if mono { [1.0; 3] } else { band_gain(f.temp, f.tint) },
            shadow: if mono { [1.0; 3] } else { band_gain(f.shadow_temp, f.shadow_tint) },
            midtone: if mono { [1.0; 3] } else { band_gain(f.mid_temp, f.mid_tint) },
            high: if mono { [1.0; 3] } else { band_gain(f.high_temp, f.high_tint) },
            shoulder: finite(f.shoulder, 0.0).max(0.0) as f32,
        }
    }

    /// `P[1..]` of `film_k` (`P[0]` is the pixel count, added by the dispatcher).
    pub fn words(&self) -> Vec<u32> {
        fn put(w: &mut Vec<u32>, vs: &[f32]) {
            w.extend(vs.iter().map(|v| v.to_bits()));
        }
        let mut w = Vec::with_capacity(WORDS);
        put(&mut w, &self.base);
        put(&mut w, &self.trim);
        w.push(u32::from(self.negative) | (u32::from(self.mono) << 1));
        put(&mut w, &[self.capture]);
        for row in &self.dye {
            put(&mut w, row);
        }
        put(&mut w, &self.fog);
        put(&mut w, &self.axis);
        put(&mut w, &[self.contrast, self.mid, self.black, self.gain, self.offset]);
        put(&mut w, &self.aim);
        put(&mut w, &[self.chroma]);
        put(&mut w, &self.balance);
        put(&mut w, &self.shadow);
        put(&mut w, &self.midtone);
        put(&mut w, &self.high);
        put(&mut w, &[self.shoulder]);
        w
    }

    /// Dye-corrected density of one pixel: the quantity the gray axis and the paper curve see.
    pub fn analytical_density(&self, t: [f32; 3]) -> [f32; 3] {
        let t = t.map(|c| capture_channel(c, self.capture));
        if self.mono {
            let y = luminance_2020(t);
            let b = luminance_2020(self.base).max(1e-4);
            return [density(y, b).clamp(D_MIN, D_MAX); 3];
        }
        let d0 = [0, 1, 2].map(|i| density(t[i], self.base[i]).clamp(D_MIN, D_MAX));
        analytical(&self.dye, d0)
    }

    pub fn apply(&self, t: [f32; 3]) -> [f32; 3] {
        let scene = if self.negative {
            let d = self.analytical_density(t);
            let scene = [0, 1, 2].map(|i| {
                let s = (d[i] - self.fog[i]) / self.axis[i];
                self.print(self.mid + self.trim[i] * (s - self.mid)) * self.aim[i]
            });
            saturate(scene, self.chroma).map(|c| c * self.gain + self.offset)
        } else {
            let mut t = t.map(|c| capture_channel(c, self.capture));
            let base = if self.mono {
                let y = luminance_2020(t);
                t = [y; 3];
                [luminance_2020(self.base).max(1e-4); 3]
            } else {
                self.base
            };
            let v = [0, 1, 2].map(|i| {
                let v = ((t[i] / base[i]).max(EPS).powf(self.trim[i]) * self.gain + self.offset).max(0.0);
                if (self.contrast - 1.0).abs() > 1e-4 { v.powf(self.contrast) } else { v }
            });
            saturate(v, self.chroma)
        };
        self.finish(scene)
    }

    /// Scene light of one straightened density: an exponential about the middle, with the black
    /// anchor subtracted so it prints at zero and the middle still prints at [`GREY`].
    fn print(&self, s: f32) -> f32 {
        let slope = self.contrast / FILM_GAMMA as f32;
        let light = |x: f32| 10f32.powf((slope * (x - self.mid)).clamp(-12.0, 8.0));
        let floor = light(self.black);
        let v = GREY as f32 * (light(s) - floor) / (1.0 - floor).max(1e-6);
        toe(v).min(HEADROOM)
    }

    /// Temperature and tint, overall and per tonal band, then the shoulder. The bands are in stops
    /// about middle gray: shadows below two stops under, highlights from half a stop over.
    fn finish(&self, scene: [f32; 3]) -> [f32; 3] {
        let scene = scene.map(|c| c.max(0.0));
        let ev = (luminance_2020(scene).max(1e-6) / GREY as f32).log2();
        let sh = 1.0 - smooth(-3.0, -0.5, ev);
        let hi = smooth(0.5, 2.5, ev);
        let mid = (1.0 - sh - hi).max(0.0);
        [0, 1, 2].map(|i| {
            let band = sh * self.shadow[i] + mid * self.midtone[i] + hi * self.high[i];
            let mut v = scene[i] * (self.balance[i] * band).max(0.0);
            if self.shoulder > 1e-4 && v > self.shoulder {
                let e = v - self.shoulder;
                v = self.shoulder + e / (1.0 + e / self.shoulder);
            }
            v.min(HEADROOM)
        })
    }
}

/// `c` with its distance from its own luminance scaled by `k`, but no further than a channel
/// reaching zero, so hue and luminance are unchanged.
fn saturate(c: [f32; 3], k: f32) -> [f32; 3] {
    if (k - 1.0).abs() <= 1e-4 {
        return c;
    }
    let y = luminance_2020(c);
    let room = c.iter().filter(|v| **v < y).map(|v| y / (y - v)).fold(k, f32::min);
    c.map(|v| (y + (v - y) * room).max(0.0))
}

/// `g` scaled to unit luminance, so a colour gain moves hue, not brightness.
fn unit_luminance(g: [f32; 3]) -> [f32; 3] {
    let y = luminance_2020(g);
    if y > 1e-6 && y.is_finite() { g.map(|v| v / y) } else { [1.0; 3] }
}

/// `log10(base / t)`: density above clear film. Brighter than the base is negative.
pub(crate) fn density(t: f32, base: f32) -> f32 {
    (base.max(EPS) / t.max(EPS)).log10()
}

fn toe(v: f32) -> f32 {
    if v >= TOE { v } else { TOE * ((v - TOE) / TOE).max(-60.0).exp() }
}

pub(crate) fn mat(m: &[[f32; 3]; 3], v: [f32; 3]) -> [f32; 3] {
    m.map(|row| row[0] * v[0] + row[1] * v[1] + row[2] * v[2])
}

/// Integral to analytical density (Hanson, J. Opt. Soc. Am. 40, 1950).
///
/// A gray subject lays down equal dye amounts, but a camera's wide filters see the dyes' side
/// absorption, so the *integral* densities are not equal. Hanson undoes that: with
/// `(cg, mb, cb)` = (cyan in green, magenta in blue, cyan in blue) and yellow's side absorptions
/// taken as zero, `C = Dr`, `M = Dg − cg·C`, `Y = Db − cb·C − mb·M`. Rows are not scaled: scaling
/// them to sum to one would keep an equal-*integral* gray gray, which is the wrong gray, and the
/// extra weight on blue is what printed a cyan-blue cast on every conversion.
pub fn dye_matrix(scan: FilmScan) -> [[f32; 3]; 3] {
    let [cg, mb, cb] = scan.dye().map(|v| v as f32);
    [[1.0, 0.0, 0.0], [-cg, 1.0, 0.0], [mb * cg - cb, -mb, 1.0]]
}

/// Hanson inverse of integral density, clamped to a physical dye amount.
pub(crate) fn analytical(dye: &[[f32; 3]; 3], d: [f32; 3]) -> [f32; 3] {
    mat(dye, d).map(|v| v.max(0.0))
}

/// ±0.40 density at the end of the slider (about 2.5×) between red and blue, at constant
/// luminance. Positive temperature adds red and removes blue. Positive tint removes green.
fn band_gain(temp: f64, tint: f64) -> [f32; 3] {
    let t = if temp.is_finite() { (temp / 100.0).clamp(-1.0, 1.0) as f32 } else { 0.0 };
    let n = if tint.is_finite() { (tint / 100.0).clamp(-1.0, 1.0) as f32 } else { 0.0 };
    let warm = 10f32.powf(0.40 * t);
    let green = 10f32.powf(-0.40 * n);
    unit_luminance([warm, green, 1.0 / warm])
}

pub(crate) fn capture_channel(c: f32, gamma: f32) -> f32 {
    if !c.is_finite() || c < 0.0 {
        return 0.0;
    }
    if gamma > 1.001 { c.powf(gamma) } else { c }
}

fn smooth(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}
