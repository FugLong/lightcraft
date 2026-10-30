//! Negative Craft tests, on synthetic negatives of known scenes.

use lightcraft_develop::{Film, FilmScan, FilmStock};
use lightcraft_raster::Rgb32f;

use super::kernel::{FILM_GAMMA, GREY};
use super::solve::{Axis, scene, solve};
use super::*;

const BASE: [f64; 3] = [0.85, 0.42, 0.16];
/// Density of a middle gray above the base.
const MID: f64 = 0.8;

/// Transmission of a negative that saw scene light `s`, with a cast `D_c = fog_c + axis_c · D`.
fn negative(s: [f64; 3], fog: [f64; 3], axis: [f64; 3]) -> [f32; 3] {
    [0, 1, 2].map(|c| {
        let d = (FILM_GAMMA * (s[c].max(1e-4) / GREY).log10() + MID).max(0.02);
        (BASE[c] * 10f64.powf(-(fog[c] + axis[c] * d))) as f32
    })
}

/// A scene: a neutral ramp from deep shadow to near white over the top, colour patches below,
/// each shaded from dark to light like a lit object.
fn scene_light(u: f64, v: f64) -> [f64; 3] {
    if v < 0.6 {
        return [0.02 * (0.9f64 / 0.02).powf(u); 3];
    }
    let colour = match (u * 4.0) as usize {
        0 => [0.40, 0.10, 0.05],
        1 => [0.08, 0.30, 0.10],
        2 => [0.05, 0.10, 0.35],
        _ => [0.45, 0.30, 0.22],
    };
    let shade = 0.3 + 1.2 * (u * 4.0).fract();
    colour.map(|c| c * shade)
}

fn picture(w: usize, h: usize, fog: [f64; 3], axis: [f64; 3]) -> Rgb32f {
    Rgb32f::from_fn(w, h, |x, y| negative(scene_light(x as f64 / w as f64, y as f64 / h as f64), fog, axis))
}

/// A scan: dark holder, then a band of blank film, then the picture.
fn scan(fog: [f64; 3]) -> (Rgb32f, [f64; 4]) {
    scan_of(fog, scene_light)
}

fn scan_of(fog: [f64; 3], scene: impl Fn(f64, f64) -> [f64; 3]) -> (Rgb32f, [f64; 4]) {
    let (w, h, holder, rebate) = (300usize, 200usize, 12usize, 10usize);
    let edge = holder + rebate;
    let img = Rgb32f::from_fn(w, h, |x, y| {
        let out = |p: usize, n: usize| p.min(n - 1 - p);
        let (dx, dy) = (out(x, w), out(y, h));
        if dx < holder || dy < holder {
            [0.002; 3]
        } else if dx < edge || dy < edge {
            BASE.map(|c| c as f32)
        } else {
            let (u, v) = ((x - edge) as f64 / (w - 2 * edge) as f64, (y - edge) as f64 / (h - 2 * edge) as f64);
            negative(scene(u, v), fog, [1.0; 3])
        }
    });
    let frame = [edge as f64 / w as f64, edge as f64 / h as f64, 1.0 - edge as f64 / w as f64, 1.0 - edge as f64 / h as f64];
    (img, frame)
}

fn color_film() -> Film {
    Film { enabled: true, scan: FilmScan::Flat, ..Film::default() }
}

fn spread(p: [f32; 3]) -> f32 {
    let mean = (p[0] + p[1] + p[2]) / 3.0;
    (p[0].max(p[1]).max(p[2]) - p[0].min(p[1]).min(p[2])) / mean.max(1e-6)
}

#[test]
fn the_middle_density_prints_middle_gray() {
    let mut film = color_film();
    film.set_base(BASE);
    film.pivot = MID;
    film.d_lo = 0.05;
    let k = FilmKernel::from_film(&film);
    let out = k.apply(negative([GREY; 3], [0.0; 3], [1.0; 3]));
    for c in out {
        assert!((c - GREY as f32).abs() < 1e-3, "middle gray printed {out:?}");
    }
    let black = k.apply(BASE.map(|c| c as f32 * 10f32.powf(-0.05)));
    assert!(black.iter().all(|c| *c < 0.003), "the black anchor printed {black:?}");
}

#[test]
fn contrast_turns_about_the_middle() {
    let mut film = color_film();
    film.set_base(BASE);
    film.pivot = MID;
    let bright = negative([0.7; 3], [0.0; 3], [1.0; 3]);
    let dark = negative([0.04; 3], [0.0; 3], [1.0; 3]);
    let middle = negative([GREY; 3], [0.0; 3], [1.0; 3]);
    let at = |grade: f64, t: [f32; 3]| {
        let mut f = film.clone();
        f.grade = grade;
        FilmKernel::from_film(&f).apply(t)[1]
    };
    assert!(at(1.6, bright) > at(1.0, bright) * 1.2, "more contrast brightens the highlights");
    assert!(at(1.6, dark) < at(1.0, dark) * 0.8, "more contrast darkens the shadows");
    assert!((at(1.6, middle) - at(1.0, middle)).abs() < 0.01, "the middle stays put");
    assert!(at(0.5, bright) < at(1.0, bright), "less contrast is flatter");
}

