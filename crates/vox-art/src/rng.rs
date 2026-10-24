//! Deterministic randomness: FNV-1a to hash the seed string, SplitMix64 to expand it.
//! No global state, no platform entropy: the same seed always gives the same stream.

/// 64-bit FNV-1a over raw bytes.
pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// SplitMix64 finaliser (a good avalanche for hash outputs).
pub(crate) fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Well-mixed 64-bit hash of a seed string.
pub(crate) fn seed_hash(seed: &str) -> u64 {
    mix64(fnv1a64(seed.as_bytes()) ^ 0x5658_4f56_4f58_4152) // "VXOVOXAR"
}

/// SplitMix64 generator.
#[derive(Clone)]
pub(crate) struct Rng(u64);

impl Rng {
    /// A stream derived from `seed`; different `stream` values give independent sequences.
    pub fn new(seed: &str, stream: u64) -> Self {
        Rng(mix64(seed_hash(seed) ^ stream.wrapping_mul(0x9e37_79b9_7f4a_7c15)))
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        mix64(self.0)
    }

    /// Uniform in [0, 1).
    pub fn f(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 * (1.0 / (1u64 << 24) as f32)
    }

    /// Uniform in [a, b).
    pub fn range(&mut self, a: f32, b: f32) -> f32 {
        a + (b - a) * self.f()
    }

    /// -1.0 or 1.0.
    pub fn sign(&mut self) -> f32 {
        if self.next_u64() & 1 == 0 {
            -1.0
        } else {
            1.0
        }
    }

    /// Uniform index in 0..n (n > 0).
    pub fn below(&mut self, n: usize) -> usize {
        ((self.next_u64() >> 11) % n.max(1) as u64) as usize
    }

    pub fn chance(&mut self, p: f32) -> bool {
        self.f() < p
    }

    /// Index picked with the given (non-negative) weights.
    pub fn weighted(&mut self, weights: &[f32]) -> usize {
        let total: f32 = weights.iter().sum();
        let mut r = self.f() * total;
        for (i, w) in weights.iter().enumerate() {
            if r < *w {
                return i;
            }
            r -= w;
        }
        weights.len() - 1
    }
}

/// Stateless per-pixel hash noise in [0, 1). Used for grain and dither.
#[inline]
pub(crate) fn pixel_noise(x: u32, y: u32, salt: u32) -> f32 {
    let mut h = x.wrapping_mul(0x8da6_b343) ^ y.wrapping_mul(0xd816_3841) ^ salt.wrapping_mul(0xcb1a_b31f);
    h ^= h >> 16;
    h = h.wrapping_mul(0x7feb_352d);
    h ^= h >> 15;
    h = h.wrapping_mul(0x846c_a68b);
    h ^= h >> 16;
    (h >> 8) as f32 * (1.0 / (1u32 << 24) as f32)
}
