//! Turns a seed into a scene: picks a style for the palette, then lays out ribbons, folds,
//! light and lens settings. All randomness comes from the seed.

use crate::color::{Ramp, Rgb};
use crate::palette::{Palette, Spec, Style};
use crate::render::{Background, Glow, Layer, Ribbon, Scene, Spot, Streak};
use crate::fmath;
use crate::rng::Rng;
use std::f32::consts::{PI, TAU};

#[inline]
fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

fn dir(a: f32) -> [f32; 2] {
    [fmath::cos(a), fmath::sin(a)]
}

fn perp(a: f32) -> [f32; 2] {
    [-fmath::sin(a), fmath::cos(a)]
}

fn add(p: [f32; 2], v: [f32; 2], k: f32) -> [f32; 2] {
    [p[0] + v[0] * k, p[1] + v[1] * k]
}

/// A sheet (very wide ribbon) whose edge passes through `p` with direction `angle`.
/// `side` = +1 puts the sheet on the left of the direction of travel, -1 on the right.
fn sheet_with_edge(p: [f32; 2], angle: f32, half_w: f32, side: f32) -> Ribbon {
    let o = add(p, perp(angle), -side * half_w);
    Ribbon::new(o, angle, half_w)
}

/// Moves a ribbon's frame origin along its own direction, so the vertex of its bend leaves
/// the frame and the visible edge is a sweeping curve rather than a symmetric arch.
/// The centre line still passes through the old origin (so edges and creases stay where
/// they were placed).
fn offset_vertex(r: &mut Rng, rb: &mut Ribbon) {
    let k = r.sign() * r.range(0.35, 0.6);
    let [c1, c2, c3] = rb.bend;
    let s = -k; // the old origin, seen from the new one
    let tc = c1 * s + c2 * s * s + c3 * s * s * s;
    rb.o = add(add(rb.o, dir(rb.angle), k), perp(rb.angle), -tc);
}

/// A ribbon whose centre line follows (approximately) a circle around `c` with radius `rad`,
/// touching it at polar angle `at`.
fn arc_ribbon(c: [f32; 2], rad: f32, at: f32, half_w: f32) -> Ribbon {
    let p = add(c, dir(at), rad);
    // Tangent; its left normal points back at the centre, so a positive bend curves inwards.
    let mut r = Ribbon::new(p, at + PI * 0.5, half_w);
    r.bend = [0.0, 0.5 / rad.max(0.05), 0.0];
    r
}

/// A satin band: mostly flat with softly rolled edges and a twist along its length.
fn satin(r: &mut Rng, p: [f32; 2], angle: f32, half_w: f32) -> Ribbon {
    let mut rb = Ribbon::new(p, angle, half_w);
    rb.tube = r.range(0.05, 0.15);
    rb.curl = r.range(0.15, 0.45);
    rb.tilt = [r.range(-0.4, 0.4), r.range(0.5, 1.1), TAU * r.range(0.7, 1.6), r.range(0.0, TAU)];
    rb.ambient = 0.05;
    rb.ao = 0.3;
    rb.gloss = 0.45;
    rb.shin = 14.0;
    rb
}

pub(crate) struct Ctx<'a> {
    pub rng: Rng,
    pub spec: &'a Spec,
    /// 0 = tiny icon (about 32 px), 1 = full-size thumbnail.
    pub detail: f32,
}

pub(crate) fn pick_style(seed: &str, palette: Palette) -> Style {
    let mut rng = Rng::new(seed, 0x5157);
    Style::ALL[rng.weighted(&palette.spec().styles)]
}

