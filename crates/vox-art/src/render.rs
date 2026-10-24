//! The silk renderer. A scene is a background wash plus a stack of layers drawn back to front:
//!
//! * `Ribbon`: a wide band bent along a cubic Bezier centre line (in the ribbon's own frame).
//!   Its surface is a height field (rolled edges, cloth folds, a crease) that is lit per pixel
//!   with a soft diffuse term, a satin specular and a rim term, then mapped through tone ramps.
//! * `Streak`: a free-form Bezier shape rasterised with tiny-skia (light slivers, flames,
//!   specular streaks), filled with a ramp gradient.
//!
//! Every layer is blurred by its own depth-of-field radius and may cast a soft shadow on what
//! is below it. Everything is linear-light f32 until the very end.

use crate::blur;
use crate::fmath;
use crate::color::{smoothstep, Ramp, Rgb};
use tiny_skia::{FillRule, Mask, PathBuilder, Transform};

/// Maps output pixels to scene units. The scene's "frame" is the square [-0.5, 0.5]^2 scaled
/// to the image's longer side; `extent` < 1 zooms in (used for tiny icons).
#[derive(Clone, Copy, Debug)]
pub(crate) struct View {
    pub w: usize,
    pub h: usize,
    /// Pixels per scene unit.
    pub unit: f32,
    pub cx: f32,
    pub cy: f32,
}

impl View {
    pub fn new(w: usize, h: usize, extent: f32, center: [f32; 2]) -> Self {
        let s = w.max(h).max(1) as f32;
        View { w, h, unit: s / extent, cx: center[0], cy: center[1] }
    }

    /// The same scene region on a grid `f` times coarser (pixel blocks of the full grid).
    pub fn scaled(&self, f: usize) -> View {
        if f <= 1 {
            return *self;
        }
        let (lw, lh) = (self.w.div_ceil(f), self.h.div_ceil(f));
        View {
            w: lw,
            h: lh,
            unit: self.unit / f as f32,
            cx: self.cx + ((lw * f) as f32 * 0.5 - self.w as f32 * 0.5) / self.unit,
            cy: self.cy + ((lh * f) as f32 * 0.5 - self.h as f32 * 0.5) / self.unit,
        }
    }

    #[inline]
    pub fn scene(&self, px: usize, py: usize) -> [f32; 2] {
        [
            (px as f32 + 0.5 - self.w as f32 * 0.5) / self.unit + self.cx,
            (py as f32 + 0.5 - self.h as f32 * 0.5) / self.unit + self.cy,
        ]
    }

