//! The silk palettes. Each palette has three tone ramps (shadow -> highlight, written in sRGB),
//! a background gradient, a specular tint, a representative accent and the composition styles
//! it prefers. The first five come straight from the reference thumbnails.

use crate::color::{Ramp, Rgb};

/// A named colour family for silk art.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Palette {
    /// Orange -> peach -> cyan.
    Ember,
    /// Lavender -> teal -> graphite, metallic.
    Opal,
    /// Pink -> navy -> orange.
    Dusk,
    /// Red -> oxblood -> black.
    Crimson,
    /// Navy -> charcoal -> silver.
    Steel,
    /// Deep green -> olive -> chartreuse/gold.
    Moss,
    /// Ice blue -> periwinkle -> white.
    Glacier,
    /// Amber -> yellow -> rose.
    Saffron,
    /// Violet -> magenta -> indigo.
    Orchid,
    /// Teal -> deep sea -> coral.
    Lagoon,
}

impl Palette {
    /// Every palette, in a stable order (`palette_for` indexes this array).
    pub const ALL: [Palette; 10] = [
        Palette::Ember,
        Palette::Opal,
        Palette::Dusk,
        Palette::Crimson,
        Palette::Steel,
        Palette::Moss,
        Palette::Glacier,
        Palette::Saffron,
        Palette::Orchid,
        Palette::Lagoon,
    ];

    /// Lower-case name ("ember", "opal", ...).
    pub fn name(self) -> &'static str {
        match self {
            Palette::Ember => "ember",
            Palette::Opal => "opal",
            Palette::Dusk => "dusk",
            Palette::Crimson => "crimson",
            Palette::Steel => "steel",
            Palette::Moss => "moss",
            Palette::Glacier => "glacier",
            Palette::Saffron => "saffron",
            Palette::Orchid => "orchid",
            Palette::Lagoon => "lagoon",
        }
    }

    /// Parses a name produced by [`Palette::name`] (case-insensitive).
    pub fn from_name(name: &str) -> Option<Palette> {
        Palette::ALL.iter().copied().find(|p| p.name().eq_ignore_ascii_case(name))
    }

    /// The palette's signature colour as sRGB bytes.
    pub fn accent(self) -> [u8; 3] {
        let a = self.spec().accent;
        [(a >> 16) as u8, (a >> 8) as u8, a as u8]
    }

    pub(crate) fn spec(self) -> &'static Spec {
        match self {
            Palette::Ember => &EMBER,
            Palette::Opal => &OPAL,
            Palette::Dusk => &DUSK,
            Palette::Crimson => &CRIMSON,
            Palette::Steel => &STEEL,
            Palette::Moss => &MOSS,
            Palette::Glacier => &GLACIER,
            Palette::Saffron => &SAFFRON,
            Palette::Orchid => &ORCHID,
            Palette::Lagoon => &LAGOON,
        }
    }
}

/// Composition styles (see `compose.rs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Style {
    /// A close-up of folded cloth filling the frame (reference: the opal thumbnail).
    Drape,
    /// Stacked ribbons with bright gaps between them (reference: the dusk thumbnail).
    Ribbons,
    /// One big soft crease between a lit and a shadowed plane (reference: the crimson thumbnail).
    Crease,
    /// Nested curved sheets with crisp rim light (reference: the steel thumbnail).
    Arcs,
    /// Strongly out of focus colour fields and a soft ribbon (reference: the ember thumbnail).
    Bloom,
}

impl Style {
    pub const ALL: [Style; 5] = [Style::Drape, Style::Ribbons, Style::Crease, Style::Arcs, Style::Bloom];
}

pub(crate) struct Spec {
    /// Three tone ramps, each from deepest shadow to specular highlight.
    pub ramps: [[u32; 5]; 3],
    /// Specular tint.
    pub sheen: u32,
    /// Representative colour for tiny dots.
    pub accent: u32,
    /// Weights for `Style::ALL`.
    pub styles: [f32; 5],
}

impl Spec {
    pub fn ramp(&self, i: usize) -> Ramp {
        Ramp::from_hex(&self.ramps[i % 3])
    }

    pub fn ramp_color(&self, ramp: usize, stop: usize) -> Rgb {
        Rgb::hex(self.ramps[ramp % 3][stop.min(4)])
    }
}

const EMBER: Spec = Spec {
    ramps: [
        [0x2a0710, 0x86210f, 0xdc5520, 0xf7934f, 0xffd9b0],
        [0x06183a, 0x0f4f96, 0x1f9fe0, 0x8fdcff, 0xf0fbff],
        [0x360b1a, 0xa23a3c, 0xec8f74, 0xffc8ad, 0xfff1e6],
    ],
    sheen: 0xfff0dc,
    accent: 0xf2793a,
    styles: [0.10, 0.20, 0.25, 0.05, 0.40],
};