/// Builds a thumbnail scene for a seed with an explicit style. `variant` re-rolls the layout
/// (0 is the seed's first choice; `lib.rs` moves on to 1, 2, ... when a layout comes out flat).
pub(crate) fn thumbnail_scene_with(seed: &str, palette: Palette, style: Style, detail: f32, variant: u32) -> Scene {
    let spec = palette.spec();
    let stream = if variant == 0 { 0xa47 } else { 0xa47 ^ (0x51_7cc1 * variant as u64) };
    let mut cx = Ctx { rng: Rng::new(seed, stream), spec, detail };

    // Ramp order varies per seed so one palette yields several looks.
    let first = cx.rng.weighted(&[0.6, 0.3, 0.1]);
    let second = if first == 0 {
        if cx.rng.chance(0.7) {
            1
        } else {
            2
        }
    } else {
        0
    };
    let third = 3 - first - second;
    let ramps = vec![spec.ramp(first), spec.ramp(second), spec.ramp(third)];
    let order = [first, second, third];

    let light = [cx.rng.range(-0.75, 0.55), cx.rng.range(-0.9, -0.3), cx.rng.range(0.38, 0.7)];

    let mut scene = Scene {
        bg: Background { ramp: spec.ramp(2), angle: 0.0, glows: Vec::new() },
        layers: Vec::new(),
        ramps,
        light,
        sheen: Rgb::hex(spec.sheen),
        bloom: 0.35,
        bloom_threshold: 0.7,
        bloom_radius: 0.05,
        vignette: 0.3,
        exposure: 1.0,
        saturation: 1.0 + 0.12 * (1.0 - detail),
        spot: None,
    };

    match style {
        Style::Drape => drape(&mut cx, &mut scene, order),
        Style::Ribbons => ribbons(&mut cx, &mut scene, order),
        Style::Crease => crease(&mut cx, &mut scene, order),
        Style::Arcs => arcs(&mut cx, &mut scene, order),
        Style::Bloom => bloom(&mut cx, &mut scene, order),
    }
    scene
}

fn bg_ramp(colors: &[Rgb]) -> Ramp {
    Ramp::new(colors)
}

/// Close-up of folded cloth filling the frame, with a second sheet folding over it.
fn drape(cx: &mut Ctx, sc: &mut Scene, o: [usize; 3]) {
    let spec = cx.spec;
    let r = &mut cx.rng;
    let a0 = r.range(0.0, PI);
    sc.bg = Background {
        ramp: bg_ramp(&[spec.ramp_color(o[2], 0), spec.ramp_color(o[2], 1), spec.ramp_color(o[0], 1)]),
        angle: r.range(0.0, TAU),
        glows: Vec::new(),
    };

    let mut base = Ribbon::new([r.range(-0.1, 0.1), r.range(-0.1, 0.1)], a0, 3.0);
    base.bend = [0.0, r.range(-1.0, 1.0), r.range(-0.6, 0.6)];
    let f1 = TAU * r.range(1.4, 2.3) * lerp(0.75, 1.0, cx.detail);
    base.folds = vec![
        [r.range(0.8, 1.2), f1, r.range(0.0, TAU), r.range(-4.0, 4.0)],
        [r.range(0.3, 0.55), f1 * r.range(1.7, 2.3), r.range(0.0, TAU), r.range(-6.0, 6.0)],
    ];
    base.fan = r.range(-0.9, 0.9);
    base.twist = [r.range(0.25, 0.5), r.range(3.0, 7.0), r.range(0.0, TAU)];
    base.ramp_a = 0;
    base.ramp_b = 1;
    base.mix = [r.range(0.2, 0.8), r.sign() * r.range(0.8, 1.6), 0.0];
    base.ambient = 0.04;
    base.ao = 0.5;
    base.gloss = 0.45;
    base.shin = 12.0;
    base.blur = r.range(0.004, 0.009);
    sc.layers.push(Layer::Ribbon(base));

    // A sheet folding over one side.
    let p = [r.range(-0.3, 0.3), r.range(-0.25, 0.25)];
    let a1 = a0 + r.range(-0.7, 0.7);
    let mut top = sheet_with_edge(p, a1, 1.0, r.sign());
    top.bend = [0.0, r.sign() * r.range(0.5, 1.3), r.range(-0.5, 0.5)];
    offset_vertex(r, &mut top);
    top.tube = r.range(0.15, 0.3);
    top.tilt = [r.range(-0.3, 0.3), r.range(0.3, 0.7), TAU * r.range(0.6, 1.4), r.range(0.0, TAU)];
    top.folds = vec![[r.range(0.4, 0.8), TAU * r.range(1.2, 2.0), r.range(0.0, TAU), r.range(-3.0, 3.0)]];
    top.ramp_a = if r.chance(0.5) { 1 } else { 0 };
    top.ramp_b = 2;
    top.mix = [r.range(0.0, 0.6), r.sign() * r.range(0.5, 1.2), 0.0];
    top.ambient = 0.04;
    top.ao = 0.35;
    top.gloss = 0.5;
    top.shin = 18.0;
    top.rim = 0.35;
    top.rim_color = spec.ramp_color(o[0], 3);
    top.shadow = 0.65;
    top.blur = r.range(0.0, 0.002);
    sc.layers.push(Layer::Ribbon(top));

    // Out-of-focus foreground fold in a corner.
    if r.chance(0.55) {
        let corner = [r.sign() * 0.5, r.sign() * 0.42];
        let a2 = r.range(0.0, PI);
        let hw = r.range(0.09, 0.16);
        let mut fg = satin(r, corner, a2, hw);
        fg.bend = [0.0, r.range(-1.5, 1.5), 0.0];
        fg.ramp_a = 2;
        fg.ramp_b = 2;
        fg.tone_gain = 0.75;
        fg.blur = r.range(0.025, 0.04);
        fg.shadow = 0.35;
        sc.layers.push(Layer::Ribbon(fg));
    }
    sc.bloom = 0.3;
}