    #[inline]
    pub fn pixel_pos(&self, p: [f32; 2]) -> [f32; 2] {
        [
            (p[0] - self.cx) * self.unit + self.w as f32 * 0.5,
            (p[1] - self.cy) * self.unit + self.h as f32 * 0.5,
        ]
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Glow {
    pub pos: [f32; 2],
    /// Radii along the glow's own axes (scene units).
    pub radius: [f32; 2],
    pub angle: f32,
    pub color: Rgb,
    pub strength: f32,
}

#[derive(Clone)]
pub(crate) struct Background {
    /// Gradient ramp along `angle` across the frame.
    pub ramp: Ramp,
    pub angle: f32,
    pub glows: Vec<Glow>,
}

/// A cloth fold: slope amplitude (1 = 45 degrees), frequency (radians per scene unit across
/// the ribbon), phase, and skew (extra phase per unit along the ribbon).
pub(crate) type Fold = [f32; 4];

#[derive(Clone)]
pub(crate) struct Ribbon {
    pub o: [f32; 2],
    pub angle: f32,
    /// Centre line t(s) = c1 s + c2 s^2 + c3 s^3 in the ribbon frame (a cubic Bezier with
    /// evenly spaced control points along s).
    pub bend: [f32; 3],
    /// Extra sine wave on the centre line: amplitude, frequency, phase.
    pub wave: [f32; 3],
    pub half_w: f32,
    /// Relative width change per scene unit along s.
    pub taper: f32,
    /// Pointed start: the ribbon begins at s = tip.0 and reaches full width after tip.1.
    pub tip: Option<(f32, f32)>,
    pub folds: Vec<Fold>,
    /// Folds converge/spread along the ribbon.
    pub fan: f32,
    /// Fold amplitude modulation along s: amplitude, frequency, phase.
    pub twist: [f32; 3],
    /// Rolled edges: a half-cylinder cross-section (keep small for satin, 1 = a pipe).
    pub tube: f32,
    /// Gentle parabolic bulge across the ribbon (slope at the edges).
    pub curl: f32,
    /// Twist: the ribbon tilts across its width by base + amp * sin(freq * s + phase).
    pub tilt: [f32; 4],
    /// A crease across the ribbon: slope, offset (scene units from centre), width.
    pub crease: Option<[f32; 3]>,
    pub relief: f32,

    pub ramp_a: usize,
    pub ramp_b: usize,
    /// Ramp mix m = smoothstep(0, 1, base + per_s * s + per_d * d).
    pub mix: [f32; 3],
    pub tone_gain: f32,
    pub tone_bias: f32,
    pub ambient: f32,
    pub ao: f32,
    pub gloss: f32,
    pub shin: f32,
    pub rim: f32,
    pub rim_color: Rgb,
    /// Depth-of-field blur (Gaussian sigma, scene units).
    pub blur: f32,
    /// Extra edge softness (scene units).
    pub edge: f32,
    /// Strength of the soft shadow cast on layers below.
    pub shadow: f32,
    pub opacity: f32,
}

impl Ribbon {
    pub fn new(o: [f32; 2], angle: f32, half_w: f32) -> Self {
        Ribbon {
            o,
            angle,
            bend: [0.0; 3],
            wave: [0.0; 3],
            half_w,
            taper: 0.0,
            tip: None,
            folds: Vec::new(),
            fan: 0.0,
            twist: [0.0; 3],
            tube: 0.0,
            curl: 0.0,
            tilt: [0.0; 4],
            crease: None,
            relief: 1.0,
            ramp_a: 0,
            ramp_b: 0,
            mix: [0.0; 3],
            tone_gain: 0.86,
            tone_bias: 0.0,
            ambient: 0.22,
            ao: 0.3,
            gloss: 0.45,
            shin: 24.0,
            rim: 0.0,
            rim_color: Rgb::new(1.0, 1.0, 1.0),
            blur: 0.0,
            edge: 0.0,
            shadow: 0.0,
            opacity: 1.0,
        }
    }

    #[inline]
    fn centre(&self, s: f32) -> (f32, f32) {
        let [c1, c2, c3] = self.bend;
        let [wa, wf, wp] = self.wave;
        let mut t = c1 * s + c2 * s * s + c3 * s * s * s;
        let mut dt = c1 + 2.0 * c2 * s + 3.0 * c3 * s * s;
        if wa != 0.0 {
            let (sn, cs) = (fmath::sin(wf * s + wp), fmath::cos(wf * s + wp));
            t += wa * sn;
            dt += wa * wf * cs;
        }
        (t, dt)
    }

    /// Half-width at s and a 0..1 factor for the pointed tip.
    #[inline]
    fn width(&self, s: f32) -> (f32, f32) {
        let mut w = self.half_w * (1.0 + self.taper * s).max(0.08);
        let mut a = 1.0;
        if let Some((s0, len)) = self.tip {
            let k = ((s - s0) / len).clamp(0.0, 1.0);
            // Rounded-point profile: grows like sqrt, then eases into full width.
            let f = (k * (2.0 - k)).sqrt();
            w *= f.max(0.02);
            a = smoothstep(0.0, 0.08, k);
        }
        (w, a)
    }
}

/// A Bezier shape rasterised with tiny-skia.
#[derive(Clone)]
pub(crate) struct Streak {
    /// Closed outline: start point followed by cubic segments (c1, c2, end) in scene units.
    pub start: [f32; 2],
    pub cubics: Vec<[[f32; 2]; 3]>,
    pub ramp: usize,
    /// Gradient axis (scene units) and the ramp positions at its ends.
    pub axis: [[f32; 2]; 2],
    pub tones: [f32; 2],
    pub blur: f32,
    pub opacity: f32,
    /// Added as light instead of painted over.
    pub additive: bool,
}

#[derive(Clone)]
pub(crate) enum Layer {
    Ribbon(Ribbon),
    Streak(Streak),
}

#[derive(Clone)]
pub(crate) struct Scene {
    pub bg: Background,
    pub layers: Vec<Layer>,
    pub ramps: Vec<Ramp>,
    /// Direction towards the light (x right, y down, z towards the viewer).
    pub light: [f32; 3],
    pub sheen: Rgb,
    pub bloom: f32,
    pub bloom_threshold: f32,
    pub bloom_radius: f32,
    pub vignette: f32,
    pub exposure: f32,
    pub saturation: f32,
    /// Optional pool of light: tone and specular fall off away from it.
    pub spot: Option<Spot>,
}

/// A soft pool of light on the scene (like a studio spot on the cloth).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Spot {
    pub pos: [f32; 2],
    /// Gaussian radius (scene units).
    pub radius: f32,
    /// Light level far from the pool (0..1).
    pub floor: f32,
}

fn normalize3(v: [f32; 3]) -> [f32; 3] {
    let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-9);
    [v[0] / l, v[1] / l, v[2] / l]
}

