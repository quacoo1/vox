//! Procedural silk art for Vox (see docs/DESIGN.md, "Art").
//!
//! Every voice gets a deterministic, glossy abstract thumbnail made of layered satin ribbons,
//! and the app window gets a large dark (or light) silk backdrop. Rendering is pure Rust: each
//! ribbon is a band around a cubic Bezier centre line whose surface (rolled edges, twist,
//! folds, creases) is lit per pixel and mapped through Oklab tone ramps; layers get their own
//! depth-of-field blur and soft shadows; tiny-skia rasterises the Bezier light streaks and
//! writes PNGs. Same seed and size -> byte-identical pixels (all maths is plain f32, no libm).
//!
//! A seed's layout is checked on a tiny probe render first: if it comes out as a smooth wash
//! (no crease, rim or edge in the frame or in the icon crop) the layout is re-rolled, up to
//! four times. The choice depends only on the seed, so all sizes of one seed match.
//!
//! Callers render at 2x their logical size and should cache results: a 220x164 thumbnail
//! takes roughly 10 ms, a 1600x1000 backdrop under 100 ms on a laptop CPU.
//!
//! ```no_run
//! let art = vox_art::thumbnail("JBFqnCBsd6RMkjVDRZzb", 220, 164);
//! assert_eq!(art.rgba.len(), 220 * 164 * 4);
//! // slint::Image::from_rgba8(SharedPixelBuffer::clone_from_slice(&art.rgba, art.width, art.height))
//! ```

mod blur;
mod color;
mod compose;
mod fmath;
mod palette;
mod render;
mod rng;

pub use palette::Palette;

use color::{encode_srgb, Rgb};
use render::View;
use rng::pixel_noise;
use std::path::Path;

/// A rendered image: straight (not premultiplied) RGBA8, row-major, `width * height * 4` bytes.
/// Silk art is always fully opaque.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Art {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Art {
    /// Writes the image as an 8-bit RGBA PNG.
    pub fn save_png(&self, path: &Path) -> std::io::Result<()> {
        std::fs::write(path, self.encode_png()?)
    }

    /// Encodes the image as an 8-bit RGBA PNG in memory.
    pub fn encode_png(&self) -> std::io::Result<Vec<u8>> {
        // tiny-skia pixmaps are premultiplied; premultiply (a no-op for opaque art) and let
        // its encoder demultiply again.
        let mut data = self.rgba.clone();
        for px in data.as_chunks_mut::<4>().0 {
            let a = px[3] as u32;
            if a != 255 {
                for c in &mut px[..3] {
                    *c = ((*c as u32 * a + 127) / 255) as u8;
                }
            }
        }
        let size = tiny_skia::IntSize::from_wh(self.width, self.height).ok_or_else(|| std::io::Error::other("empty image"))?;
        let pixmap = tiny_skia::Pixmap::from_vec(data, size).ok_or_else(|| std::io::Error::other("bad image buffer"))?;
        pixmap.encode_png().map_err(std::io::Error::other)
    }

    /// Reads back pixel (x, y) as [r, g, b, a]. Panics if out of bounds.
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.width + x) * 4) as usize;
        [self.rgba[i], self.rgba[i + 1], self.rgba[i + 2], self.rgba[i + 3]]
    }
}

/// The palette a seed maps to. Stable for a given seed (it depends only on the seed's hash
/// and the order of [`Palette::ALL`]).
pub fn palette_for(seed: &str) -> Palette {
    let h = rng::seed_hash(seed);
    Palette::ALL[(h % Palette::ALL.len() as u64) as usize]
}

/// Renders the silk thumbnail for `seed` at any size and aspect ratio. Callers should render
/// at 2x their logical size (220x164 for a 110x82 slot, 88x88 for 44x44, 32x32 for 16x16).
/// Small sizes zoom into the composition so they stay colourful. A zero width or height gives
/// an empty image.
pub fn thumbnail(seed: &str, width: u32, height: u32) -> Art {
    let palette = palette_for(seed);
    thumbnail_styled(seed, palette, compose::pick_style(seed, palette), width, height)
}

