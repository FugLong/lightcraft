//! From the picture's densities to the print: black, middle, and contrast.
//!
//! **Color.** The rebate is the mask, so fog is 0: unexposed film is zero density. A camera does
//! not record a neutral as equal density after that. The axis is the slope of the least coloured
//! whites, the filter pack a printer sets so those print gray. A colour that sits off that slope
//! stays coloured. A Gray / Shadow / Highlight pick, or Match roll, replaces it ([`Axis::Keep`]).
//!
//! **Tone.** On luminance density, the middle is the trimmed mean (the picture's log-average
//! key), the black anchor is just under the 0.5th percentile (never denser than clear film
//! prints), and contrast is the log slope that puts the 99th percentile at scene 1.0, the top
//! of the paper. When no slope in range gets there, exposure makes up the rest.

use lightcraft_develop::Film;

use super::kernel::{FILM_GAMMA, FilmKernel, GREY};
use super::{percentile, y2020};

const CONTRAST_RANGE: (f64, f64) = (0.25, 4.0);
/// Scene light the bright end of the picture prints at.
const HIGHLIGHT: f64 = 1.0;
/// Least density between the black and the middle, as in [`FilmKernel`]. A thin negative has
/// little; pushing its black under clear film would print its shadows gray.
const MIN_BODY: f64 = 0.05;

/// How the gray axis is chosen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Axis {
    /// Fog 0, and the axis through the neutral midtones. Convert.
    Lab,
    /// Keep this `(fog, axis)`: a roll's, or one the picks set.
    Keep([f64; 3], [f64; 3]),
}

/// A solved print of one frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Print {
    pub fog: [f64; 3],
    pub axis: [f64; 3],
    /// Straightened densities of the black, the middle, and the bright end.
    pub black: f64,
    pub mid: f64,
    pub highlight: f64,
    pub contrast: f64,
    pub exposure: f64,
    /// CIE a* b* of the printed middle, for the cast readout.
    pub mid_a: f64,
    pub mid_b: f64,
}

/// Dye-corrected density of each pixel, exactly as [`FilmKernel::apply`] computes it before the
/// gray axis. The user's channel trims come after the measurement and are not part of it.
pub fn densities(px: &[[f32; 3]], film: &Film) -> Vec<[f64; 3]> {
    let k = FilmKernel::from_film(film);
    px.iter().map(|p| k.analytical_density(*p).map(f64::from)).collect()
}

/// Solve the print of a frame from its densities. `None` with too few pixels to measure.
pub fn solve(d: &[[f64; 3]], axis: Axis, mono: bool) -> Option<Print> {
    if d.len() < 16 {
        return None;
    }
    let (fog, slope) = match axis {
        _ if mono => ([0.0; 3], [1.0; 3]),
        Axis::Lab => ([0.0; 3], neutral_axis(d)),
        Axis::Keep(f, a) => (f, a.map(|v| v.clamp(0.2, 5.0))),
    };
    let mut ys: Vec<f64> = d.iter().map(|p| y2020(straighten(*p, fog, slope))).collect();
    let lo = percentile(&mut ys, 0.005)?;
    let hi = percentile(&mut ys, 0.99)?;
    let (p5, p95) = (percentile(&mut ys, 0.05)?, percentile(&mut ys, 0.95)?);
    let body: Vec<f64> = ys.iter().copied().filter(|y| (p5..=p95).contains(y)).collect();
    let mid = if body.is_empty() { percentile(&mut ys, 0.5)? } else { body.iter().sum::<f64>() / body.len() as f64 };
    // Clear film straightens to `-fog / axis`. Nothing in the picture prints darker than it does.
    let clear = (0..3).map(|c| -fog[c] / slope[c]).fold(f64::MIN, f64::max);
    let black = clear.max(lo - 0.03).min(mid - MIN_BODY);
    let (contrast, exposure) = contrast_for(black, mid, hi);
    let (mid_a, mid_b) = printed_middle(d, fog, slope, black, mid, contrast);
    Some(Print { fog, axis: slope, black, mid, highlight: hi, contrast, exposure, mid_a, mid_b })
}

/// Scene light of a straightened density with this black, middle and contrast.
pub fn scene(x: f64, black: f64, mid: f64, contrast: f64) -> f64 {
    let light = |v: f64| 10f64.powf((contrast * (v - mid) / FILM_GAMMA).clamp(-12.0, 8.0));
    let floor = light(black);
    GREY * (light(x) - floor) / (1.0 - floor).max(1e-9)
}