/// Images at least this large are processed by several threads (row bands). Results are
/// identical either way: every pixel is computed independently.
const PAR_MIN_PIXELS: usize = 160_000;

/// Runs `f(first_row, rows)` over row bands of `data` (row length `w`), in parallel for big
/// images.
pub(crate) fn par_rows<T: Send>(data: &mut [T], w: usize, f: impl Fn(usize, &mut [T]) + Sync) {
    let h = data.len().checked_div(w).unwrap_or(0);
    let threads = if data.len() >= PAR_MIN_PIXELS {
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).clamp(1, 8)
    } else {
        1
    };
    if threads <= 1 || h < 2 * threads {
        f(0, data);
        return;
    }
    let band = h.div_ceil(threads);
    std::thread::scope(|s| {
        for (i, chunk) in data.chunks_mut(band * w).enumerate() {
            let f = &f;
            s.spawn(move || f(i * band, chunk));
        }
    });
}

/// Renders a scene to linear RGB.
///
/// Smooth or strongly defocused content is rendered at 1/2 or 1/4 resolution and upsampled:
/// the background wash, every layer whose depth-of-field blur is wide, shadows and bloom.
pub(crate) fn render(scene: &Scene, view: &View) -> Vec<Rgb> {
    let (w, h) = (view.w, view.h);
    let mut canvas = vec![Rgb::BLACK; w * h];

    // Background: glows are at least ~0.08 scene units wide, so 1 low-res pixel per
    // 0.02 units is plenty.
    let bf = ((0.02 * view.unit) as usize).clamp(1, 4);
    if bf > 1 {
        let lv = view.scaled(bf);
        let mut low = vec![Rgb::BLACK; lv.w * lv.h];
        draw_background(&scene.bg, &lv, &mut low);
        let low: Vec<[f32; 3]> = low.iter().map(|c| [c.r, c.g, c.b]).collect();
        let mut full = vec![[0.0f32; 3]; w * h];
        blur::upsample_into(&low, lv.w, lv.h, bf, &mut full, w, h);
        for (c, p) in canvas.iter_mut().zip(full.iter()) {
            *c = Rgb::new(p[0], p[1], p[2]);
        }
    } else {
        draw_background(&scene.bg, view, &mut canvas);
    }

    let mut layer = vec![[0.0f32; 4]; w * h];
    for l in &scene.layers {
        let (blur_su, shadow, additive) = match l {
            Layer::Ribbon(r) => (r.blur, r.shadow, false),
            Layer::Streak(s) => (s.blur, 0.0, s.additive),
        };
        let sigma = blur_su * view.unit;
        let f = if sigma >= 6.0 && w.min(h) >= 64 {
            4
        } else if sigma >= 2.5 && w.min(h) >= 32 {
            2
        } else {
            1
        };
        let lv = view.scaled(f);
        let mut low = if f == 1 { std::mem::take(&mut layer) } else { Vec::new() };
        low.clear();
        low.resize(lv.w * lv.h, [0.0; 4]);
        match l {
            Layer::Ribbon(r) => draw_ribbon(r, scene, &lv, &mut low),
            Layer::Streak(s) => draw_streak(s, scene, &lv, &mut low),
        }
        if shadow > 0.0 {
            cast_shadow(&low, &lv, f, &mut canvas, scene, view, shadow, blur_su);
        }
        // Bilinear upsampling adds a little blur of its own; take it out of the budget.
        let s_low = ((sigma * sigma - (f * f) as f32 * 0.15).max(0.0)).sqrt() / f as f32;
        blur::gaussian(&mut low, lv.w, lv.h, s_low);
        if f == 1 {
            layer = low;
        } else {
            blur::upsample_into(&low, lv.w, lv.h, f, &mut layer, w, h);
        }
        for (c, p) in canvas.iter_mut().zip(layer.iter()) {
            if additive {
                *c = *c + Rgb::new(p[0], p[1], p[2]);
            } else {
                let k = 1.0 - p[3];
                *c = Rgb::new(p[0] + c.r * k, p[1] + c.g * k, p[2] + c.b * k);
            }
        }
    }

    post(scene, view, &mut canvas);
    canvas
}