/// Pleats of satin radiating from a gather point outside the frame, with light between them.
fn ribbons(cx: &mut Ctx, sc: &mut Scene, o: [usize; 3]) {
    let spec = cx.spec;
    let detail = cx.detail;
    let r = &mut cx.rng;
    // Bright background (the gaps), darkened towards one corner.
    sc.bg = Background {
        ramp: bg_ramp(&[spec.ramp_color(o[1], 2), spec.ramp_color(o[1], 3), spec.ramp_color(o[2], 3)]),
        angle: r.range(0.0, TAU),
        glows: vec![
            Glow {
                pos: [r.range(-0.35, 0.35), r.range(-0.35, 0.35)],
                radius: [0.3, 0.16],
                angle: r.range(0.0, PI),
                color: spec.ramp_color(o[2], 2),
                strength: 0.9,
            },
            Glow {
                pos: [r.sign() * 0.55, r.sign() * 0.5],
                radius: [0.4, 0.3],
                angle: 0.0,
                color: spec.ramp_color(o[0], 1),
                strength: 0.85,
            },
        ],
    };

    let tg = r.range(0.0, TAU);
    let gdist = r.range(0.75, 1.1);
    let g = [fmath::cos(tg) * gdist, fmath::sin(tg) * gdist];
    let base = tg + PI; // pointing from the gather point back into the frame
    let n = 3 + r.below(2);
    let spread = r.range(0.35, 0.6);
    let fg_blur = r.chance(0.35);
    let mut order: Vec<usize> = (0..n).collect();
    for i in (1..n).rev() {
        let j = r.below(i + 1);
        order.swap(i, j);
    }
    for (k, &i) in order.iter().enumerate() {
        let a = base + spread * (i as f32 / (n - 1) as f32 - 0.5) + r.range(-0.06, 0.06);
        let p = add(g, dir(a), gdist + r.range(-0.15, 0.1));
        let hw = r.range(0.09, 0.14) * lerp(1.2, 1.0, detail);
        let mut rb = satin(r, p, a, hw);
        rb.taper = r.range(0.5, 0.9);
        rb.bend = [0.0, r.range(-0.5, 0.5), r.range(-0.3, 0.3)];
        if k == 0 && r.chance(0.25) {
            rb.tip = Some((r.range(-0.45, -0.25), r.range(0.35, 0.5)));
        }
        // No inner folds: on narrow pleats they read as blotches. The twist does the work.
        rb.ramp_a = 0;
        rb.ramp_b = if r.chance(0.3) { 2 } else { 0 };
        rb.mix = [r.range(-0.5, 0.5), r.sign() * 1.5, 0.0];
        rb.tilt[1] *= 0.55;
        rb.tone_gain = 0.64;
        rb.gloss = 0.5;
        rb.shin = 16.0;
        rb.rim = 0.3;
        rb.rim_color = spec.ramp_color(o[1], 3);
        rb.shadow = 0.55;
        rb.blur = if fg_blur && k == n - 1 { 0.025 } else { r.range(0.0015, 0.006) };
        sc.layers.push(Layer::Ribbon(rb));
    }
    // A big sheet covering one side, like the navy cloth in the reference.
    if r.chance(0.5) {
        let p = add([0.0, 0.0], dir(tg), r.range(0.2, 0.35));
        let mut sh = sheet_with_edge(p, base + PI * 0.5 + r.range(-0.4, 0.4), 1.2, r.sign());
        sh.bend = [0.0, r.sign() * r.range(0.6, 1.3), 0.0];
        offset_vertex(r, &mut sh);
        sh.tube = 0.15;
        sh.tilt = [r.range(-0.3, 0.3), r.range(0.3, 0.6), TAU * r.range(0.6, 1.2), r.range(0.0, TAU)];
        sh.folds = vec![[r.range(0.5, 0.8), TAU * r.range(1.5, 2.5), r.range(0.0, TAU), r.range(-3.0, 3.0)]];
        sh.ramp_a = 0;
        sh.ramp_b = 0;
        sh.ambient = 0.05;
        sh.tone_gain = 0.6;
        sh.gloss = 0.5;
        sh.shin = 16.0;
        sh.rim = 0.35;
        sh.rim_color = spec.ramp_color(o[1], 3);
        sh.shadow = 0.55;
        sh.blur = r.range(0.002, 0.006);
        sh.ao = 0.45;
        sc.layers.insert(0, Layer::Ribbon(sh));
    }
    sc.bloom = 0.25;
}