/// Layouts tried per seed before settling for the one with the most structure.
const VARIANTS: u32 = 4;

/// Picks the seed's layout: the first variant whose probe render has real structure (a lit
/// edge, a crease, a rim) both across the frame and in the centre that small icons show.
/// A random layout can put the crease or the folds just outside the frame, which leaves a
/// smooth, generic gradient; those are re-rolled. Depends only on the seed, so every size of
/// one seed shows the same composition.
fn pick_variant(seed: &str, palette: Palette, style: palette::Style) -> u32 {
    let need = structure_floor(style);
    let mut best = (0, f32::MIN);
    for v in 0..VARIANTS {
        let score = structure_score(seed, palette, style, v);
        if score >= need {
            return v;
        }
        if score > best.1 {
            best = (v, score);
        }
    }
    best.0
}

/// How much structure a style must show (the out-of-focus bloom is soft by design).
fn structure_floor(style: palette::Style) -> f32 {
    match style {
        palette::Style::Bloom => 0.08,
        _ => 0.13,
    }
}

/// Probe-renders a layout at a tiny size and measures its structure over the whole frame and
/// over the centre crop that icons show; the weaker of the two counts.
fn structure_score(seed: &str, palette: Palette, style: palette::Style, variant: u32) -> f32 {
    let scene = compose::thumbnail_scene_with(seed, palette, style, 1.0, variant);
    let full = {
        let view = View::new(64, 48, 1.0, [0.0, 0.0]);
        structure(&render::render(&scene, &view), 64, 48)
    };
    let icon = {
        let view = View::new(40, 40, 0.6, [0.0, 0.0]);
        structure(&render::render(&scene, &view), 40, 40)
    };
    full.min(icon * 1.15)
}

/// Structure = the 90th percentile of the colour curvature |c(x-3) + c(x+3) - 2 c(x)| (sRGB,
/// both axes). Unlike a plain gradient magnitude it ignores linear ramps and broad vignettes,
/// so a smooth diagonal wash scores near zero while creases, rims, edges and ridges score high.
fn structure(lin: &[Rgb], w: usize, h: usize) -> f32 {
    let px: Vec<[f32; 3]> = lin
        .iter()
        .map(|c| [encode_srgb(shoulder(c.r)), encode_srgb(shoulder(c.g)), encode_srgb(shoulder(c.b))])
        .collect();
    let curve = |a: [f32; 3], m: [f32; 3], b: [f32; 3]| {
        let (x, y, z) = (a[0] + b[0] - 2.0 * m[0], a[1] + b[1] - 2.0 * m[1], a[2] + b[2] - 2.0 * m[2]);
        x * x + y * y + z * z
    };
    let r = 3;
    let mut g = Vec::with_capacity(w * h);
    for y in r..h.saturating_sub(r) {
        for x in r..w.saturating_sub(r) {
            let m = px[y * w + x];
            let gx = curve(px[y * w + x - r], m, px[y * w + x + r]);
            let gy = curve(px[(y - r) * w + x], m, px[(y + r) * w + x]);
            g.push((gx + gy).sqrt());
        }
    }
    if g.is_empty() {
        return 0.0;
    }
    g.sort_by(|a, b| a.total_cmp(b));
    g[(g.len() * 9) / 10]
}

fn thumbnail_styled(seed: &str, palette: Palette, style: palette::Style, width: u32, height: u32) -> Art {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 {
        return Art { width, height, rgba: Vec::new() };
    }
    let variant = pick_variant(seed, palette, style);
    let s = w.max(h) as f32;
    // 0 at icon sizes (~32 px), 1 at list-thumbnail sizes (~200 px and up).
    let detail = ((s - 32.0) / 170.0).clamp(0.0, 1.0);
    let scene = compose::thumbnail_scene_with(seed, palette, style, detail, variant);
    // Small icons show the centre of the composition, so features stay big and colourful.
    let extent = 0.6 + 0.4 * detail;
    let view = View::new(w, h, extent, [0.0, 0.0]);
    let lin = render::render(&scene, &view);
    let grain = 0.012 * detail;
    let salt = rng::seed_hash(seed) as u32;
    finish(&lin, w, grain, salt, width, height)
}

