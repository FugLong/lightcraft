//! Where the picture is on the scan.
//!
//! Every pixel of a small preview is classed against the scan's own base ([`super::base`]):
//! lamp (brighter than film can be), holder (as dark as the darkest part of the scan), or blank
//! film (within 0.05 density of the base in every channel), or near it (within 0.15: clear film
//! away from the sprocket holes, where less lamp flare reaches it). Each edge is then walked
//! inward, one line at a time, reading the central 70 % of the line across the picture the other
//! direction found (so a border down one side is not read into every row):
//!
//! 1. Lines with lamp light in them (an open gate, sprocket holes) are outside: a negative's
//!    picture is never brighter than its own base, in any channel.
//! 2. Holder lines are outside, until blank film has been crossed. After that a dark line is the
//!    picture's dense end (a sky), not the holder.
//! 3. A narrow, flat band of blank film that ends in one step is the rebate. Deep shadows look
//!    like blank film too, but fade in.
//! 4. On a strip, a narrow flat gap of near-blank film further in, with picture past it, is the
//!    edge between this frame and its neighbour.

use lightcraft_develop::{FilmFrame, FilmStock};
use lightcraft_raster::Rgb32f;

use super::base::{self, Base};
use super::{percentile, y2020};

/// Long edge of the preview the edges are walked on.
const PREVIEW: usize = 256;
/// Edges are looked for in the outer 40 % of each side.
const REACH: f64 = 0.4;
/// The picture keeps at least this fraction of the scan in each direction.
const MIN_PICTURE: f64 = 0.3;

/// The picture, and the base it was found against.
#[derive(Clone, Copy, Debug)]
pub struct Detection {
    pub frame: FilmFrame,
    pub base: Base,
    /// Something outside the picture was found: holder, lamp, rebate, or a neighbouring frame.
    pub bordered: bool,
    /// `base` is blank film of the right colour. A holder with no orange rebate is not a mask.
    pub mask: bool,
}

/// Find the picture on `img` (scan-linear, in the space density is measured in).
pub fn detect(img: &Rgb32f, stock: FilmStock) -> Option<Detection> {
    if img.width < 8 || img.height < 8 {
        return None;
    }
    let small = preview(img);
    let base = base::classify(&small.data, stock)?;
    let classes = Classes::new(&small, &base);
    let negative = stock.negative();
    let (sw, sh) = (small.width, small.height);
    let insets_within = |xs: (usize, usize), ys: (usize, usize)| {
        let rows = Lines::of(&small, &classes, true, xs);
        let cols = Lines::of(&small, &classes, false, ys);
        [walk(&cols, negative), walk(&rows, negative), walk(&cols.reversed(), negative), walk(&rows.reversed(), negative)]
    };
    // Each line is read across the picture the other direction found, so a border down one side
    // does not look like sprocket holes along every row.
    let [l, t, r, b] = insets_within((0, sw), (0, sh));
    let insets = insets_within((l, sw.saturating_sub(r).max(l + 1)), (t, sh.saturating_sub(b).max(t + 1)));
    let (w, h) = (sw as f64, sh as f64);
    let [left, top, right, bottom] = insets.map(|n| n as f64);
    let (x0, y0, x1, y1) = (left / w, top / h, 1.0 - right / w, 1.0 - bottom / h);
    if x1 - x0 < MIN_PICTURE || y1 - y0 < MIN_PICTURE {
        return Some(Detection { frame: FilmFrame::default(), base, bordered: false, mask: false });
    }
    let frame = FilmFrame { corners: [[x0, y0], [x1, y0], [x1, y1], [x0, y1]] };
    let bordered = insets.iter().any(|n| *n > 0);
    let measured = if bordered { rebate_base(&small, &classes, insets, stock, base) } else { None };
    let mask = measured.is_some();
    let base = measured.unwrap_or(base);
    Some(Detection { frame, base, bordered, mask })
}

/// Clear film of the ring around the picture: not holder, not lamp, not the picture itself.
fn rebate_base(img: &Rgb32f, classes: &Classes, insets: [usize; 4], stock: FilmStock, guess: Base) -> Option<Base> {
    let [l, t, r, b] = insets;
    let (w, h) = (img.width, img.height);
    let x1 = w.saturating_sub(r).max(l);
    let y1 = h.saturating_sub(b).max(t);
    let mut px = Vec::new();
    for y in 0..h {
        for x in 0..w {
            let i = y.saturating_mul(w).saturating_add(x);
            if x >= l && x < x1 && y >= t && y < y1 {
                continue;
            }
            let Some(p) = img.data.get(i) else { continue };
            // Holder and clipped lamp only. `classes.lamp` is vs the first-pass histogram, which
            // can be the picture; that would throw the rebate away and leave Convert with no mask.
            if classes.holder.get(i).copied().unwrap_or(false) || guess.is_clip(*p) {
                continue;
            }
            px.push(*p);
        }
    }
    if px.is_empty() {
        return None;
    }
    base::from_rebate(&px, stock, guess)
}