#[test]
fn the_kernel_never_returns_garbage() {
    let mut film = color_film();
    film.set_base(BASE);
    film.pivot = f64::NAN;
    film.grade = f64::INFINITY;
    film.exposure = 1e300;
    for f in [film.clone(), Film { stock: FilmStock::BwNeg, ..film.clone() }, Film { stock: FilmStock::ColorPos, ..film }] {
        let k = FilmKernel::from_film(&f);
        for t in [[f32::NAN; 3], [f32::INFINITY; 3], [-1.0; 3], [0.0; 3], [1e30; 3], [0.3, 0.1, 0.02]] {
            let out = k.apply(t);
            assert!(out.iter().all(|c| c.is_finite() && *c >= 0.0), "{t:?} printed {out:?}");
        }
        assert_eq!(k.words().len(), WORDS);
    }
}

#[test]
fn a_camera_slope_prints_gray_and_keeps_a_sky() {
    let slope = [1.0, 1.35, 1.55];
    let (w, h, holder, rebate) = (300usize, 200usize, 12usize, 10usize);
    let edge = holder + rebate;
    let img = Rgb32f::from_fn(w, h, |x, y| {
        let out = |p: usize, n: usize| p.min(n - 1 - p);
        let (dx, dy) = (out(x, w), out(y, h));
        if dx < holder || dy < holder {
            [0.002; 3]
        } else if dx < edge || dy < edge {
            BASE.map(|c| c as f32)
        } else {
            let u = (x - edge) as f64 / (w - 2 * edge) as f64;
            let v = (y - edge) as f64 / (h - 2 * edge) as f64;
            if u > 0.7 {
                negative([0.12, 0.22, 0.70].map(|c| c * (0.4 + v)), [0.0; 3], slope)
            } else {
                negative([0.03 * (0.85f64 / 0.03).powf(v); 3], [0.0; 3], slope)
            }
        }
    });
    let a = analyze_image(&img, &color_film());
    assert!(a.frame_error.is_none(), "{:?}", a.frame_error);
    let axis = a.film.axis();
    for c in 0..3 {
        assert!((axis[c] - slope[c] / (slope.iter().sum::<f64>() / 3.0)).abs() < 0.08, "axis {axis:?}");
    }
    let k = FilmKernel::from_film(&a.film);
    let gray = k.apply(negative([0.18; 3], [0.0; 3], slope));
    assert!(spread(gray) < 0.08, "sloped gray printed {gray:?}");
    let sky = k.apply(negative([0.12, 0.22, 0.70], [0.0; 3], slope));
    assert!(sky[2] > sky[0] * 1.4, "sky was pulled neutral {sky:?}");
}

#[test]
fn the_solve_prints_the_bright_end_at_the_top_of_the_paper() {
    let mut film = color_film();
    film.set_base(BASE);
    let img = picture(64, 40, [0.0; 3], [1.0; 3]);
    let p = solve(&solve::densities(&img.data, &film), Axis::Lab, false).expect("print");
    let top = scene(p.highlight, p.black, p.mid, p.contrast) * 2f64.powf(p.exposure);
    assert!((top - 1.0).abs() < 0.02, "bright end printed {top} ({p:?})");
    assert!(p.mid > p.black + 0.1 && p.highlight > p.mid, "{p:?}");
    assert!((0.6..1.6).contains(&p.contrast), "a normal negative needs about normal contrast: {}", p.contrast);
}

#[test]
fn convert_finds_the_picture_past_the_holder_and_the_rebate() {
    let (img, want) = scan([0.1, 0.0, -0.07]);
    let a = analyze_image(&img, &color_film());
    assert!(a.frame_error.is_none(), "{:?}", a.frame_error);
    let c = a.film.frame.corners;
    let got = [c[0][0], c[0][1], c[2][0], c[2][1]];
    for (g, w) in got.iter().zip(want) {
        assert!((g - w).abs() < 0.02, "frame {got:?}, want {want:?}");
    }
    let base = a.film.base();
    for c in 0..3 {
        assert!((base[c] / BASE[c] - 1.0).abs() < 0.02, "base {base:?}");
    }
}

/// Integral densities a camera records for equal analytical dyes `d` (a gray subject).
fn camera_integral(d: f64) -> [f64; 3] {
    let [cg, mb, cb] = FilmScan::Camera.dye();
    [d, d * (1.0 + cg), d * (1.0 + mb + cb)]
}