fn draw_background(bg: &Background, view: &View, out: &mut [Rgb]) {
    let (sa, ca) = (fmath::sin(bg.angle), fmath::cos(bg.angle));
    // Project the frame corners to find the gradient's extent.
    let mut lo = f32::MAX;
    let mut hi = f32::MIN;
    for (px, py) in [(0, 0), (view.w, 0), (0, view.h), (view.w, view.h)] {
        let p = view.scene(px.min(view.w.saturating_sub(1)), py.min(view.h.saturating_sub(1)));
        let u = p[0] * ca + p[1] * sa;
        lo = lo.min(u);
        hi = hi.max(u);
    }
    let span = (hi - lo).max(1e-4);
    let glows: Vec<(&Glow, f32, f32)> =
        bg.glows.iter().map(|g| (g, fmath::sin(g.angle), fmath::cos(g.angle))).collect();
    par_rows(out, view.w, |y0, rows| {
        for (ry, row) in rows.chunks_mut(view.w).enumerate() {
            let py = y0 + ry;
            for (px, o) in row.iter_mut().enumerate() {
                let p = view.scene(px, py);
                let u = ((p[0] * ca + p[1] * sa) - lo) / span;
                let mut c = bg.ramp.at(u);
                for (g, s, cs) in &glows {
                    let dx = p[0] - g.pos[0];
                    let dy = p[1] - g.pos[1];
                    let a = (dx * cs + dy * s) / g.radius[0];
                    let b = (-dx * s + dy * cs) / g.radius[1];
                    let e = fmath::exp(-(a * a + b * b));
                    c = c.lerp(g.color, (g.strength * e).min(1.0));
                }
                *o = c;
            }
        }
    });
}

/// Per-pixel surface sample of a ribbon.
#[derive(Clone, Copy, Default)]
struct Sample {
    /// Height (scene units).
    h: f32,
    /// Normalised fold height (-1..1), for occlusion.
    fold: f32,
    /// Along coordinate.
    s: f32,
    /// Across distance from the centre line.
    d: f32,
    /// Coverage 0..1.
    cov: f32,
}

