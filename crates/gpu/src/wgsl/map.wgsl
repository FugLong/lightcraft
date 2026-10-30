// Element-wise kernels (one thread per pixel, 1-D on a 2-D grid). Bindings: a, b, c (inputs), dst.
// P[0] = pixel count; further parameters per kernel. Each mirrors a CPU expression in
// `lightcraft_pipeline::local` (named in the comment).

fn rgb_a(i: u32) -> vec3<f32> {
    return vec3<f32>(a[3u * i], a[3u * i + 1u], a[3u * i + 2u]);
}

fn put_rgb(i: u32, v: vec3<f32>) {
    dst[3u * i] = v.x;
    dst[3u * i + 1u] = v.y;
    dst[3u * i + 2u] = v.z;
}

// `log_lum` of an RGB image.
@compute @workgroup_size(256)
fn log_lum_k(@builtin(global_invocation_id) g: vec3<u32>, @builtin(num_workgroups) nw: vec3<u32>) {
    let i = lin_index(g, nw);
    if (i >= pu(0u)) {
        return;
    }
    dst[i] = log_lum(rgb_a(i));
}

// `dark_of`: the dark channel (dehaze).
@compute @workgroup_size(256)
fn dark_k(@builtin(global_invocation_id) g: vec3<u32>, @builtin(num_workgroups) nw: vec3<u32>) {
    let i = lin_index(g, nw);
    if (i >= pu(0u)) {
        return;
    }
    let c = rgb_a(i);
    dst[i] = min(min(c.x, c.y), c.z);
}

// Guided filter input pair (p, p²).
@compute @workgroup_size(256)
fn guided_pre(@builtin(global_invocation_id) g: vec3<u32>, @builtin(num_workgroups) nw: vec3<u32>) {
    let i = lin_index(g, nw);
    if (i >= pu(0u)) {
        return;
    }
    let p = a[i];
    dst[2u * i] = p;
    dst[2u * i + 1u] = p * p;
}

// Guided filter coefficients (a, b) from blurred (mean, corr). P[1] = eps.
@compute @workgroup_size(256)
fn guided_ab(@builtin(global_invocation_id) g: vec3<u32>, @builtin(num_workgroups) nw: vec3<u32>) {
    let i = lin_index(g, nw);
    if (i >= pu(0u)) {
        return;
    }
    let m = a[2u * i];
    let c = a[2u * i + 1u];
    let v = max(c - m * m, 0.0);
    let k = v / (v + pf(1u));
    dst[2u * i] = k;
    dst[2u * i + 1u] = m - k * m;
}

// `q = a·p + b` (a: p, b: interleaved blurred coefficients).
@compute @workgroup_size(256)
fn guided_apply(@builtin(global_invocation_id) g: vec3<u32>, @builtin(num_workgroups) nw: vec3<u32>) {
    let i = lin_index(g, nw);
    if (i >= pu(0u)) {
        return;
    }
    dst[i] = b[2u * i] * a[i] + b[2u * i + 1u];
}

// Cross guided filter (`masks::guided_cross`), a: guide, b: input. P[1] = 0: (guide, input);
// 1: (guide·input, guide²).
@compute @workgroup_size(256)
fn xguided_pre(@builtin(global_invocation_id) g: vec3<u32>, @builtin(num_workgroups) nw: vec3<u32>) {
    let i = lin_index(g, nw);
    if (i >= pu(0u)) {
        return;
    }
    let gi = a[i];
    let p = b[i];
    if (pu(1u) == 0u) {
        dst[2u * i] = gi;
        dst[2u * i + 1u] = p;
    } else {
        dst[2u * i] = gi * p;
        dst[2u * i + 1u] = gi * gi;
    }
}

// Coefficients (a, b) from blurred means (a: guide, input) and correlations (b: guide·input,
// guide²). P[1] = eps.
@compute @workgroup_size(256)
fn xguided_ab(@builtin(global_invocation_id) g: vec3<u32>, @builtin(num_workgroups) nw: vec3<u32>) {
    let i = lin_index(g, nw);
    if (i >= pu(0u)) {
        return;
    }
    let mi = a[2u * i];
    let mp = a[2u * i + 1u];
    let v = max(b[2u * i + 1u] - mi * mi, 0.0);
    let k = (b[2u * i] - mi * mp) / (v + pf(1u));
    dst[2u * i] = k;
    dst[2u * i + 1u] = mp - k * mi;
}

