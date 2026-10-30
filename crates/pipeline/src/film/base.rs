//! Clear film (the base) and the lamp.
//!
//! Convert measures the base from the rebate ring around the picture ([`from_rebate`]). The
//! whole-scan histogram ([`estimate`]) is only the first pass that classifies holder and lamp
//! so the edges can be walked: blank film is a tall narrow luminance peak, the lamp sits above
//! a sparse valley, and the first guess is the brightest tall peak below that valley.

use lightcraft_develop::FilmStock;

use super::{log_histogram, percentile, y2020};

/// A channel this close to 1 (the white of the encoding) may have clipped.
const CEILING: f64 = 0.95;
/// Red / blue of blank colour negative. The synthetic rebate is about 5; a pale fringe is near 1.
/// Below this the orange mask is not in the sample, and printing it comes out blue.
const ORANGE: f32 = 1.8;

/// What the histogram says about the bright end of a scan.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Base {
    /// Transmission of clear film, per channel.
    pub rgb: [f64; 3],
    /// Luminance from which pixels are lamp, not film. `None` when no lamp cluster stands apart.
    pub lamp_cut: Option<f64>,
    /// Luminance at which a channel clips.
    pub clip: f64,
}

impl Base {
    pub fn luminance(&self) -> f64 {
        y2020(self.rgb)
    }

    /// Brighter than film can be: past the base by a quarter in luminance or in any one channel
    /// (a negative never passes more light than its own clear film), clipped, or in the lamp
    /// cluster.
    pub fn is_lamp(&self, p: [f32; 3]) -> bool {
        let y = y2020(p.map(f64::from));
        let past = (0..3).any(|c| f64::from(p[c]) > self.rgb[c] * 1.25);
        past || y > self.luminance() * 1.25 || self.is_clip(p) || self.lamp_cut.is_some_and(|cut| y >= cut)
    }

    /// Clipped encoding white: a lamp hole, not film. Does not depend on Dmin, so a dark guess
    /// cannot throw the picture away.
    pub fn is_clip(&self, p: [f32; 3]) -> bool {
        if !p.iter().all(|c| c.is_finite()) {
            return true;
        }
        let max = f64::from(p[0].max(p[1]).max(p[2]));
        max >= self.clip.min(CEILING) * 0.985 || max >= 0.98
    }
}

/// Red well above blue: the orange mask. A pale or neutral pixel is not blank colour film.
pub(crate) fn is_orange(p: [f32; 3]) -> bool {
    if !p.iter().all(|c| c.is_finite() && *c >= 0.0) {
        return false;
    }
    let r = p[0];
    r > 1e-3 && r > p[2] * ORANGE
}

/// The base the edge walk classifies against. On a colour negative that is the brightest orange,
/// so a pale fringe is lamp and the picture — darker orange — is not blank film.
pub fn classify(px: &[[f32; 3]], stock: FilmStock) -> Option<Base> {
    let guess = estimate(px)?;
    if !(stock.negative() && !stock.mono()) {
        return Some(guess);
    }
    let orange: Vec<[f32; 3]> = px.iter().copied().filter(|p| !guess.is_clip(*p) && is_orange(*p) && y2020(p.map(f64::from)) > 0.02).collect();
    if orange.len() < 16 {
        return Some(guess);
    }
    let band = brightest_peak(&orange);
    chroma_mode(&band).map(|rgb| Base { rgb, lamp_cut: guess.lamp_cut, clip: guess.clip }).or(Some(guess))
}