fn draw_ribbon(r: &Ribbon, scene: &Scene, view: &View, out: &mut [[f32; 4]]) {
    let w = view.w;
    let ps = 1.0 / view.unit; // scene units per pixel
    let soft = r.edge.max(0.55 * ps);
    let (sa, ca) = (fmath::sin(r.angle), fmath::cos(r.angle));
    // Beyond this distance outside the edge nothing is visible or needed for normals.
    let margin = soft + 3.0 * ps;
    let fold_norm: f32 = r.folds.iter().map(|f| (f[0] / f[1].max(1e-3)).abs()).sum::<f32>().max(1e-6);
    let has_tilt = r.tilt[0] != 0.0 || r.tilt[1] != 0.0;

    // Pass 1: geometry and height.
    let mut samples = vec![Sample::default(); out.len()];
    par_rows(&mut samples, w, |y0, rows| {
        for (ry, row) in rows.chunks_mut(w).enumerate() {
            let py = y0 + ry;
            for (px, smp) in row.iter_mut().enumerate() {
                let p = view.scene(px, py);
                let dx = p[0] - r.o[0];
                let dy = p[1] - r.o[1];
                let s = dx * ca + dy * sa;
                let t = -dx * sa + dy * ca;
                let (tc, dtc) = r.centre(s);
                let d = (t - tc) / (1.0 + dtc * dtc).sqrt();
                let (hw, tip_a) = r.width(s);
                if d.abs() > hw + margin || tip_a <= 0.0 {
                    continue;
                }
                let v = (d / hw).clamp(-1.0, 1.0);

                let mut height = 0.0;
                if r.tube != 0.0 {
                    height += r.tube * hw * (1.0 - v * v).max(0.0).sqrt();
                }
                if r.curl != 0.0 {
                    height += r.curl * hw * 0.5 * (1.0 - v * v);
                }
                if has_tilt {
                    height += (r.tilt[0] + r.tilt[1] * fmath::sin(r.tilt[2] * s + r.tilt[3])) * d;
                }
                let mut fold = 0.0;
                if !r.folds.is_empty() {
                    let fan = (1.0 + r.fan * s).max(0.25);
                    let amp = 1.0 + r.twist[0] * fmath::sin(r.twist[1] * s + r.twist[2]);
                    for f in &r.folds {
                        let fr = f[1] / fan;
                        fold += f[0] / fr * fmath::sin(fr * d + f[2] + f[3] * s);
                    }
                    fold *= amp;
                    height += fold;
                }
                if let Some([k, d0, cw]) = r.crease {
                    // k * cw * ln(cosh(x)): the slope turns from -k to +k across the crease.
                    let x = (d - d0) / cw;
                    height += k * cw * (fmath::softplus(2.0 * x) - x - std::f32::consts::LN_2);
                }
                let edge = (hw - d.abs()) / (2.0 * soft) + 0.5;
                *smp = Sample {
                    h: height * r.relief,
                    fold: (fold / fold_norm).clamp(-1.0, 1.0),
                    s,
                    d,
                    cov: smoothstep(0.0, 1.0, edge) * tip_a * r.opacity,
                };
            }
        }
    });

    // Pass 2: lighting.
    let l = normalize3(scene.light);
    let hv = normalize3([l[0], l[1], l[2] + 1.0]);
    let ramp_a = &scene.ramps[r.ramp_a];
    let ramp_b = &scene.ramps[r.ramp_b];
    let same_ramp = r.ramp_a == r.ramp_b;
    let shin_broad = (r.shin * 0.18).max(2.0);
    let hi_a = ramp_a.at(1.0);
    let hi_b = ramp_b.at(1.0);
    let h = view.h;
    let samples = &samples;
    par_rows(out, w, |y0, rows| {
        for (ry, row) in rows.chunks_mut(w).enumerate() {
            let py = y0 + ry;
            for (px, o) in row.iter_mut().enumerate() {
                let smp = samples[py * w + px];
                let cov = smp.cov;
                if cov <= 1e-4 {
                    continue;
                }
                let (xl, xr) = (px.saturating_sub(1), (px + 1).min(w - 1));
                let (yu, yd) = (py.saturating_sub(1), (py + 1).min(h - 1));
                let hx = if xr > xl {
                    (samples[py * w + xr].h - samples[py * w + xl].h) / ((xr - xl) as f32 * ps)
                } else {
                    0.0
                };
                let hy = if yd > yu {
                    (samples[yd * w + px].h - samples[yu * w + px].h) / ((yd - yu) as f32 * ps)
                } else {
                    0.0
                };
                let nrm = normalize3([-hx, -hy, 1.0]);
                let ndl = nrm[0] * l[0] + nrm[1] * l[1] + nrm[2] * l[2];
                let wrap = ((ndl + 0.3) / 1.3).max(0.0);
                let diff = wrap * wrap.sqrt().sqrt(); // wrap^1.25
                let occl = 1.0 - r.ao * (0.5 - 0.5 * smp.fold);
                let pool = match &scene.spot {
                    Some(sp) => {
                        let p = view.scene(px, py);
                        let (dx, dy) = (p[0] - sp.pos[0], p[1] - sp.pos[1]);
                        sp.floor + (1.0 - sp.floor) * fmath::exp(-(dx * dx + dy * dy) / (sp.radius * sp.radius))
                    }
                    None => 1.0,
                };
                let tone = ((r.ambient + (1.0 - r.ambient) * diff) * occl * r.tone_gain * pool + r.tone_bias)
                    .clamp(0.0, 1.0);
                let m = smoothstep(0.0, 1.0, r.mix[0] + r.mix[1] * smp.s + r.mix[2] * smp.d);
                let mut c = if same_ramp { ramp_a.at(tone) } else { ramp_a.at(tone).lerp(ramp_b.at(tone), m) };
                let nh = (nrm[0] * hv[0] + nrm[1] * hv[1] + nrm[2] * hv[2]).max(0.0);
                if nh > 0.02 {
                    let lg = fmath::log2(nh);
                    let spec = r.gloss * pool * (fmath::exp2(r.shin * lg) + 0.15 * fmath::exp2(shin_broad * lg));
                    if spec > 1e-4 {
                        let hi = if same_ramp { hi_a } else { hi_a.lerp(hi_b, m) };
                        c = c + hi.lerp(scene.sheen, 0.5) * spec;
                    }
                }
                if r.rim > 0.0 {
                    let f = (1.0 - nrm[2]).clamp(0.0, 1.0);
                    let f2 = f * f;
                    c = c + r.rim_color * (r.rim * f2 * f2);
                }
                *o = [c.r * cov, c.g * cov, c.b * cov, cov];
            }
        }
    });
}