/// The base of `img` without looking for the frame.
pub fn base_of(img: &Rgb32f) -> Option<Base> {
    base::estimate(&preview(img).data)
}

struct Classes {
    lamp: Vec<bool>,
    holder: Vec<bool>,
    blank: Vec<bool>,
    near: Vec<bool>,
}

impl Classes {
    fn new(img: &Rgb32f, base: &Base) -> Self {
        let ys: Vec<f64> = img.data.iter().map(|p| y2020(p.map(f64::from))).collect();
        let by = base.luminance();
        let floor = percentile(&mut ys.clone(), 0.005).unwrap_or(0.0);
        // As dark as the darkest part of the scan, and far denser than the base.
        let holder = (4.0 * floor).max(by * 0.04).min(by * 0.1);
        let within = |p: &[f32; 3], d: f32| (0..3).all(|c| super::kernel::density(p[c], base.rgb[c] as f32).abs() < d);
        Self {
            lamp: img.data.iter().map(|p| base.is_lamp(*p)).collect(),
            holder: ys.iter().map(|y| *y <= holder).collect(),
            blank: img.data.iter().map(|p| within(p, 0.05)).collect(),
            near: img.data.iter().map(|p| within(p, 0.15)).collect(),
        }
    }
}

/// Per line, from the outer edge: holder, lamp, blank and near-blank fractions, and the spread
/// of log luminance (median absolute deviation).
#[derive(Clone, Default)]
struct Lines {
    holder: Vec<f64>,
    lamp: Vec<f64>,
    blank: Vec<f64>,
    near: Vec<f64>,
    spread: Vec<f64>,
}

impl Lines {
    /// Lines across `span` (columns for rows, rows for columns), reading its middle 70 %.
    fn of(img: &Rgb32f, classes: &Classes, rows: bool, span: (usize, usize)) -> Self {
        let (w, h) = (img.width, img.height);
        let (count, across) = if rows { (h, w) } else { (w, h) };
        let (lo, hi) = (span.0.min(across), span.1.min(across));
        let len = hi.saturating_sub(lo);
        let from = lo + len * 15 / 100;
        let to = (lo + len * 85 / 100).max(from + 1).min(across);
        let mut out = Lines::default();
        for line in 0..count {
            let idx: Vec<usize> = (from..to).map(|k| if rows { line * w + k } else { k * w + line }).collect();
            let frac = |m: &[bool]| idx.iter().filter(|i| m.get(**i).copied().unwrap_or(false)).count() as f64 / idx.len().max(1) as f64;
            out.holder.push(frac(&classes.holder));
            out.lamp.push(frac(&classes.lamp));
            out.blank.push(frac(&classes.blank));
            out.near.push(frac(&classes.near));
            let mut logs: Vec<f64> = idx.iter().filter_map(|i| img.data.get(*i)).map(|p| y2020(p.map(f64::from)).max(1e-5).ln()).collect();
            let med = percentile(&mut logs, 0.5).unwrap_or(0.0);
            let mut dev: Vec<f64> = logs.iter().map(|l| (l - med).abs()).collect();
            out.spread.push(percentile(&mut dev, 0.5).unwrap_or(0.0));
        }
        out
    }

    fn reversed(&self) -> Self {
        let rev = |v: &[f64]| v.iter().rev().copied().collect();
        Lines { holder: rev(&self.holder), lamp: rev(&self.lamp), blank: rev(&self.blank), near: rev(&self.near), spread: rev(&self.spread) }
    }
}