/// Angle whose left normal points away from the light, so a crease across it has one face
/// towards the light and one away.
fn facing_light(light: [f32; 3]) -> f32 {
    light[1].atan2(light[0]) + PI * 0.5
}

/// A soft crease between a lit plane and a plane turning away.
fn crease(cx: &mut Ctx, sc: &mut Scene, o: [usize; 3]) {
    let spec = cx.spec;
    let r = &mut cx.rng;
    sc.bg = Background {
        ramp: bg_ramp(&[spec.ramp_color(o[1], 0), spec.ramp_color(o[1], 1), spec.ramp_color(o[0], 1)]),
        angle: r.range(0.0, TAU),
        glows: Vec::new(),
    };
    // Low light and a pool of light on the lit side: one face of the crease glows, the
    // rest of the cloth falls away towards black (the reference's crimson tile).
    // The pool is wide and the floor not too low: the lit face should fill a good part of the
    // frame (a small highlight on a flat dark plane reads as mud, especially as an icon).
    sc.light[2] = r.range(0.34, 0.52);
    let lxy = (sc.light[0] * sc.light[0] + sc.light[1] * sc.light[1]).sqrt().max(1e-3);
    let k = r.range(0.08, 0.22);
    sc.spot = Some(Spot {
        pos: [sc.light[0] / lxy * k + r.range(-0.1, 0.1), sc.light[1] / lxy * k + r.range(-0.1, 0.1)],
        radius: r.range(0.5, 0.72),
        floor: r.range(0.32, 0.45),
    });
    let a0 = facing_light(sc.light) + r.range(-0.5, 0.5);
    let mut sh = Ribbon::new([r.range(-0.12, 0.12), r.range(-0.12, 0.12)], a0, 3.0);
    sh.bend = [0.0, r.range(-0.9, 0.9), r.range(-0.4, 0.4)];
    offset_vertex(r, &mut sh);
    // Ridge (negative k) or valley; either way one face turns to the light.
    sh.crease = Some([r.sign() * r.range(0.9, 1.4), r.range(-0.1, 0.1), r.range(0.1, 0.24)]);
    // A slow, broad undulation gives the frame a vivid-to-deep gradient around the crease.
    sh.folds = vec![[r.range(0.55, 0.85), TAU * r.range(0.45, 0.7), r.range(0.0, TAU), r.range(-1.0, 1.0)]];
    sh.ramp_a = 0;
    sh.ramp_b = 1;
    sh.mix = [0.5, r.range(-0.8, 0.8), r.sign() * r.range(2.0, 4.0)];
    sh.ambient = 0.03;
    sh.ao = 0.25;
    sh.gloss = 0.55;
    sh.shin = 10.0;
    sh.tone_gain = 1.0;
    sh.blur = r.range(0.006, 0.014);
    sc.layers.push(Layer::Ribbon(sh));

    if r.chance(0.65) {
        let p = [r.range(-0.3, 0.3), r.range(-0.3, 0.3)];
        let mut s2 = sheet_with_edge(p, a0 + r.range(-0.9, 0.9), 1.0, r.sign());
        s2.bend = [0.0, r.sign() * r.range(0.6, 1.6), 0.0];
        offset_vertex(r, &mut s2);
        s2.tube = 0.15;
        s2.curl = 0.3;
        s2.tilt = [r.range(-0.5, 0.5), r.range(0.3, 0.6), TAU * r.range(0.5, 1.0), r.range(0.0, TAU)];
        s2.ramp_a = 1;
        s2.ramp_b = 0;
        s2.mix = [r.range(0.0, 0.5), r.sign(), 0.0];
        s2.ambient = 0.03;
        s2.gloss = 0.45;
        s2.shin = 12.0;
        s2.rim = 0.25;
        s2.rim_color = spec.ramp_color(o[0], 3);
        s2.blur = r.range(0.003, 0.025);
        s2.shadow = 0.6;
        sc.layers.push(Layer::Ribbon(s2));
    }
    sc.bloom = 0.4;
    sc.vignette = 0.42;
}