fn camera_negative(s: [f64; 3]) -> [f32; 3] {
    let a = [0, 1, 2].map(|k| (FILM_GAMMA * (s[k].max(1e-4) / GREY).log10() + MID).max(0.02));
    let [cg, mb, cb] = FilmScan::Camera.dye();
    let d = [a[0], a[1] + cg * a[0], a[2] + mb * a[1] + cb * a[0]];
    [0, 1, 2].map(|c| (BASE[c] * 10f64.powf(-d[c])) as f32)
}

fn lab_ab(rgb: [f32; 3]) -> (f64, f64) {
    let xyz = lightcraft_color::REC2020.to_xyz().apply(rgb.map(f64::from));
    let f = |t: f64| if t > 0.008856 { t.cbrt() } else { 7.787037 * t + 16.0 / 116.0 };
    let (fx, fy, fz) = (f(xyz[0] / 0.95047), f(xyz[1]), f(xyz[2] / 1.08883));
    (500.0 * (fx - fy), 200.0 * (fy - fz))
}

#[test]
fn hanson_recovers_equal_dye_amounts() {
    let m = dye_matrix(FilmScan::Camera);
    let d = 0.8f32;
    let integral = camera_integral(d as f64).map(|v| v as f32);
    let anal = super::kernel::analytical(&m, integral);
    for c in anal {
        assert!((c - d).abs() < 1e-5, "analytical {anal:?} from integral {integral:?}");
    }
}

#[test]
fn camera_dye_prints_a_gray_ramp_lab_neutral() {
    let (w, h, holder, rebate) = (300usize, 200usize, 12usize, 10usize);
    let edge = holder + rebate;
    let img = Rgb32f::from_fn(w, h, |x, y| {
        let out = |p: usize, n: usize| p.min(n - 1 - p);
        let (dx, dy) = (out(x, w), out(y, h));
        if dx < holder || dy < holder {
            [0.002; 3]
        } else if dx < edge || dy < edge {
            BASE.map(|c| c as f32)
        } else {
            let u = (x - edge) as f64 / (w - 2 * edge) as f64;
            camera_negative([0.02 * (0.9f64 / 0.02).powf(u); 3])
        }
    });
    let a = analyze_image(&img, &Film { enabled: true, scan: FilmScan::Camera, ..Film::default() });
    assert!(a.frame_error.is_none(), "{:?}", a.frame_error);
    let k = FilmKernel::from_film(&a.film);
    for s in [0.05, 0.18, 0.5] {
        let out = k.apply(camera_negative([s; 3]));
        assert!(spread(out) < 0.04, "camera dyes printed gray {s} as {out:?}");
        let (aa, bb) = lab_ab(out);
        assert!(aa.abs() < 3.0 && bb.abs() < 3.0, "Lab cast on gray {s}: a* {aa:.1} b* {bb:.1} {out:?}");
    }
}

#[test]
fn a_gray_card_beside_blue_sky_stays_gray() {
    let sky = |u: f64, v: f64| {
        if u < 0.33 {
            [0.04 * (0.8f64 / 0.04).powf(v); 3]
        } else {
            let s = 0.15 + 0.7 * v;
            [0.12 * s, 0.22 * s, 0.70 * s]
        }
    };
    let (img, _) = scan_of([0.0; 3], sky);
    let a = analyze_image(&img, &color_film());
    assert!(a.frame_error.is_none(), "{:?}", a.frame_error);
    assert_lab_pack(&a.film);
    let k = FilmKernel::from_film(&a.film);
    for s in [0.08, 0.18, 0.45] {
        let out = k.apply(negative([s; 3], [0.0; 3], [1.0; 3]));
        assert!(spread(out) < 0.05, "sky pulled gray {s} to {out:?}");
        let (aa, bb) = lab_ab(out);
        assert!(aa.abs() < 4.0 && bb.abs() < 4.0, "Lab cast on gray {s}: a* {aa:.1} b* {bb:.1} {out:?}");
    }
    let printed_sky = k.apply(negative([0.12, 0.22, 0.70], [0.0; 3], [1.0; 3]));
    assert!(printed_sky[2] > printed_sky[0] * 1.5, "sky was gray-worlded to {printed_sky:?}");
}