/// Clear film of the pixels in `px`, or `None` when there are too few to tell.
pub fn estimate(px: &[[f32; 3]]) -> Option<Base> {
    let mut maxes: Vec<f64> = px.iter().map(|p| f64::from(p[0].max(p[1]).max(p[2]))).filter(|v| v.is_finite()).collect();
    let top = percentile(&mut maxes, 0.999)?;
    // Only a scan that reaches the top of its encoding has clipped. Below it, the brightest
    // pixels are real: often the base itself, when the holder hides the lamp.
    let clip = if top >= CEILING { top } else { f64::MAX };
    let unclipped: Vec<[f32; 3]> = px
        .iter()
        .copied()
        .filter(|p| p.iter().all(|c| c.is_finite()) && f64::from(p[0].max(p[1]).max(p[2])) < clip * 0.985)
        .filter(|p| y2020(p.map(f64::from)) > 1e-4)
        .collect();
    if unclipped.len() < 64 {
        return None;
    }
    let ys: Vec<f64> = unclipped.iter().map(|p| y2020(p.map(f64::from))).collect();
    let lamp_cut = lamp_cut(&ys);
    let film: Vec<[f32; 3]> = unclipped.iter().zip(&ys).filter(|(_, y)| lamp_cut.is_none_or(|cut| **y < cut)).map(|(p, _)| *p).collect();
    let candidates = brightest_peak(&film);
    let rgb = chroma_mode(&candidates)?;
    Some(Base { rgb, lamp_cut, clip })
}

/// Dmin of the rebate ring: the chroma of unexposed film, not the palest fringe and not a
/// whole-scan histogram. A colour negative with no orange population is `None` — that histogram
/// printed dark and blue. Other stocks keep the previous fallback.
pub fn from_rebate(px: &[[f32; 3]], stock: FilmStock, guess: Base) -> Option<Base> {
    let film: Vec<[f32; 3]> = px.iter().copied().filter(|p| !lamp_like(*p, guess.clip) && !guess.is_clip(*p)).collect();
    let carry = |rgb: [f64; 3]| Base { rgb, lamp_cut: guess.lamp_cut, clip: guess.clip };
    if stock.negative() && !stock.mono() {
        let orange: Vec<[f32; 3]> = film.iter().copied().filter(|p| is_orange(*p)).collect();
        if orange.len() < 16 {
            return None;
        }
        return chroma_mode(&orange).map(carry);
    }
    if film.len() < 16 {
        return Some(guess);
    }
    chroma_mode(&film).map(carry).or(Some(guess))
}

/// Clipped white, empty, or a near-neutral glow: lamp, not film.
fn lamp_like(p: [f32; 3], clip: f64) -> bool {
    if !p.iter().all(|c| c.is_finite()) {
        return true;
    }
    let max = f64::from(p[0].max(p[1]).max(p[2]));
    let min = f64::from(p[0].min(p[1]).min(p[2]));
    if max < 1e-4 || max >= clip.min(CEILING) * 0.985 {
        return true;
    }
    max > 0.55 && (max - min) / max < 0.12
}

/// The luminance of the valley under the lamp cluster.
///
/// The highest valley with a real cluster on each side, the two clusters at least 1.3× apart, and
/// at least half the scan below it. Below the lamp there is blank film, a peak at least half as
/// tall as the lamp's; without one, the bright cluster is the base over a picture with no thin
/// shadows. The scan is smoothed first, so grain does not make valleys.
fn lamp_cut(ys: &[f64]) -> Option<f64> {
    const BINS: usize = 64;
    let hist = log_histogram(ys, 0.5, BINS, &[1.0, 2.0, 3.0, 2.0, 1.0])?;
    if hist.span() < 1.3f64.ln() {
        return None;
    }
    let n = ys.len() as f64;
    let h = &hist.counts;
    for j in (2..BINS.saturating_sub(2)).rev() {
        let (lower, upper) = (h.get(..j)?, h.get(j..)?);
        let (i_lo, lo_max) = argmax(lower)?;
        let (i_hi, hi_max) = argmax(upper)?;
        let (Some(&here), Some(&left), Some(&right)) = (h.get(j), h.get(j - 1), h.get(j + 1)) else { continue };
        if upper.iter().sum::<f64>() < 0.002 * n || lo_max <= 0.0 {
            continue;
        }
        let deep = here < 0.25 * lo_max.min(hi_max) && here <= left && here <= right && lo_max >= 0.5 * hi_max;
        let apart = hist.center(j + i_hi) - hist.center(i_lo) >= 1.3f64.ln();
        let under = ys.iter().filter(|y| y.max(1e-5).ln() < hist.edge(j)).count() as f64 >= 0.5 * n;
        if deep && apart && under {
            return Some(hist.center(j).exp());
        }
    }
    None
}