fn draw_streak(st: &Streak, scene: &Scene, view: &View, out: &mut [[f32; 4]]) {
    let (w, h) = (view.w, view.h);
    if w == 0 || h == 0 {
        return;
    }
    let Some(mut mask) = Mask::new(w as u32, h as u32) else { return };
    let mut pb = PathBuilder::new();
    let p0 = view.pixel_pos(st.start);
    pb.move_to(p0[0], p0[1]);
    for seg in &st.cubics {
        let a = view.pixel_pos(seg[0]);
        let b = view.pixel_pos(seg[1]);
        let c = view.pixel_pos(seg[2]);
        pb.cubic_to(a[0], a[1], b[0], b[1], c[0], c[1]);
    }
    pb.close();
    let Some(path) = pb.finish() else { return };
    mask.fill_path(&path, FillRule::Winding, true, Transform::identity());
    let data = mask.data();
    let ramp = &scene.ramps[st.ramp];
    let [a, b] = st.axis;
    let ax = [b[0] - a[0], b[1] - a[1]];
    let len2 = (ax[0] * ax[0] + ax[1] * ax[1]).max(1e-6);
    par_rows(out, w, |y0, rows| {
        for (ry, row) in rows.chunks_mut(w).enumerate() {
            let py = y0 + ry;
            for (px, o) in row.iter_mut().enumerate() {
                let m = data[py * w + px];
                if m == 0 {
                    continue;
                }
                let cov = m as f32 / 255.0 * st.opacity;
                let p = view.scene(px, py);
                let t = (((p[0] - a[0]) * ax[0] + (p[1] - a[1]) * ax[1]) / len2).clamp(0.0, 1.0);
                let c = ramp.at(st.tones[0] + (st.tones[1] - st.tones[0]) * t);
                *o = if st.additive {
                    [c.r * cov, c.g * cov, c.b * cov, 0.0]
                } else {
                    [c.r * cov, c.g * cov, c.b * cov, cov]
                };
            }
        }
    });
}