#[test]
fn a_bright_sky_is_not_the_mask() {
    let (w, h, holder, rebate) = (300usize, 200usize, 12usize, 10usize);
    let edge = holder + rebate;
    let img = Rgb32f::from_fn(w, h, |x, y| {
        let out = |p: usize, n: usize| p.min(n - 1 - p);
        let (dx, dy) = (out(x, w), out(y, h));
        if x < 8 && y < 8 {
            [1.0; 3]
        } else if dx < holder || dy < holder {
            [0.002; 3]
        } else if dx < edge || dy < edge {
            BASE.map(|c| c as f32)
        } else {
            let u = (x - edge) as f64 / (w - 2 * edge) as f64;
            let v = (y - edge) as f64 / (h - 2 * edge) as f64;
            if u < 0.25 {
                negative([0.03 * (0.85f64 / 0.03).powf(v); 3], [0.0; 3], [1.0; 3])
            } else {
                let sky = 0.2 + 0.6 * v;
                negative([0.10 * sky, 0.20 * sky, 0.65 * sky], [0.0; 3], [1.0; 3])
            }
        }
    });
    let a = analyze_image(&img, &color_film());
    assert!(a.frame_error.is_none(), "{:?}", a.frame_error);
    let base = a.film.base();
    for c in 0..3 {
        assert!((base[c] / BASE[c] - 1.0).abs() < 0.04, "base {base:?}");
    }
    assert_lab_pack(&a.film);
    let k = FilmKernel::from_film(&a.film);
    for s in [0.08, 0.18, 0.45] {
        let out = k.apply(negative([s; 3], [0.0; 3], [1.0; 3]));
        let (aa, bb) = lab_ab(out);
        assert!(aa.abs() < 4.0 && bb.abs() < 4.0, "cast on gray {s}: a* {aa:.1} b* {bb:.1} {out:?}");
        assert!(spread(out) < 0.05, "gray {s} printed {out:?}");
    }
}

#[test]
fn two_scenes_on_the_same_rebate_share_the_mask() {
    let sky = |u: f64, v: f64| {
        let s = 0.08 * (0.9f64 / 0.08).powf(u);
        if v < 0.45 { [0.15 * s, 0.25 * s, 0.75 * s] } else { [s; 3] }
    };
    let trees = |u: f64, v: f64| {
        let s = 0.08 * (0.9f64 / 0.08).powf(u);
        if v < 0.45 { [0.10 * s, 0.45 * s, 0.12 * s] } else { [s; 3] }
    };
    let (sky_img, _) = scan_of([0.0; 3], sky);
    let (tree_img, _) = scan_of([0.0; 3], trees);
    let a = analyze_image(&sky_img, &color_film());
    let b = analyze_image(&tree_img, &color_film());
    assert!(a.frame_error.is_none() && b.frame_error.is_none(), "{:?} {:?}", a.frame_error, b.frame_error);
    for c in 0..3 {
        assert!((a.film.base()[c] - b.film.base()[c]).abs() < 0.02, "bases {:?} {:?}", a.film.base(), b.film.base());
    }
    assert_lab_pack(&a.film);
    assert_lab_pack(&b.film);
    let ka = FilmKernel::from_film(&a.film);
    let kb = FilmKernel::from_film(&b.film);
    let ga = ka.apply(negative([0.18; 3], [0.0; 3], [1.0; 3]));
    let gb = kb.apply(negative([0.18; 3], [0.0; 3], [1.0; 3]));
    for c in 0..3 {
        assert!((ga[c] - gb[c]).abs() < 0.04, "gray printed {ga:?} vs {gb:?}");
    }
}

#[test]
fn convert_prints_neutrals_neutral_and_keeps_colour() {
    let (img, _) = scan([0.0; 3]);
    let a = analyze_image(&img, &color_film());
    assert_lab_pack(&a.film);
    let k = FilmKernel::from_film(&a.film);
    let mut last = 0.0;
    for s in [0.05, 0.18, 0.5] {
        let out = k.apply(negative([s; 3], [0.0; 3], [1.0; 3]));
        assert!(spread(out) < 0.03, "gray {s} printed {out:?}");
        assert!(out[1] > last, "the ramp is not monotone at {s}");
        last = out[1];
    }
    // A saturated red keeps its saturation: red over green is near the scene's 4.
    let red = k.apply(negative([0.40, 0.10, 0.05], [0.0; 3], [1.0; 3]));
    let ratio = red[0] / red[1].max(1e-6);
    assert!((2.8..6.0).contains(&ratio), "red faded to {red:?} (r/g {ratio})");
}

#[test]
fn a_washed_rebate_edge_is_not_dmin() {
    let pale = [0.96f32, 0.88, 0.78];
    let (w, h, holder, wash, rebate) = (300usize, 200usize, 12usize, 4usize, 12usize);
    let inner = holder + wash + rebate;
    let img = Rgb32f::from_fn(w, h, |x, y| {
        let out = |p: usize, n: usize| p.min(n - 1 - p);
        let (dx, dy) = (out(x, w), out(y, h));
        if dx < holder || dy < holder {
            [0.002; 3]
        } else if dx < holder + wash || dy < holder + wash {
            pale
        } else if dx < inner || dy < inner {
            BASE.map(|c| c as f32)
        } else {
            let (u, v) = ((x - inner) as f64 / (w - 2 * inner) as f64, (y - inner) as f64 / (h - 2 * inner) as f64);
            negative(scene_light(u, v), [0.0; 3], [1.0; 3])
        }
    });
    let a = analyze_image(&img, &color_film());
    assert!(a.frame_error.is_none(), "{:?}", a.frame_error);
    let base = a.film.base();
    for c in 0..3 {
        assert!((base[c] / BASE[c] - 1.0).abs() < 0.06, "washed fringe became the mask {base:?}");
    }
}