/// Pixels of the brightest histogram peak at least a quarter as tall as the tallest one.
/// A pale plateau of lamp falloff or blur is not a peak; uniform blank film is.
fn brightest_peak(film: &[[f32; 3]]) -> Vec<[f32; 3]> {
    const BINS: usize = 80;
    let ys: Vec<f64> = film.iter().map(|p| y2020(p.map(f64::from))).collect();
    let Some(hist) = log_histogram(&ys, 0.5, BINS, &[1.0, 2.0, 1.0]) else { return film.to_vec() };
    let h = &hist.counts;
    let at = |i: usize| h.get(i).copied().unwrap_or(0.0);
    let peaks: Vec<usize> = (0..BINS).filter(|&i| at(i) > 0.0 && at(i) >= at(i.saturating_sub(1)) && at(i) >= at(i + 1)).collect();
    let tallest = peaks.iter().map(|&i| at(i)).fold(0.0, f64::max);
    let Some(top) = peaks.iter().copied().filter(|&i| at(i) >= 0.25 * tallest).max() else { return film.to_vec() };
    let (lo, hi) = (hist.edge(top.saturating_sub(1)), hist.edge((top + 2).min(BINS)));
    let band: Vec<[f32; 3]> = film.iter().zip(&ys).filter(|(_, y)| (lo..hi).contains(&y.max(1e-5).ln())).map(|(p, _)| *p).collect();
    if band.len() >= 16 {
        return band;
    }
    let mut sorted = ys.clone();
    let floor = percentile(&mut sorted, 0.97).unwrap_or(0.0);
    film.iter().zip(&ys).filter(|(_, y)| **y >= floor).map(|(p, _)| *p).collect()
}

/// Median colour near the most common chromaticity, so a stray picture pixel at the same
/// luminance does not tint the base.
fn chroma_mode(px: &[[f32; 3]]) -> Option<[f64; 3]> {
    const BINS: usize = 24;
    let chroma: Vec<(f64, f64)> = px
        .iter()
        .map(|p| {
            let g = f64::from(p[1]).max(1e-6);
            ((f64::from(p[0]).max(1e-6) / g).ln(), (f64::from(p[2]).max(1e-6) / g).ln())
        })
        .collect();
    let range = |f: fn(&(f64, f64)) -> f64| chroma.iter().map(f).fold((f64::MAX, f64::MIN), |(lo, hi), v| (lo.min(v), hi.max(v)));
    let ((x0, x1), (y0, y1)) = (range(|c| c.0), range(|c| c.1));
    if !(x0.is_finite() && x1.is_finite() && y0.is_finite() && y1.is_finite()) {
        return None;
    }
    let (sx, sy) = (((x1 - x0) / BINS as f64).max(1e-6), ((y1 - y0) / BINS as f64).max(1e-6));
    let cell = |c: &(f64, f64)| {
        let i = (((c.0 - x0) / sx) as usize).min(BINS - 1);
        let j = (((c.1 - y0) / sy) as usize).min(BINS - 1);
        i * BINS + j
    };
    let mut counts = vec![0u32; BINS * BINS];
    for c in &chroma {
        if let Some(slot) = counts.get_mut(cell(c)) {
            *slot += 1;
        }
    }
    let (best, _) = counts.iter().enumerate().max_by_key(|(_, n)| **n)?;
    let (cx, cy) = (x0 + ((best / BINS) as f64 + 0.5) * sx, y0 + ((best % BINS) as f64 + 0.5) * sy);
    let near: Vec<[f32; 3]> =
        px.iter().zip(&chroma).filter(|(_, c)| (c.0 - cx).abs() <= 1.5 * sx && (c.1 - cy).abs() <= 1.5 * sy).map(|(p, _)| *p).collect();
    let pick = if near.len() >= 8 { &near } else { px };
    let mut out = [0.0; 3];
    for (c, slot) in out.iter_mut().enumerate() {
        let mut v: Vec<f64> = pick.iter().filter_map(|p| p.get(c)).map(|v| f64::from(*v)).collect();
        *slot = percentile(&mut v, 0.5)?;
    }
    Some(out)
}

fn argmax(v: &[f64]) -> Option<(usize, f64)> {
    v.iter().copied().enumerate().fold(None, |best, (i, x)| match best {
        Some((_, b)) if b >= x => best,
        _ => Some((i, x)),
    })
}