/// Darkens the canvas under a layer with a soft shadow offset away from the light. `layer`
/// is at the resolution of `lv` (factor `f` below `view`).
#[allow(clippy::too_many_arguments)]
fn cast_shadow(
    layer: &[[f32; 4]],
    lv: &View,
    f: usize,
    canvas: &mut [Rgb],
    scene: &Scene,
    view: &View,
    strength: f32,
    blur_su: f32,
) {
    let (w, h) = (view.w, view.h);
    let mut a: Vec<[f32; 1]> = layer.iter().map(|p| [p[3]]).collect();
    let radius = (0.03 + blur_su * 1.5) * lv.unit;
    blur::gaussian_fast(&mut a, lv.w, lv.h, radius);
    let a = if f > 1 {
        let mut full = vec![[0.0f32; 1]; w * h];
        blur::upsample_into(&a, lv.w, lv.h, f, &mut full, w, h);
        full
    } else {
        a
    };
    let l = normalize3(scene.light);
    let off = 0.022 * view.unit;
    let ox = fmath::round(-l[0] * off) as isize;
    let oy = fmath::round(-l[1] * off) as isize;
    let a = &a;
    par_rows(canvas, w, |y0, rows| {
        for (ry, row) in rows.chunks_mut(w).enumerate() {
            let sy = ((y0 + ry) as isize - oy).clamp(0, h as isize - 1) as usize;
            for (px, c) in row.iter_mut().enumerate() {
                let sx = (px as isize - ox).clamp(0, w as isize - 1) as usize;
                let k = 1.0 - strength * a[sy * w + sx][0];
                *c = *c * k;
            }
        }
    });
}

fn post(scene: &Scene, view: &View, canvas: &mut [Rgb]) {
    let (w, h) = (view.w, view.h);
    if scene.bloom > 0.0 {
        let t = scene.bloom_threshold;
        let mut bright: Vec<[f32; 3]> =
            canvas.iter().map(|c| [(c.r - t).max(0.0), (c.g - t).max(0.0), (c.b - t).max(0.0)]).collect();
        blur::gaussian_fast(&mut bright, w, h, scene.bloom_radius * view.unit);
        for (c, b) in canvas.iter_mut().zip(bright.iter()) {
            *c = *c + Rgb::new(b[0], b[1], b[2]) * scene.bloom;
        }
    }
    let half_diag = ((w * w + h * h) as f32).sqrt() * 0.5;
    par_rows(canvas, w, |y0, rows| {
        for (ry, row) in rows.chunks_mut(w).enumerate() {
            let py = y0 + ry;
            for (px, c) in row.iter_mut().enumerate() {
                let dx = px as f32 + 0.5 - w as f32 * 0.5;
                let dy = py as f32 + 0.5 - h as f32 * 0.5;
                let r = (dx * dx + dy * dy).sqrt() / half_diag.max(1e-3);
                let v = smoothstep(0.35, 1.1, r);
                let k = (1.0 - scene.vignette * v * v) * scene.exposure;
                *c = (*c * k).saturate(scene.saturation);
            }
        }
    });
}
