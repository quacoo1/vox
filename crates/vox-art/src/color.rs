//! Colour helpers. All rendering happens in linear-light RGB (f32); palettes are written in
//! sRGB hex and ramps are interpolated in Oklab so gradients stay vivid instead of muddy.

use std::ops::{Add, Mul, Sub};
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Rgb {
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

impl Rgb {
    pub const BLACK: Rgb = Rgb { r: 0.0, g: 0.0, b: 0.0 };

    pub const fn new(r: f32, g: f32, b: f32) -> Self {
        Rgb { r, g, b }
    }

    /// From an sRGB hex literal (0xRRGGBB) to linear light.
    pub fn hex(v: u32) -> Self {
        let c = |s: u32| srgb_to_linear(((v >> s) & 0xff) as f32 / 255.0);
        Rgb::new(c(16), c(8), c(0))
    }

    pub fn lerp(self, o: Rgb, t: f32) -> Rgb {
        Rgb::new(
            self.r + (o.r - self.r) * t,
            self.g + (o.g - self.g) * t,
            self.b + (o.b - self.b) * t,
        )
    }

    pub fn luma(self) -> f32 {
        0.2126 * self.r + 0.7152 * self.g + 0.0722 * self.b
    }

    /// Scales chroma around luma (1 = unchanged).
    pub fn saturate(self, k: f32) -> Rgb {
        let l = self.luma();
        Rgb::new(
            (l + (self.r - l) * k).max(0.0),
            (l + (self.g - l) * k).max(0.0),
            (l + (self.b - l) * k).max(0.0),
        )
    }

    /// Linear RGB to sRGB bytes (no dither), for representative colours.
    pub fn to_srgb8(self) -> [u8; 3] {
        let q = |c: f32| (linear_to_srgb(c.clamp(0.0, 1.0)) * 255.0 + 0.5).floor().clamp(0.0, 255.0) as u8;
        [q(self.r), q(self.g), q(self.b)]
    }
}

impl Add for Rgb {
    type Output = Rgb;
    fn add(self, o: Rgb) -> Rgb {
        Rgb::new(self.r + o.r, self.g + o.g, self.b + o.b)
    }
}

impl Sub for Rgb {
    type Output = Rgb;
    fn sub(self, o: Rgb) -> Rgb {
        Rgb::new(self.r - o.r, self.g - o.g, self.b - o.b)
    }
}

impl Mul<f32> for Rgb {
    type Output = Rgb;
    fn mul(self, k: f32) -> Rgb {
        Rgb::new(self.r * k, self.g * k, self.b * k)
    }
}

impl Mul<Rgb> for Rgb {
    type Output = Rgb;
    fn mul(self, o: Rgb) -> Rgb {
        Rgb::new(self.r * o.r, self.g * o.g, self.b * o.b)
    }
}

pub(crate) fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

pub(crate) fn linear_to_srgb(c: f32) -> f32 {
    if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

const ENC_LUT_SIZE: usize = 4096;

/// Fast linear -> sRGB (0..1) with a lookup table and linear interpolation.
#[inline]
pub(crate) fn encode_srgb(c: f32) -> f32 {
    static LUT: OnceLock<Vec<f32>> = OnceLock::new();
    let lut = LUT.get_or_init(|| {
        (0..=ENC_LUT_SIZE)
            .map(|i| linear_to_srgb(i as f32 / ENC_LUT_SIZE as f32))
            .collect()
    });
    let x = c.clamp(0.0, 1.0) * ENC_LUT_SIZE as f32;
    let i = (x as usize).min(ENC_LUT_SIZE - 1);
    let f = x - i as f32;
    lut[i] + (lut[i + 1] - lut[i]) * f
}

// ---------------------------------------------------------------- Oklab

#[derive(Clone, Copy)]
struct Lab {
    l: f32,
    a: f32,
    b: f32,
}

fn to_oklab(c: Rgb) -> Lab {
    let l = 0.412_221_47 * c.r + 0.536_332_55 * c.g + 0.051_445_995 * c.b;
    let m = 0.211_903_5 * c.r + 0.680_699_5 * c.g + 0.107_396_96 * c.b;
    let s = 0.088_302_46 * c.r + 0.281_718_85 * c.g + 0.629_978_7 * c.b;
    let (l, m, s) = (l.max(0.0).cbrt(), m.max(0.0).cbrt(), s.max(0.0).cbrt());
    Lab {
        l: 0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        a: 1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
        b: 0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
    }
}

fn from_oklab(c: Lab) -> Rgb {
    let l = c.l + 0.396_337_78 * c.a + 0.215_803_76 * c.b;
    let m = c.l - 0.105_561_346 * c.a - 0.063_854_17 * c.b;
    let s = c.l - 0.089_484_18 * c.a - 1.291_485_5 * c.b;
    let (l, m, s) = (l * l * l, m * m * m, s * s * s);
    Rgb::new(
        (4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s).max(0.0),
        (-1.268_438 * l + 2.609_757_4 * m - 0.341_319_38 * s).max(0.0),
        (-0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s).max(0.0),
    )
}

// ---------------------------------------------------------------- Ramps

const RAMP_LUT: usize = 256;

/// A tone ramp (shadow -> highlight) sampled into a lookup table. The stops are joined with a
/// Catmull-Rom spline in Oklab, which avoids both muddy mid-points and Mach bands at the stops.
#[derive(Clone)]
pub(crate) struct Ramp {
    lut: Vec<Rgb>,
}

impl Ramp {
    pub fn new(stops: &[Rgb]) -> Self {
        let labs: Vec<Lab> = stops.iter().map(|c| to_oklab(*c)).collect();
        let n = labs.len();
        let get = |i: isize| -> Lab {
            if i < 0 {
                // Reflect the first segment so the curve starts straight.
                let (a, b) = (labs[0], labs[1.min(n - 1)]);
                Lab { l: 2.0 * a.l - b.l, a: 2.0 * a.a - b.a, b: 2.0 * a.b - b.b }
            } else if i as usize >= n {
                let (a, b) = (labs[n - 1], labs[n.saturating_sub(2)]);
                Lab { l: 2.0 * a.l - b.l, a: 2.0 * a.a - b.a, b: 2.0 * a.b - b.b }
            } else {
                labs[i as usize]
            }
        };
        let lut = (0..RAMP_LUT)
            .map(|k| {
                if n == 1 {
                    return stops[0];
                }
                let x = k as f32 / (RAMP_LUT - 1) as f32 * (n - 1) as f32;
                let i = (x.floor() as isize).min(n as isize - 2);
                let t = x - i as f32;
                let (p0, p1, p2, p3) = (get(i - 1), get(i), get(i + 1), get(i + 2));
                let cr = |a: f32, b: f32, c: f32, d: f32| {
                    let t2 = t * t;
                    let t3 = t2 * t;
                    0.5 * ((2.0 * b) + (-a + c) * t + (2.0 * a - 5.0 * b + 4.0 * c - d) * t2
                        + (-a + 3.0 * b - 3.0 * c + d) * t3)
                };
                from_oklab(Lab {
                    l: cr(p0.l, p1.l, p2.l, p3.l).max(0.0),
                    a: cr(p0.a, p1.a, p2.a, p3.a),
                    b: cr(p0.b, p1.b, p2.b, p3.b),
                })
            })
            .collect();
        Ramp { lut }
    }

    pub fn from_hex(stops: &[u32]) -> Self {
        let v: Vec<Rgb> = stops.iter().map(|h| Rgb::hex(*h)).collect();
        Ramp::new(&v)
    }

    /// Samples the ramp at t in [0, 1] (clamped).
    #[inline]
    pub fn at(&self, t: f32) -> Rgb {
        let x = t.clamp(0.0, 1.0) * (RAMP_LUT - 1) as f32;
        let i = (x as usize).min(RAMP_LUT - 2);
        let f = x - i as f32;
        self.lut[i].lerp(self.lut[i + 1], f)
    }
}

#[inline]
pub(crate) fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}
