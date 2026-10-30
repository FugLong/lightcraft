//! A roll printed as one: the frames share the mask colour and the gray axis, as a lab prints a
//! roll with one filter pack. Exposure and contrast are solved again per frame.

use super::{FrameStats, percentile};

/// Stops of rebate drift past which a roll still converts, and reports the frame.
pub const DRIFT_WARN: f64 = 0.5;

/// One frame after a roll has shared its colour.
#[derive(Clone, Debug, PartialEq)]
pub struct SharedFrame {
    /// The roll's mask colour at this frame's own rebate level, so a different shutter speed
    /// does not repaint the roll.
    pub base: [f64; 3],
    pub fog: [f64; 3],
    pub axis: [f64; 3],
    /// Stops this frame's rebate sits from the roll's median.
    pub drift: f64,
}

/// Share mask colour and the gray axis across a roll.
pub fn share_roll(frames: &[FrameStats]) -> Vec<SharedFrame> {
    let measured: Vec<&FrameStats> = frames.iter().filter(|f| f.mask_measured).collect();
    let median = |v: &mut Vec<f64>, d: f64| percentile(v, 0.5).unwrap_or(d);
    let level = median(&mut measured.iter().map(|f| f.base[1].max(1e-6)).collect(), 1.0).max(1e-6);
    let ratio = |c: usize| median(&mut measured.iter().map(|f| f.base[c] / f.base[1].max(1e-6)).collect(), 1.0);
    let ratio = [ratio(0), 1.0, ratio(2)];
    let prints: Vec<_> = frames.iter().filter_map(|f| f.print).collect();
    let channel = |get: fn(&super::Print) -> [f64; 3], c: usize, d: f64| median(&mut prints.iter().map(|p| get(p)[c]).collect(), d);
    let fog = [channel(|p| p.fog, 0, 0.0), 0.0, channel(|p| p.fog, 2, 0.0)];
    let axis = [channel(|p| p.axis, 0, 1.0), 1.0, channel(|p| p.axis, 2, 1.0)];
    frames
        .iter()
        .map(|f| {
            let g = f.base[1].max(1e-6);
            SharedFrame {
                base: if f.mask_measured { ratio.map(|r| r * g) } else { f.base },
                fog,
                axis,
                drift: if f.mask_measured { (g / level).log2() } else { 0.0 },
            }
        })
        .collect()
}