// `max(p, clamp(a·guide + b, 0, 1))` (a: guide, b: blurred coefficients, c: the input p).
@compute @workgroup_size(256)
fn xguided_apply(@builtin(global_invocation_id) g: vec3<u32>, @builtin(num_workgroups) nw: vec3<u32>) {
    let i = lin_index(g, nw);
    if (i >= pu(0u)) {
        return;
    }
    dst[i] = max(c[i], clamp(b[2u * i] * a[i] + b[2u * i + 1u], 0.0, 1.0));
}

// White balance: 3×3 matrix (P[1] = has matrix, P[2..10] row-major). P[11] = 0 keeps negatives
// (the orange mask in a working space); any other value clamps at 0.
@compute @workgroup_size(256)
fn wb_k(@builtin(global_invocation_id) g: vec3<u32>, @builtin(num_workgroups) nw: vec3<u32>) {
    let i = lin_index(g, nw);
    if (i >= pu(0u)) {
        return;
    }
    var c = rgb_a(i);
    if (pu(1u) != 0u) {
        c = vec3<f32>(
            pf(2u) * c.x + pf(3u) * c.y + pf(4u) * c.z,
            pf(5u) * c.x + pf(6u) * c.y + pf(7u) * c.z,
            pf(8u) * c.x + pf(9u) * c.y + pf(10u) * c.z,
        );
    }
    if (pu(11u) != 0u) {
        c = max(c, vec3<f32>(0.0));
    }
    put_rgb(i, c);
}

// Negative Craft (`lightcraft_pipeline::film::FilmKernel::apply`). P[1..] is `FilmKernel::words`:
// base 1–3, trim 4–6, flags 7 (1 negative, 2 mono), capture 8, dye 9–17 (row-major), fog 18–20,
// axis 21–23, contrast 24 (negative slope and slide power), mid 25, black 26, gain 27, offset 28,
// aim 29–31, chroma 32, balance 33–35, shadow 36–38, midtone 39–41, high 42–44, shoulder 45.
fn film_light(x: f32, slope: f32, mid: f32) -> f32 {
    return pow(10.0, clamp(slope * (x - mid), -12.0, 8.0));
}

// `FilmKernel::print` on each channel: scene light of a straightened density.
fn film_print(s: vec3<f32>) -> vec3<f32> {
    let slope = pf(24u) / 0.6;
    let mid = pf(25u);
    let floor = film_light(pf(26u), slope, mid);
    let light = vec3<f32>(film_light(s.x, slope, mid), film_light(s.y, slope, mid), film_light(s.z, slope, mid));
    let v = GREY * (light - vec3<f32>(floor)) / max(1.0 - floor, 1e-6);
    // `toe`: below TOE the black rolls off exponentially.
    let toe = 0.002;
    let rolled = toe * exp(max((v - vec3<f32>(toe)) / toe, vec3<f32>(-60.0)));
    return min(select(rolled, v, v >= vec3<f32>(toe)), vec3<f32>(64.0));
}

// `kernel::saturate`: scale the distance from luminance, no further than a channel reaching zero.
fn film_saturate(c: vec3<f32>, k: f32) -> vec3<f32> {
    if (abs(k - 1.0) <= 1e-4) {
        return c;
    }
    let y = lum2020(c);
    var room = k;
    for (var j = 0u; j < 3u; j++) {
        if (c[j] < y) {
            room = min(room, y / (y - c[j]));
        }
    }
    return max(vec3<f32>(y) + (c - vec3<f32>(y)) * room, vec3<f32>(0.0));
}