/// Renders the window backdrop: very dark navy with soft silver-blue silk (`dark`), or the
/// light "Paper" variant. Rendered at reduced resolution and upscaled (it is out of focus).
pub fn backdrop(width: u32, height: u32, dark: bool) -> Art {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 {
        return Art { width, height, rgba: Vec::new() };
    }
    let scene = compose::backdrop_scene(dark);
    let max_side = w.max(h);
    let factor = (max_side as f32 / 800.0).max(1.0);
    let lw = ((w as f32 / factor).round() as usize).max(1);
    let lh = ((h as f32 / factor).round() as usize).max(1);
    let view = View::new(lw, lh, 1.0, [0.0, 0.0]);
    let low = render::render(&scene, &view);
    let full = if lw == w && lh == h { low } else { upscale(&low, lw, lh, w, h) };
    let grain = if dark { 0.010 } else { 0.006 };
    finish(&full, w, grain, if dark { 0xd4 } else { 0x11 }, width, height)
}

/// A representative colour for tiny dots (sRGB). It is measured from the seed's own art,
/// favouring bright, saturated areas, so a dot matches its thumbnail.
pub fn accent(seed: &str) -> [u8; 3] {
    let art = thumbnail(seed, 24, 24);
    let mut acc = Rgb::BLACK;
    let mut wsum = 0.0f32;
    for px in art.rgba.as_chunks::<4>().0 {
        let c = Rgb::new(
            color::srgb_to_linear(px[0] as f32 / 255.0),
            color::srgb_to_linear(px[1] as f32 / 255.0),
            color::srgb_to_linear(px[2] as f32 / 255.0),
        );
        let mx = c.r.max(c.g).max(c.b);
        let mn = c.r.min(c.g).min(c.b);
        let chroma = mx - mn;
        let wgt = 0.02 + chroma * chroma * 4.0 + mx * 0.1;
        acc = acc + c * wgt;
        wsum += wgt;
    }
    let mut c = acc * (1.0 / wsum.max(1e-6));
    c = c.saturate(1.25);
    // Keep the dot visible on the dark panel.
    let l = c.luma();
    if l < 0.12 {
        let k = 0.12 / l.max(1e-4);
        c = c * k.min(6.0);
    }
    c.to_srgb8()
}

/// Tone map, encode to sRGB, add grain and dither, quantise.
fn finish(lin: &[Rgb], w: usize, grain: f32, salt: u32, width: u32, height: u32) -> Art {
    let mut out = vec![[0u8; 4]; lin.len()];
    render::par_rows(&mut out, w, |y0, rows| {
        for (ry, row) in rows.chunks_mut(w).enumerate() {
            let y = (y0 + ry) as u32;
            for (x, o) in row.iter_mut().enumerate() {
                let x = x as u32;
                let c = lin[y as usize * w + x as usize];
                let g = if grain > 0.0 { (pixel_noise(x, y, salt ^ 0x9e37) - 0.5) * grain } else { 0.0 };
                for (k, v) in [c.r, c.g, c.b].into_iter().enumerate() {
                    let t = encode_srgb(shoulder(v)) + g;
                    // Triangular dither of +-1 LSB so smooth gradients never band.
                    let d = pixel_noise(x, y, salt.wrapping_add((k as u32).wrapping_mul(0x632b_e5ab)))
                        + pixel_noise(x, y, salt.wrapping_add((k as u32).wrapping_mul(0x85eb_ca77).wrapping_add(1)))
                        - 1.0;
                    o[k] = (t * 255.0 + 0.5 + d).clamp(0.0, 255.0) as u8; // non-negative: truncation == floor
                }
                o[3] = 255;
            }
        }
    });
    Art { width, height, rgba: out.into_iter().flatten().collect() }
}