/// Broad curved sheets layered over each other with crisp rim light.
fn arcs(cx: &mut Ctx, sc: &mut Scene, o: [usize; 3]) {
    let spec = cx.spec;
    let r = &mut cx.rng;
    sc.bg = Background {
        ramp: bg_ramp(&[spec.ramp_color(o[0], 0), spec.ramp_color(o[0], 1), spec.ramp_color(o[2], 2)]),
        angle: r.range(0.0, TAU),
        glows: Vec::new(),
    };
    let c = [r.sign() * r.range(0.45, 0.9), r.sign() * r.range(0.45, 0.9)];
    let to_mid = [-c[0], -c[1]];
    let dist = (to_mid[0] * to_mid[0] + to_mid[1] * to_mid[1]).sqrt();
    let at0 = to_mid[1].atan2(to_mid[0]);
    let n = 2 + r.below(2);
    // Back to front: from the outermost sheet inwards, each one overlapping the last.
    let mut rad = dist + r.range(0.15, 0.3);
    for k in 0..n {
        let hw = r.range(0.12, 0.26);
        let mut rb = arc_ribbon(c, rad, at0 + r.range(-0.35, 0.35), hw);
        rb.tube = r.range(0.1, 0.22);
        rb.curl = r.range(0.25, 0.6);
        rb.tilt = [r.range(-0.5, 0.5), r.range(0.3, 0.7), TAU * r.range(0.5, 1.0), r.range(0.0, TAU)];
        rb.folds = vec![[r.range(0.1, 0.3), TAU * r.range(2.0, 3.5), r.range(0.0, TAU), r.range(-3.0, 3.0)]];
        rb.ramp_a = if k % 2 == 0 { 0 } else { 1 };
        rb.ramp_b = 2;
        rb.mix = [r.range(-0.3, 0.6), r.sign() * r.range(0.5, 1.5), 0.0];
        rb.ambient = 0.04;
        rb.gloss = 0.6;
        rb.shin = 24.0;
        rb.rim = 0.7;
        rb.rim_color = spec.ramp_color(2, 3);
        rb.shadow = 0.7;
        rb.blur = if k == 0 { r.range(0.006, 0.014) } else { r.range(0.0, 0.003) };
        sc.layers.push(Layer::Ribbon(rb));
        rad -= hw * r.range(1.1, 1.7);
    }
    sc.bloom = 0.3;
}