/// Lines outside the picture, counted from the outer edge.
fn walk(l: &Lines, negative: bool) -> usize {
    let n = l.holder.len();
    let limit = (n as f64 * REACH) as usize;
    let far = ((n as f64 * 0.12) as usize).max(2);
    let at = |v: &[f64], i: usize| v.get(i).copied().unwrap_or(0.0);
    let lit = |i: usize| at(&l.lamp, i) >= 0.2;

    // Lamp light: an open gate or sprocket holes. A single lit line needs a lit neighbour, so
    // one bright speck is not a border.
    let mut edge = 0;
    for i in 0..limit {
        if i - edge > far {
            break;
        }
        let neighbour = (i + 1 < limit && lit(i + 1)) || (i > 0 && lit(i - 1));
        if at(&l.lamp, i) >= 0.5 || (lit(i) && neighbour) {
            edge = i + 1;
        }
    }

    // The holder. Up to two blurred lines between holder lines still count as holder, and so does
    // the out-of-focus fringe at the very edge, where a slightly turned holder edge crosses lines.
    let fringe_lines = ((n as f64 * 0.03) as usize).max(2);
    let mut touched = (0..edge).any(|k| at(&l.blank, k) >= 0.3);
    let outside = |i: usize, touched: bool| {
        let fringe = !touched && i < fringe_lines && (at(&l.holder, i) >= 0.1 || at(&l.spread, i) >= 0.25);
        at(&l.holder, i) + at(&l.lamp, i) >= 0.85 || fringe
    };
    let mut i = edge;
    while i < limit {
        let here = outside(i, touched) && !(touched && at(&l.holder, i) > at(&l.lamp, i));
        let ahead = !touched && (i + 1..(i + 3).min(limit)).any(|k| outside(k, touched));
        if !(here || ahead) {
            break;
        }
        touched |= at(&l.blank, i) >= 0.3;
        i += 1;
    }
    // The holder's own blurred edge, on its inner side: at most two lines, so a textured picture
    // right against the holder loses no more than that.
    if i > edge {
        let end = (i + 2).min(limit);
        while i < end && !touched && (at(&l.holder, i) >= 0.1 || at(&l.spread, i) >= 0.1) {
            i += 1;
        }
    }
    edge = edge.max(i);
    if !negative {
        return edge;
    }

    // The rebate. At the bare scan edge only truly blank film counts: a flat black shirt is
    // base-like too. Once holder or sprockets have been crossed, edge printing and the uneven
    // flare near the holes are allowed. It may start a few blurred lines in: whatever lies
    // outside the rebate is outside the picture.
    let (film, blank, flat) = if edge > 0 { (&l.near, 0.8, 0.1) } else { (&l.blank, 0.9, 0.03) };
    let rebate = |k: usize| at(film, k) + at(&l.lamp, k) >= blank && at(&l.spread, k) < flat;
    let start = (edge..(edge + fringe_lines).min(limit)).find(|k| rebate(*k)).unwrap_or(edge);
    let mut j = start;
    while j < limit && rebate(j) {
        j += 1;
    }
    // Clear film stops at the frame's edge in one step; deep shadows that look like it fade in.
    let clear = |k: usize| at(&l.blank, k) + at(&l.lamp, k);
    let step = |k: usize| clear(k - 1) - clear(k).min(clear(k + 1));
    let end = (start + 1..=j).max_by(|a, b| step(*a).total_cmp(&step(*b)));
    if let Some(k) = end.filter(|k| k - edge <= far && step(*k) >= 0.25) {
        edge = k;
    }

    // The gap to a neighbouring frame on a strip.
    let gap = |k: usize| at(&l.near, k) >= 0.8 && at(&l.spread, k) < 0.05;
    if let Some(k) = (edge + 2..limit).find(|k| gap(*k)) {
        let mut g = k;
        while g < limit && gap(g) {
            g += 1;
        }
        let narrow = g - k <= ((n as f64 * 0.08) as usize).max(2);
        if narrow && at(&l.near, (g + 1).min(n.saturating_sub(1))) < 0.5 {
            edge = g;
        }
    }
    edge
}

/// Area average down to [`PREVIEW`] on the long edge. Grain on a rebate averages to the film's
/// colour, and a single dusty pixel is not a border.
pub(crate) fn preview(img: &Rgb32f) -> Rgb32f {
    let long = img.width.max(img.height);
    if long <= PREVIEW || img.width == 0 || img.height == 0 {
        return img.clone();
    }
    let k = PREVIEW as f64 / long as f64;
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
            for (acc, c) in s.iter_mut().zip(p) {
                *acc += f64::from(*c);
            }
            n = n.saturating_add(1);
        }
    }
    if n == 0 {
        return [0.0; 3];
    }
    s.map(|v| (v / f64::from(n)) as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 60 lines from the outer edge: `outer` lines of holder fringe, then `clear` and `near`
    /// fractions per line, with flat texture.
    fn lines(outer: usize, clear: impl Fn(usize) -> f64, near: impl Fn(usize) -> f64) -> Lines {
        let n = 60;
        let fringe = |i: usize| i < outer;
        Lines {
            holder: (0..n).map(|i| if fringe(i) { 0.2 } else { 0.0 }).collect(),
            lamp: vec![0.0; n],
            blank: (0..n).map(|i| if fringe(i) { 0.0 } else { clear(i) }).collect(),
            near: (0..n).map(|i| if fringe(i) { 0.0 } else { near(i) }).collect(),
            spread: (0..n).map(|i| if fringe(i) { 0.3 } else { 0.02 }).collect(),
        }
    }

    #[test]
    fn the_rebate_ends_where_clear_film_stops() {
        // Clear film to line 8, then a pale picture that is still mostly near the base.
        let l = lines(2, |i| if i < 8 { 1.0 } else { 0.5 }, |i| if i < 8 { 1.0 } else { 0.85 });
        assert_eq!(walk(&l, true), 8);
    }

    #[test]
    fn shadows_fading_in_at_the_edge_are_not_a_rebate() {
        // A black shirt off the bottom of the frame: nearly clear film, getting denser line by line.
        let l = lines(2, |i| (0.96 - 0.02 * i as f64).max(0.0), |_| 0.85);
        assert_eq!(walk(&l, true), 2);
    }
}