#[test]
fn a_pale_fringe_that_outnumbers_the_rebate_is_not_dmin() {
    let pale = [0.96f32, 0.88, 0.78];
    let (w, h, holder, wash, rebate) = (300usize, 200usize, 8usize, 28usize, 8usize);
    let inner = holder + wash + rebate;
    let img = Rgb32f::from_fn(w, h, |x, y| {
        let out = |p: usize, n: usize| p.min(n - 1 - p);
        let (dx, dy) = (out(x, w), out(y, h));
        if dx < holder || dy < holder {
            [0.002; 3]
        } else if dx < holder + wash || dy < holder + wash {
            pale
        } else if dx < inner || dy < inner {
            BASE.map(|c| c as f32)
        } else {
            let (u, v) = ((x - inner) as f64 / (w - 2 * inner) as f64, (y - inner) as f64 / (h - 2 * inner) as f64);
            negative(scene_light(u, v), [0.0; 3], [1.0; 3])
        }
    });
    let a = analyze_image(&img, &color_film());
    assert!(a.frame_error.is_none(), "{:?}", a.frame_error);
    let base = a.film.base();
    for c in 0..3 {
        assert!((base[c] / BASE[c] - 1.0).abs() < 0.08, "the pale majority became the mask {base:?}");
    }
}

#[test]
fn a_neutral_border_is_refused() {
    let (w, h, holder, rebate) = (300usize, 200usize, 12usize, 16usize);
    let edge = holder + rebate;
    let img = Rgb32f::from_fn(w, h, |x, y| {
        let out = |p: usize, n: usize| p.min(n - 1 - p);
        let (dx, dy) = (out(x, w), out(y, h));
        if dx < holder || dy < holder {
            [0.002; 3]
        } else if dx < edge || dy < edge {
            [0.82, 0.80, 0.78]
        } else {
            [0.40, 0.42, 0.38]
        }
    });
    let a = analyze_image(&img, &color_film());
    assert_eq!(a.frame_error.as_deref(), Some("No blank film found. Click Border on the orange unexposed film."));
    assert!(!a.film.enabled, "a neutral ring must not print");
}

#[test]
fn an_orange_click_is_kept_when_the_ring_is_not_orange() {
    let (w, h, holder, rebate) = (300usize, 200usize, 12usize, 16usize);
    let edge = holder + rebate;
    let img = Rgb32f::from_fn(w, h, |x, y| {
        let out = |p: usize, n: usize| p.min(n - 1 - p);
        let (dx, dy) = (out(x, w), out(y, h));
        if dx < holder || dy < holder {
            [0.002; 3]
        } else if dx < edge || dy < edge {
            [0.90, 0.86, 0.82]
        } else {
            [0.40, 0.42, 0.38]
        }
    });
    let mut film = color_film();
    film.set_base(BASE);
    let a = analyze_image(&img, &film);
    assert!(a.frame_error.is_none(), "{:?}", a.frame_error);
    assert!(a.film.enabled);
    let base = a.film.base();
    for c in 0..3 {
        assert!((base[c] - BASE[c]).abs() < 1e-6, "the click was replaced by {base:?}");
    }
}

#[test]
fn a_click_on_the_rebate_is_not_blown() {
    let (img, _) = scan([0.0; 3]);
    let capture = color_film().capture;
    let rebate = sample_border(&img, 17.0 / 300.0, 0.5, capture).expect("rebate");
    for c in 0..3 {
        assert!((rebate[c] / BASE[c] - 1.0).abs() < 0.04, "click {rebate:?}");
    }
    let stock = FilmStock::ColorNeg;
    assert_eq!(border_error(rebate, stock), None, "orange film was refused as {rebate:?}");
    let lamp = sample_border(&Rgb32f::filled(32, 32, [1.0; 3]), 0.5, 0.5, capture).expect("lamp");
    assert_eq!(border_error(lamp, stock), Some("that spot is blown out — click the blank film, not the white lamp"));
    let holder = sample_border(&Rgb32f::filled(32, 32, [0.002; 3]), 0.5, 0.5, capture).expect("holder");
    assert_eq!(border_error(holder, stock), Some("that spot is too dark — click the blank film, not the picture"));
    let pale = [0.96, 0.88, 0.78];
    assert_eq!(border_error(pale, stock), Some("that spot is not orange film — click the blank orange border"));
}