/// Soft highlight roll-off above 0.8 (linear), so speculars never clip hard.
#[inline]
fn shoulder(x: f32) -> f32 {
    let k = 0.8;
    if x <= k {
        x.max(0.0)
    } else {
        k + (1.0 - k) * (1.0 - fmath::exp(-(x - k) / (1.0 - k)))
    }
}

/// Separable Catmull-Rom upscale of a linear image.
fn upscale(src: &[Rgb], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<Rgb> {
    fn weights(t: f32) -> [f32; 4] {
        let t2 = t * t;
        let t3 = t2 * t;
        [
            0.5 * (-t3 + 2.0 * t2 - t),
            0.5 * (3.0 * t3 - 5.0 * t2 + 2.0),
            0.5 * (-3.0 * t3 + 4.0 * t2 + t),
            0.5 * (t3 - t2),
        ]
    }
    fn taps(n_src: usize, n_dst: usize) -> Vec<([usize; 4], [f32; 4])> {
        (0..n_dst)
            .map(|i| {
                let x = (i as f32 + 0.5) * n_src as f32 / n_dst as f32 - 0.5;
                let x0 = x.floor();
                let t = x - x0;
                let w = weights(t);
                let idx = [-1i64, 0, 1, 2].map(|k| (x0 as i64 + k).clamp(0, n_src as i64 - 1) as usize);
                (idx, w)
            })
            .collect()
    }
    let tx = taps(sw, dw);
    let ty = taps(sh, dh);
    // Horizontal pass.
    let mut mid = vec![Rgb::BLACK; dw * sh];
    for y in 0..sh {
        let row = &src[y * sw..(y + 1) * sw];
        for (x, (idx, wt)) in tx.iter().enumerate() {
            let mut c = Rgb::BLACK;
            for k in 0..4 {
                c = c + row[idx[k]] * wt[k];
            }
            mid[y * dw + x] = c;
        }
    }
    // Vertical pass.
    let mut out = vec![Rgb::BLACK; dw * dh];
    let mid = &mid;
    render::par_rows(&mut out, dw, |y0, rows| {
        for (ry, row) in rows.chunks_mut(dw).enumerate() {
            let y = y0 + ry;
            for (x, o) in row.iter_mut().enumerate() {
                let (idx, wt) = &ty[y];
                let mut c = Rgb::BLACK;
                for k in 0..4 {
                    c = c + mid[idx[k] * dw + x] * wt[k];
                }
                *o = Rgb::new(c.r.max(0.0), c.g.max(0.0), c.b.max(0.0));
            }
        }
    });
    out
}

#[doc(hidden)]
pub mod debug {
    //! Introspection used by the CLI's contact sheets. Not part of the stable API.
    use crate::palette::Style;

    pub const STYLES: [&str; 5] = ["drape", "ribbons", "crease", "arcs", "bloom"];

    fn name(s: Style) -> &'static str {
        STYLES[Style::ALL.iter().position(|x| *x == s).unwrap_or(0)]
    }

    /// The composition style a seed uses.
    pub fn style_name(seed: &str) -> &'static str {
        name(crate::compose::pick_style(seed, crate::palette_for(seed)))
    }

    /// Renders `seed` with a forced palette and style (index into `STYLES`).
    pub fn thumbnail_with(seed: &str, palette: crate::Palette, style: usize, w: u32, h: u32) -> crate::Art {
        crate::thumbnail_styled(seed, palette, Style::ALL[style % Style::ALL.len()], w, h)
    }

    /// The structure score of each layout variant of `seed` (see `pick_variant`), and the one chosen.
    pub fn variant_scores(seed: &str, palette: crate::Palette, style: usize) -> (Vec<f32>, u32) {
        let st = Style::ALL[style % Style::ALL.len()];
        let scores = (0..crate::VARIANTS).map(|v| crate::structure_score(seed, palette, st, v)).collect();
        (scores, crate::pick_variant(seed, palette, st))
    }
}