/// A strongly out-of-focus fold: soft colour fields, a glowing highlight and a second hue.
fn bloom(cx: &mut Ctx, sc: &mut Scene, o: [usize; 3]) {
    let spec = cx.spec;
    let r = &mut cx.rng;
    sc.bg = Background {
        ramp: bg_ramp(&[spec.ramp_color(o[0], 0), spec.ramp_color(o[0], 1), spec.ramp_color(o[0], 2)]),
        angle: r.range(0.0, TAU),
        glows: vec![Glow {
            pos: [r.range(-0.3, 0.3), r.range(-0.3, 0.3)],
            radius: [0.45, 0.3],
            angle: r.range(0.0, PI),
            color: spec.ramp_color(o[0], 2),
            strength: 0.7,
        }],
    };
    // The main fold, far out of focus, in low light with a soft pool of light.
    sc.light[2] = r.range(0.35, 0.55);
    sc.spot = Some(Spot {
        pos: [r.range(-0.3, 0.3), r.range(-0.25, 0.25)],
        radius: r.range(0.45, 0.65),
        floor: r.range(0.45, 0.6),
    });
    let a0 = facing_light(sc.light) + r.range(-0.6, 0.6);
    let mut sh = Ribbon::new([r.range(-0.1, 0.1), r.range(-0.1, 0.1)], a0, 3.0);
    sh.bend = [0.0, r.range(-1.0, 1.0), r.range(-0.4, 0.4)];
    offset_vertex(r, &mut sh);
    sh.crease = Some([r.sign() * r.range(0.8, 1.2), r.range(-0.12, 0.12), r.range(0.12, 0.22)]);
    sh.folds = vec![[r.range(0.3, 0.6), TAU * r.range(0.9, 1.5), r.range(0.0, TAU), r.range(-2.0, 2.0)]];
    sh.ramp_a = 0;
    sh.ramp_b = 2;
    sh.mix = [r.range(0.2, 0.8), r.sign() * r.range(1.0, 2.0), 0.0];
    sh.ambient = 0.04;
    sh.ao = 0.3;
    sh.gloss = 0.4;
    sh.shin = 9.0;
    sh.tone_gain = 0.85;
    sh.blur = r.range(0.03, 0.045);
    sc.layers.push(Layer::Ribbon(sh));

    // A band of the second hue along one side (the cyan edge in the reference).
    let side = [r.range(-0.35, 0.35), r.sign() * r.range(0.28, 0.42)];
    let (ba, bw) = (r.range(-0.4, 0.4), r.range(0.07, 0.12));
    let mut band = satin(r, side, ba, bw);
    band.bend = [0.0, r.range(-1.2, 1.2), 0.0];
    band.ramp_a = 1;
    band.ramp_b = 1;
    band.gloss = 0.7;
    band.shin = 12.0;
    band.rim = 0.35;
    band.rim_color = spec.ramp_color(o[2], 4);
    band.blur = r.range(0.022, 0.035);
    band.shadow = 0.3;
    sc.layers.push(Layer::Ribbon(band));

    // A soft light leak.
    let a = [r.range(-0.5, 0.0), r.range(-0.5, -0.1)];
    let b = [r.range(0.1, 0.5), r.range(-0.3, 0.2)];
    let mid = [(a[0] + b[0]) * 0.5 + r.range(-0.08, 0.08), (a[1] + b[1]) * 0.5 + r.range(-0.08, 0.08)];
    sc.layers.push(Layer::Streak(Streak {
        start: a,
        cubics: vec![
            [add(mid, [-0.08, -0.06], 1.0), add(mid, [0.1, -0.08], 1.0), b],
            [add(mid, [0.12, 0.1], 1.0), add(mid, [-0.06, 0.12], 1.0), a],
        ],
        ramp: 2,
        axis: [a, b],
        tones: [0.6, 0.85],
        blur: 0.05,
        opacity: 0.3,
        additive: true,
    }));
    sc.bloom = 0.35;
    sc.vignette = 0.38;
}