@compute @workgroup_size(256)
fn film_k(@builtin(global_invocation_id) g: vec3<u32>, @builtin(num_workgroups) nw: vec3<u32>) {
    let i = lin_index(g, nw);
    if (i >= pu(0u)) {
        return;
    }
    var t = rgb_a(i);
    // `capture_channel`: non-finite or negative transmission is 0 (NaN fails `t >= 0`).
    t = select(vec3<f32>(0.0), t, t >= vec3<f32>(0.0) & t < vec3<f32>(3.0e38));
    let capture = pf(8u);
    if (capture > 1.001) {
        t = pow(t, vec3<f32>(capture));
    }
    var base = vec3<f32>(pf(1u), pf(2u), pf(3u));
    let trim = vec3<f32>(pf(4u), pf(5u), pf(6u));
    let flags = pu(7u);
    if ((flags & 2u) != 0u) {
        t = vec3<f32>(lum2020(t));
        base = vec3<f32>(max(lum2020(base), 1e-4));
    }
    let eps = 1e-6;
    let gain = pf(27u);
    let offset = pf(28u);
    let chroma = pf(32u);
    var scene: vec3<f32>;
    if ((flags & 1u) != 0u) {
        // `density`, the dye matrix, the gray axis, and the channel trims about the middle.
        let raw = log(max(base, vec3<f32>(eps)) / max(t, vec3<f32>(eps))) / log(10.0);
        let d0 = clamp(raw, vec3<f32>(-1.0), vec3<f32>(5.0));
        // Hanson inverse, then a physical dye amount (not negative).
        let d = max(vec3<f32>(
            dot(vec3<f32>(pf(9u), pf(10u), pf(11u)), d0),
            dot(vec3<f32>(pf(12u), pf(13u), pf(14u)), d0),
            dot(vec3<f32>(pf(15u), pf(16u), pf(17u)), d0),
        ), vec3<f32>(0.0));
        let fog = vec3<f32>(pf(18u), pf(19u), pf(20u));
        let axis = vec3<f32>(pf(21u), pf(22u), pf(23u));
        let mid = vec3<f32>(pf(25u));
        let s = (d - fog) / axis;
        scene = film_print(mid + trim * (s - mid)) * vec3<f32>(pf(29u), pf(30u), pf(31u));
        scene = film_saturate(scene, chroma) * gain + vec3<f32>(offset);
    } else {
        scene = max(pow(max(t / base, vec3<f32>(eps)), trim) * gain + vec3<f32>(offset), vec3<f32>(0.0));
        let power = pf(24u);
        if (abs(power - 1.0) > 1e-4) {
            scene = pow(scene, vec3<f32>(power));
        }
        scene = film_saturate(scene, chroma);
    }
    // `FilmKernel::finish`: temperature and tint per tonal band (in stops about middle gray),
    // then the shoulder.
    var v = max(scene, vec3<f32>(0.0));
    let ev = log2(max(lum2020(v), 1e-6) / GREY);
    let sh = 1.0 - sstep(-3.0, -0.5, ev);
    let hi = sstep(0.5, 2.5, ev);
    let mid_w = max(1.0 - sh - hi, 0.0);
    let balance = vec3<f32>(pf(33u), pf(34u), pf(35u));
    let band = vec3<f32>(pf(36u), pf(37u), pf(38u)) * sh + vec3<f32>(pf(39u), pf(40u), pf(41u)) * mid_w
        + vec3<f32>(pf(42u), pf(43u), pf(44u)) * hi;
    v = v * max(balance * band, vec3<f32>(0.0));
    let shoulder = pf(45u);
    if (shoulder > 1e-4) {
        let e = max(v - vec3<f32>(shoulder), vec3<f32>(0.0));
        let rolled = vec3<f32>(shoulder) + e / (vec3<f32>(1.0) + e / vec3<f32>(shoulder));
        v = select(v, rolled, v > vec3<f32>(shoulder));
    }
    put_rgb(i, min(v, vec3<f32>(64.0)));
}

// Luminance NR: scale by 2^((f − l)·k) (a: image, b: log luminance l, c: filtered f). P[1] = k.
@compute @workgroup_size(256)
fn nr_lum(@builtin(global_invocation_id) g: vec3<u32>, @builtin(num_workgroups) nw: vec3<u32>) {
    let i = lin_index(g, nw);
    if (i >= pu(0u)) {
        return;
    }
    let d = (c[i] - b[i]) * pf(1u);
    put_rgb(i, rgb_a(i) * exp2(d));
}