/// The contrast that prints `hi` at [`HIGHLIGHT`], and the exposure that finishes the job when
/// the contrast range runs out.
fn contrast_for(black: f64, mid: f64, hi: f64) -> (f64, f64) {
    if hi - mid < 0.02 {
        return (1.0, 0.0);
    }
    let (mut lo_k, mut hi_k) = CONTRAST_RANGE;
    for _ in 0..40 {
        let k = 0.5 * (lo_k + hi_k);
        if scene(hi, black, mid, k) > HIGHLIGHT {
            hi_k = k;
        } else {
            lo_k = k;
        }
    }
    let k = 0.5 * (lo_k + hi_k);
    let ev = -(scene(hi, black, mid, k).max(1e-6) / HIGHLIGHT).log2();
    let ev = if ev.abs() < 0.01 { 0.0 } else { ev.clamp(-2.0, 2.0) };
    (k, ev)
}

/// Slope of the least coloured whites, normalised so it averages 1.
///
/// Fog stays 0, so this is a line through clear film. Equal densities stay at 1. A handful of
/// pixels, or a cluster sitting on the base, has no slope and stays at 1.
fn neutral_axis(d: &[[f64; 3]]) -> [f64; 3] {
    let mut ys: Vec<f64> = d.iter().map(|p| y2020(*p)).filter(|y| y.is_finite()).collect();
    // The whites, not the middle. A cast the paper leaves in the highlights is what a print is
    // judged by, and skin sits further down the scale.
    let (Some(lo), Some(hi)) = (percentile(&mut ys, 0.72), percentile(&mut ys, 0.96)) else { return [1.0; 3] };
    let near: Vec<[f64; 3]> = d.iter().copied().filter(|p| p.iter().all(|c| c.is_finite()) && (lo..=hi).contains(&y2020(*p))).collect();
    if near.len() < 16 {
        return [1.0; 3];
    }
    let chroma = |p: [f64; 3]| (p[0] - p[1]).hypot(p[2] - p[1]);
    // Close to the least coloured, not a fraction of the frame: a sky beside a gray card stays
    // out, and a frame whose every neutral carries the same slope stays in.
    let min_c = near.iter().copied().map(chroma).fold(f64::MAX, f64::min);
    let tol = (min_c * 0.25).max(0.02);
    let gray: Vec<[f64; 3]> = near.iter().copied().filter(|p| chroma(*p) <= min_c + tol).collect();
    let used = if gray.len() >= 16 { &gray[..] } else { &near[..] };
    let n = used.len() as f64;
    let mean = [0, 1, 2].map(|c| used.iter().map(|p| p[c]).sum::<f64>() / n);
    let scale = (mean[0] + mean[1] + mean[2]) / 3.0;
    if !scale.is_finite() || scale < 0.05 {
        return [1.0; 3];
    }
    mean.map(|v| if v.is_finite() { (v / scale).clamp(0.2, 5.0) } else { 1.0 })
}

pub fn straighten(d: [f64; 3], fog: [f64; 3], axis: [f64; 3]) -> [f64; 3] {
    [0, 1, 2].map(|c| (d[c] - fog[c]) / axis[c].max(1e-3))
}

/// a* b* of the grayest pixels printed nearest the middle — leftover cast, not the scene.
fn printed_middle(d: &[[f64; 3]], fog: [f64; 3], axis: [f64; 3], black: f64, mid: f64, contrast: f64) -> (f64, f64) {
    let near: Vec<[f64; 3]> = d
        .iter()
        .copied()
        .filter(|p| {
            let s = straighten(*p, fog, axis);
            (y2020(s) - mid).abs() <= 0.05
        })
        .collect();
    if near.len() < 8 {
        return (0.0, 0.0);
    }
    let chroma = |p: [f64; 3]| {
        let s = straighten(p, fog, axis);
        (s[0] - s[1]).hypot(s[2] - s[1])
    };
    let mut ch: Vec<f64> = near.iter().copied().map(chroma).collect();
    let cut = percentile(&mut ch, 0.25).unwrap_or(f64::MAX);
    let gray: Vec<[f64; 3]> = near.iter().copied().filter(|p| chroma(*p) <= cut).collect();
    let used = if gray.len() >= 8 { &gray[..] } else { &near[..] };
    let (mut a, mut b, mut n) = (0.0, 0.0, 0.0);
    for p in used {
        let s = straighten(*p, fog, axis);
        let (pa, pb) = lab_ab(s.map(|v| scene(v, black, mid, contrast).max(0.0)));
        a += pa;
        b += pb;
        n += 1.0;
    }
    if n < 8.0 { (0.0, 0.0) } else { (a / n, b / n) }
}

fn lab_ab(rgb: [f64; 3]) -> (f64, f64) {
    let xyz = lightcraft_color::REC2020.to_xyz().apply(rgb);
    let f = |t: f64| if t > 0.008856 { t.cbrt() } else { 7.787037 * t + 16.0 / 116.0 };
    let (fx, fy, fz) = (f(xyz[0] / 0.95047), f(xyz[1]), f(xyz[2] / 1.08883));
    (500.0 * (fx - fy), 200.0 * (fy - fz))
}