/// The window backdrop: very dark navy with soft, out-of-focus silver-blue silk sweeps (or the
/// light "Paper" variant with the same folds in white and pale blue-grey).
///
/// Layout (scene units, the frame is x in [-0.5, 0.5] and y in about [-0.31, 0.31] at 16:10):
/// a dark curtain bulging in from the left with a rolled, rim-lit edge; a fold rising from the
/// bottom; and a big sweep across the top right whose crest carries the brightest sheen.
pub(crate) fn backdrop_scene(dark: bool) -> Scene {
    let (ramps, bg, sheen, glow) = if dark {
        (
            vec![
                // Cloth: navy-black to silver-blue.
                Ramp::from_hex(&[0x010204, 0x040811, 0x0b1424, 0x22334c, 0x8ea2c0]),
                // Overlapping sheet: almost black navy.
                Ramp::from_hex(&[0x010203, 0x03060c, 0x08101c, 0x1a2639, 0x6a7d9a]),
                // Second hue on the cloth: blue-black with a steel-blue sheen.
                Ramp::from_hex(&[0x010203, 0x04080f, 0x0a1322, 0x203049, 0x8094b4]),
            ],
            Ramp::from_hex(&[0x010204, 0x050912, 0x0b1320]),
            Rgb::hex(0xa9b8d0),
            Rgb::hex(0x0b1424),
        )
    } else {
        (
            vec![
                Ramp::from_hex(&[0xa6afbd, 0xc5ccd6, 0xdde2e9, 0xeef1f5, 0xffffff]),
                Ramp::from_hex(&[0xb0b8c4, 0xccd2db, 0xe0e4ea, 0xf0f2f6, 0xfdfdfe]),
                Ramp::from_hex(&[0xa9b4c3, 0xc7cfda, 0xdee3ea, 0xeef1f5, 0xffffff]),
            ],
            Ramp::from_hex(&[0xe1e5eb, 0xe8ebf0, 0xf0f2f5]),
            Rgb::hex(0xffffff),
            Rgb::hex(0xf4f6f9),
        )
    };
    let tone = if dark { 0.55 } else { 0.95 };
    let mut layers = Vec::new();

    // The cloth: one sheet filling the frame with a few slow, sweeping folds whose crests
    // catch the light.
    let mut cloth = Ribbon::new([0.12, -0.08], 2.25, 3.0);
    cloth.bend = [0.0, 0.85, 0.25];
    cloth.folds = vec![[0.95, TAU * 1.05, 0.6, 1.6], [0.3, TAU * 2.2, 2.1, -1.8]];
    cloth.fan = 0.5;
    cloth.twist = [0.3, 4.0, 1.0];
    cloth.ramp_a = 0;
    cloth.ramp_b = 2;
    cloth.mix = [0.5, 1.2, 0.0];
    cloth.ambient = 0.04;
    cloth.ao = 0.5;
    cloth.tone_gain = tone;
    cloth.gloss = if dark { 0.75 } else { 0.35 };
    cloth.shin = 22.0;
    cloth.blur = 0.012;
    layers.push(Layer::Ribbon(cloth));

    // Left: a darker sheet whose edge bulges in from the left; its rolled lip faces the light.
    // Edge through (-0.3, 0), running downwards; the sheet lies to the left (perp side).
    let mut left = sheet_with_edge([-0.3, 0.0], PI * 0.5, 0.9, -1.0);
    left.bend = [0.1, 0.9, 0.0];
    left.tube = 0.3;
    left.curl = 0.5;
    left.tilt = [0.0, 0.15, TAU * 0.8, 1.0];
    left.ramp_a = 1;
    left.ramp_b = 2;
    left.mix = [0.3, 1.2, 0.0];
    left.ambient = 0.05;
    left.tone_gain = tone;
    left.gloss = if dark { 0.5 } else { 0.25 };
    left.shin = 16.0;
    left.rim = if dark { 0.22 } else { 0.08 };
    left.rim_color = sheen;
    left.shadow = if dark { 0.65 } else { 0.25 };
    left.blur = 0.004;
    layers.push(Layer::Ribbon(left));

    // Bottom right: a sheet sweeping up from the lower edge; only its rim-lit edge shows.
    let mut low = sheet_with_edge([0.3, 0.24], -0.5, 0.9, -1.0);
    low.bend = [0.0, 1.1, 0.0];
    low.tube = 0.28;
    low.curl = 0.4;
    low.ramp_a = 2;
    low.ramp_b = 1;
    low.mix = [0.5, 1.0, 0.0];
    low.ambient = 0.05;
    low.tone_gain = tone;
    low.gloss = if dark { 0.5 } else { 0.25 };
    low.shin = 18.0;
    low.rim = if dark { 0.2 } else { 0.08 };
    low.rim_color = sheen;
    low.shadow = if dark { 0.6 } else { 0.22 };
    low.blur = 0.007;
    layers.push(Layer::Ribbon(low));

    Scene {
        bg: Background {
            ramp: bg,
            angle: PI * 0.5,
            glows: vec![Glow { pos: [-0.05, 0.12], radius: [0.55, 0.3], angle: 0.25, color: glow, strength: 0.6 }],
        },
        layers,
        ramps,
        light: [0.5, -0.72, 0.48],
        sheen,
        bloom: if dark { 0.18 } else { 0.0 },
        bloom_threshold: 0.25,
        bloom_radius: 0.03,
        vignette: if dark { 0.3 } else { 0.05 },
        exposure: 1.0,
        saturation: 1.0,
        spot: None,
    }
}