#[test]
fn a_bright_lamp_is_not_the_base() {
    let mut px = vec![[1.0f32; 3]; 3000];
    px.extend(std::iter::repeat_n(BASE.map(|c| c as f32), 2000));
    let pic = picture(60, 60, [0.0; 3], [1.0; 3]);
    px.extend(pic.data.iter().copied());
    let b = base::estimate(&px).expect("base");
    for c in 0..3 {
        assert!((b.rgb[c] / BASE[c] - 1.0).abs() < 0.02, "base {:?}", b.rgb);
    }
    assert!(b.is_lamp([1.0; 3]), "the lamp is lamp");
    assert!(!b.is_lamp(BASE.map(|c| c as f32)), "the base is film");
}

#[test]
fn neutral_picks_set_the_axis() {
    let fog = [0.1, 0.0, -0.05];
    let axis = [1.1, 1.0, 0.92];
    let mut film = color_film();
    film.set_base(BASE);
    let t = |s: f64| negative([s; 3], fog, axis).map(f64::from);
    assert_eq!(axis_from_picks(&film), None, "no picks, no axis");
    film.gray_pick = Some(t(GREY));
    let (one_fog, one_axis) = axis_from_picks(&film).expect("one pick");
    assert_eq!(one_axis, [1.0; 3], "one pick keeps the slopes");
    let d = linear_density(t(GREY), &film);
    let s = solve::straighten(d, one_fog, one_axis);
    assert!((s[0] - s[1]).abs() < 1e-9 && (s[2] - s[1]).abs() < 1e-9, "the pick is neutral: {s:?}");
    film.shadow_pick = Some(t(0.03));
    film.highlight_pick = Some(t(0.7));
    let (got_fog, got_axis) = axis_from_picks(&film).expect("three picks");
    for c in [0, 2] {
        assert!((got_axis[c] - axis[c]).abs() < 0.01 && (got_fog[c] - fog[c]).abs() < 0.01, "{got_fog:?} {got_axis:?}");
    }
}

#[test]
fn a_margin_change_keeps_contrast_and_exposure() {
    let (img, _) = scan([0.1, 0.0, -0.07]);
    let mut film = analyze_image(&img, &color_film()).film;
    film.grade = 2.2;
    film.exposure = 0.7;
    film.inset = 20.0;
    let p = place(&img, &film).expect("print");
    p.write_measurement_to(&mut film);
    assert_eq!((film.grade, film.exposure), (2.2, 0.7));
}

#[test]
fn a_failed_measure_does_not_enable_conversion() {
    let nan = Rgb32f::filled(40, 30, [f32::NAN; 3]);
    let a = analyze_image(&nan, &color_film());
    assert!(a.frame_error.is_some(), "expected a refusal, got a print");
    assert!(!a.film.enabled, "a white default mask must not print");
}

#[test]
fn convert_without_a_rebate_stays_off() {
    let img = Rgb32f::filled(80, 60, negative([0.18; 3], [0.0; 3], [1.0; 3]));
    let a = analyze_image(&img, &color_film());
    assert_eq!(a.frame_error.as_deref(), Some("No blank film found. Click Border on the orange unexposed film."));
    assert!(!a.film.enabled, "a histogram of the picture must not print as Dmin");
    assert_eq!(a.film.base(), Film::default().base());
}

#[test]
fn a_border_click_is_enough_without_a_detected_rebate() {
    let img = Rgb32f::filled(80, 60, negative([0.18; 3], [0.0; 3], [1.0; 3]));
    let mut film = color_film();
    film.set_base(BASE);
    let a = analyze_image(&img, &film);
    assert!(a.frame_error.is_none(), "{:?}", a.frame_error);
    assert!(a.film.enabled);
    assert_eq!(a.film.base(), BASE);
    assert_lab_pack(&a.film);
}

#[test]
fn camera_hanson_on_working_space_gray_is_not_neutral() {
    let mut film = color_film();
    film.scan = FilmScan::Camera;
    film.set_base(BASE);
    film.pivot = MID;
    film.d_lo = 0.05;
    let out = FilmKernel::from_film(&film).apply(negative([GREY; 3], [0.0; 3], [1.0; 3]));
    assert!(spread(out) > 0.08, "Camera Hanson on a JPEG-like gray printed {out:?}");
    film.scan = FilmScan::Flat;
    let flat = FilmKernel::from_film(&film).apply(negative([GREY; 3], [0.0; 3], [1.0; 3]));
    assert!(spread(flat) < 0.03, "Flat must print that gray {flat:?}");
}

#[test]
fn flat_integral_density_keeps_the_camera_dye_blue() {
    let mut film = color_film();
    film.set_base(BASE);
    film.pivot = MID;
    film.d_lo = 0.05;
    let d = FilmKernel::from_film(&film).analytical_density(camera_negative([0.18; 3]));
    assert!(d[2] > d[0] + 0.15, "Flat removed the extra blue density {d:?}");
}