const OPAL: Spec = Spec {
    ramps: [
        [0x0c1218, 0x283a44, 0x5b979c, 0xa6dbd4, 0xecfbf6],
        [0x120f24, 0x352f68, 0x7a6cc4, 0xb7aeee, 0xf1eeff],
        [0x07080a, 0x1c2127, 0x434d57, 0x88949f, 0xe2e8ee],
    ],
    sheen: 0xf0fffb,
    accent: 0x8fd0cc,
    styles: [0.50, 0.15, 0.10, 0.20, 0.05],
};

const DUSK: Spec = Spec {
    ramps: [
        [0x030817, 0x0b1c50, 0x1c4aa2, 0x5788d6, 0xc6dafe],
        // Pink whose shadows turn to plum and navy (the reference's pink-on-navy pleats), not maroon.
        [0x0e0a2c, 0x5a2868, 0xd8789f, 0xf7c0c6, 0xfff0ec],
        [0x2a0b03, 0x973610, 0xef7428, 0xffad6b, 0xfff0d6],
    ],
    sheen: 0xffe0d6,
    accent: 0xe98aa0,
    styles: [0.15, 0.50, 0.10, 0.10, 0.15],
};

const CRIMSON: Spec = Spec {
    ramps: [
        [0x0a0203, 0x4a0808, 0xb3170e, 0xf0472a, 0xffb48f],
        [0x040102, 0x1c0405, 0x520a0c, 0x8f1b17, 0xe0674a],
        [0x1a0402, 0x761907, 0xe23f1b, 0xff7b45, 0xffd2ab],
    ],
    sheen: 0xffc2a0,
    accent: 0xe2381f,
    styles: [0.10, 0.15, 0.50, 0.15, 0.10],
};

const STEEL: Spec = Spec {
    ramps: [
        [0x02050b, 0x0a1527, 0x1b3152, 0x5b7495, 0xd4dfec],
        [0x030406, 0x10141a, 0x29303b, 0x6c7787, 0xe0e6ee],
        [0x060910, 0x1a2434, 0x485a72, 0xa3b3c7, 0xf3f7fc],
    ],
    sheen: 0xeef4fb,
    accent: 0x7f97b8,
    styles: [0.35, 0.15, 0.05, 0.45, 0.0],
};

const MOSS: Spec = Spec {
    ramps: [
        [0x020a0a, 0x0a2a24, 0x2c6843, 0x7cae77, 0xe0f1ca],
        // Chartreuse / gold whose shadows fall to deep forest green, not olive drab.
        [0x020c0a, 0x0e3a28, 0x7f9c22, 0xd4d35a, 0xfaf6c8],
        [0x020f11, 0x093838, 0x28786b, 0x77c2a5, 0xe2faee],
    ],
    sheen: 0xf2f8d8,
    accent: 0x8fbf6a,
    styles: [0.35, 0.25, 0.10, 0.10, 0.20],
};

const GLACIER: Spec = Spec {
    ramps: [
        [0x040f1b, 0x0e3254, 0x2c7cb6, 0x8ac6ea, 0xf1faff],
        [0x09091f, 0x24286a, 0x5866c6, 0xa6b2ef, 0xf3f3ff],
        [0x09111a, 0x283848, 0x67879e, 0xc2dae6, 0xfcfeff],
    ],
    sheen: 0xffffff,
    accent: 0x6fb6e6,
    styles: [0.35, 0.20, 0.10, 0.25, 0.10],
};

const SAFFRON: Spec = Spec {
    ramps: [
        [0x2a0610, 0x82260a, 0xd87406, 0xf9bb22, 0xfff2c2],
        [0x1d0410, 0x681135, 0xcd3f6c, 0xf698b2, 0xffeff3],
        [0x2a0a0c, 0x8a420c, 0xe0a61a, 0xffe079, 0xfffbe6],
    ],
    sheen: 0xfff8e0,
    accent: 0xf2a516,
    styles: [0.15, 0.25, 0.20, 0.05, 0.35],
};

const ORCHID: Spec = Spec {
    ramps: [
        [0x0c0418, 0x30115a, 0x6a2fb0, 0xb07fe6, 0xf2e6ff],
        [0x1c0414, 0x620f45, 0xc22c86, 0xf07fc0, 0xffeaf6],
        [0x05061a, 0x161a52, 0x343c9e, 0x7a84d8, 0xeaecff],
    ],
    sheen: 0xfbeaff,
    accent: 0xb46be0,
    styles: [0.25, 0.25, 0.15, 0.15, 0.20],
};

const LAGOON: Spec = Spec {
    ramps: [
        [0x021014, 0x063c46, 0x0f8090, 0x5fc8cc, 0xdffaf6],
        [0x020812, 0x08223f, 0x154d7e, 0x4f8fbe, 0xd8ecfa],
        [0x200806, 0x7a2a1e, 0xe0674e, 0xffa98e, 0xfff0e8],
    ],
    sheen: 0xeafffb,
    accent: 0x2fb0b4,
    styles: [0.30, 0.25, 0.10, 0.15, 0.20],
};