// Chromaticity rgb / Y.
@compute @workgroup_size(256)
fn chroma_k(@builtin(global_invocation_id) g: vec3<u32>, @builtin(num_workgroups) nw: vec3<u32>) {
    let i = lin_index(g, nw);
    if (i >= pu(0u)) {
        return;
    }
    let c0 = rgb_a(i);
    let y = max(lum2020(c0), 1e-6);
    put_rgb(i, vec3<f32>(c0.x / y, c0.y / y, c0.z / y));
}

// Colour NR: blend chromaticity towards its blur and re-apply luminance (a: image, b: chroma,
// c: blurred chroma). P[1] = t.
@compute @workgroup_size(256)
fn nr_col(@builtin(global_invocation_id) g: vec3<u32>, @builtin(num_workgroups) nw: vec3<u32>) {
    let i = lin_index(g, nw);
    if (i >= pu(0u)) {
        return;
    }
    let yl = lum2020(rgb_a(i));
    let t = pf(1u);
    let c0 = vec3<f32>(b[3u * i], b[3u * i + 1u], b[3u * i + 2u]);
    let cb = vec3<f32>(c[3u * i], c[3u * i + 1u], c[3u * i + 2u]);
    put_rgb(i, max((c0 + (cb - c0) * t) * yl, vec3<f32>(0.0)));
}

// Every P[1]-th value of `a` (airlight sampling). P[0] = output count.
@compute @workgroup_size(256)
fn subsample(@builtin(global_invocation_id) g: vec3<u32>, @builtin(num_workgroups) nw: vec3<u32>) {
    let i = lin_index(g, nw);
    if (i >= pu(0u)) {
        return;
    }
    dst[i] = a[i * pu(1u)];
}

// `redeye::eye_pixel` (a: image). P[1] = width, P[2] = eye count, then EYE_WORDS per eye:
// cx, cy, r, darken, pet, has catchlight, catchlight x, y, r (output pixels).
@compute @workgroup_size(256)
fn redeye_k(@builtin(global_invocation_id) g: vec3<u32>, @builtin(num_workgroups) nw: vec3<u32>) {
    let i = lin_index(g, nw);
    if (i >= pu(0u)) {
        return;
    }
    let w = pu(1u);
    let x = f32(i % w) + 0.5;
    let y = f32(i / w) + 0.5;
    var c = rgb_a(i);
    for (var e = 0u; e < pu(2u); e++) {
        let o = 3u + e * EYE_WORDS;
        let ex = x - pf(o);
        let ey = y - pf(o + 1u);
        let d = sqrt(ex * ex + ey * ey) / pf(o + 2u);
        var m = 1.0 - sstep(1.0, 1.35, d);
        if (pf(o + 4u) != 0.0) {
            m = 1.0 - sstep(0.95, 1.2, d);
        }
        let darken = pf(o + 3u);
        if (m > 0.0) {
            if (pf(o + 4u) != 0.0) {
                let k = min(lum2020(c), 0.04) * (1.0 - 0.85 * darken);
                c = c + (vec3<f32>(k) - c) * m;
            } else {
                let red = (c.x - max(c.y, c.z)) / max(c.x, 1e-6);
                let a = m * sstep(0.25, 0.5, red);
                let n = 0.5 * (c.y + c.z) * (1.0 - 0.9 * darken);
                c = c + (vec3<f32>(n) - c) * a;
            }
        }
        if (pf(o + 5u) != 0.0) {
            let lx = x - pf(o + 6u);
            let ly = y - pf(o + 7u);
            let dc = sqrt(lx * lx + ly * ly) / pf(o + 8u);
            let mc = 1.0 - sstep(0.5, 1.0, dc);
            if (mc > 0.0) {
                c = c + (vec3<f32>(0.9) - c) * mc;
            }
        }
    }
    put_rgb(i, c);
}