#[test]
fn the_film_inverse_keeps_a_negative_working_space_lobe() {
    let mut img = Rgb32f::filled(2, 2, [0.4, 0.2, -0.15]);
    let id = [[1.0f32, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    apply_matrix(&mut img, &id);
    assert!(img.data[0][2] < 0.0, "the inverse clamped the orange mask's blue: {:?}", img.data[0]);
    apply_matrix_clamped(&mut img, &id);
    assert!(img.data[0][2] >= 0.0, "the positive was not clamped");
}

#[test]
fn an_unclamped_matrix_round_trip_keeps_equal_density() {
    let m = lightcraft_color::Mat3([[1.45, -0.30, -0.15], [-0.25, 1.40, -0.15], [-0.55, -0.25, 1.80]]);
    let inv = m.inverse().expect("invertible");
    let base = BASE.map(|c| c as f32);
    let gray = camera_negative([0.18; 3]);
    let forward = |p: [f32; 3]| m.apply_f32(p);
    let rec = forward(base);
    assert!(rec[2] < 0.0, "orange film should drive working-space blue negative, got {rec:?}");
    let back = |p: [f32; 3]| inv.apply_f32(p);
    let (base_back, gray_back) = (back(forward(base)), back(forward(gray)));
    for c in 0..3 {
        assert!((base_back[c] - base[c]).abs() < 1e-3, "base {base_back:?}");
        assert!((gray_back[c] - gray[c]).abs() < 1e-3, "gray {gray_back:?}");
    }
    let density = |t: [f32; 3], b: [f32; 3]| [0, 1, 2].map(|i| (b[i].max(1e-6) / t[i].max(1e-6)).log10());
    let orig = density(gray, base);
    let ok = density(gray_back, base_back);
    for c in 0..3 {
        assert!((ok[c] - orig[c]).abs() < 0.02, "unclamped densities {ok:?} left the dye {orig:?}");
    }
    let clamped = |p: [f32; 3]| forward(p).map(|v| v.max(0.0));
    let damaged = back(clamped(base));
    assert!((damaged[2] - base[2]).abs() > 0.02, "clamping did not move the recovered blue {damaged:?}");
    let bad = density(back(clamped(gray)), damaged);
    assert!((bad[2] - orig[2]).abs() > 0.05, "clamping left the blue density {bad:?} (was {orig:?})");
}

#[test]
fn camera_rgb_convert_selects_camera_hanson_unless_the_scan_was_picked() {
    let info = crate::SourceInfo {
        raw: true,
        transmission: true,
        to_working: Some([[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]),
        ..crate::SourceInfo::default()
    };
    let mut film = Film { enabled: true, ..Film::default() };
    assert_eq!(film.scan, FilmScan::Flat);
    prepare_scan(&info, &mut film);
    assert_eq!(film.scan, FilmScan::Camera);
    film.scan = FilmScan::Flat;
    film.scan_picked = true;
    prepare_scan(&info, &mut film);
    assert_eq!(film.scan, FilmScan::Flat, "an explicit Flat scan stays");
    film.scan = FilmScan::Minilab;
    prepare_scan(&info, &mut film);
    assert_eq!(film.scan, FilmScan::Minilab);
    let jpeg = crate::SourceInfo::default();
    film.scan = FilmScan::Camera;
    film.scan_picked = true;
    prepare_scan(&jpeg, &mut film);
    assert_eq!(film.scan, FilmScan::Flat, "a rendered file cannot take Camera Hanson");
}

#[test]
fn hostile_scans_do_not_panic() {
    let film = color_film();
    let nan = Rgb32f::filled(40, 30, [f32::NAN; 3]);
    let images = [
        Rgb32f::filled(0, 0, [0.5; 3]),
        Rgb32f::filled(1, 1, [0.5; 3]),
        Rgb32f::filled(3, 200, [0.5; 3]),
        Rgb32f::filled(64, 64, [0.0; 3]),
        Rgb32f::filled(64, 64, [1.0; 3]),
        nan,
        Rgb32f::from_fn(50, 50, |x, y| if (x + y) % 2 == 0 { [f32::INFINITY, -1.0, 0.3] } else { [0.4, f32::NAN, 0.1] }),
    ];
    for img in &images {
        for stock in [FilmStock::ColorNeg, FilmStock::BwNeg, FilmStock::ColorPos] {
            let f = Film { stock, ..film.clone() };
            let a = analyze_image(img, &f);
            let _ = analyze_picture(img, &f);
            let _ = place(img, &a.film);
            let _ = inspect(img, &a.film);
            let _ = detect_frame(img, &f);
            let _ = sample_border(img, 0.5, 0.5, f.capture);
            let k = FilmKernel::from_film(&a.film);
            for p in img.data.iter().take(64) {
                assert!(k.apply(*p).iter().all(|c| c.is_finite()));
            }
        }
    }
}

fn assert_lab_pack(film: &Film) {
    let fog = film.fog();
    let axis = film.axis();
    for c in 0..3 {
        assert!(fog[c].abs() < 0.02, "fog {fog:?}");
        assert!((axis[c] - 1.0).abs() < 0.02, "axis {axis:?}");
    }
}

fn measured() -> (Rgb32f, Film) {
    let (img, _) = scan([0.0; 3]);
    let film = analyze_image(&img, &color_film()).film;
    (img, film)
}

fn print_of(film: &Film, s: [f64; 3]) -> [f32; 3] {
    FilmKernel::from_film(film).apply(negative(s, [0.0; 3], [1.0; 3]))
}

#[test]
fn channel_contrast_keeps_the_middle_gray() {
    let (_, mut film) = measured();
    let middle = solve::scene(film.pivot, film.d_lo, film.pivot, film.grade);
    let mid_s = GREY * 10f64.powf((film.pivot - MID) / FILM_GAMMA);
    let before = [print_of(&film, [0.02; 3]), print_of(&film, [mid_s; 3]), print_of(&film, [0.7; 3])];
    film.gamma_b = 1.3;
    let dark = print_of(&film, [0.02; 3]);
    let gray = print_of(&film, [mid_s; 3]);
    let light = print_of(&film, [0.7; 3]);
    assert!(spread(gray) < 0.02, "the middle took a cast: {gray:?} (middle {middle})");
    assert!((gray[1] - before[1][1]).abs() < 1e-4, "green moved");
    assert!(light[2] > before[2][2] * 1.05 && dark[2] < before[0][2] * 0.95, "blue contrast: {dark:?} {light:?}");
}

#[test]
fn a_remeasure_does_not_undo_the_user_s_trims() {
    let (img, film) = measured();
    let plain = place(&img, &film).expect("print");
    let mut trimmed = film.clone();
    trimmed.gamma_r = 0.8;
    trimmed.gamma_b = 1.25;
    trimmed.presat = 40.0;
    trimmed.temp = 30.0;
    assert_eq!(place(&img, &trimmed), Some(plain), "the measurement saw the trims");
}

#[test]
fn saturation_keeps_neutrals_and_brightness() {
    let (_, film) = measured();
    for presat in [-60.0, 50.0] {
        let f = Film { presat, ..film.clone() };
        for s in [0.05, 0.18, 0.6] {
            let gray = print_of(&f, [s; 3]);
            assert!(spread(gray) < 0.02, "saturation {presat} tinted gray {s}: {gray:?}");
        }
        let red = [0.40, 0.10, 0.05];
        let (a, b) = (print_of(&film, red), print_of(&f, red));
        let y = |c: [f32; 3]| lightcraft_color::luminance_2020(c);
        assert!((y(a) - y(b)).abs() < 0.01 * y(a).max(1e-3) + 1e-4, "saturation changed brightness: {a:?} -> {b:?}");
        let chroma = |c: [f32; 3]| c[0] / c[1].max(1e-6);
        assert_eq!(chroma(b) > chroma(a), presat > 0.0, "{a:?} -> {b:?}");
    }
}

#[test]
fn color_balance_moves_hue_not_brightness() {
    let (_, film) = measured();
    let y = |c: [f32; 3]| lightcraft_color::luminance_2020(c);
    for (temp, tint) in [(60.0, 0.0), (-60.0, 0.0), (0.0, 50.0), (0.0, -50.0)] {
        let f = Film { temp, tint, ..film.clone() };
        let (a, b) = (print_of(&film, [0.18; 3]), print_of(&f, [0.18; 3]));
        assert!((y(a) - y(b)).abs() < 1e-3 * y(a).max(1e-3), "temp {temp} tint {tint}: {a:?} -> {b:?}");
        assert!(spread(b) > 0.05, "no colour moved");
    }
}

#[test]
fn tonal_balances_touch_their_own_tones() {
    let (_, film) = measured();
    let f = Film { shadow_temp: 80.0, ..film.clone() };
    let (mid_a, mid_b) = (print_of(&film, [0.18; 3]), print_of(&f, [0.18; 3]));
    let (dark_a, dark_b) = (print_of(&film, [0.01; 3]), print_of(&f, [0.01; 3]));
    let warmth = |c: [f32; 3]| c[0] / c[2].max(1e-6);
    assert!(warmth(dark_b) > warmth(dark_a) * 1.5, "shadow temp missed the shadows: {dark_a:?} -> {dark_b:?}");
    assert!((warmth(mid_b) / warmth(mid_a) - 1.0).abs() < 0.1, "shadow temp moved middle gray: {mid_a:?} -> {mid_b:?}");
}
