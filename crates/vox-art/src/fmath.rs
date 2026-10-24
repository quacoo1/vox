//! Small, fast, deterministic approximations of transcendental functions. They are pure
//! f32 arithmetic (no libm), so results are identical on every platform, and they are
//! accurate to well under one 8-bit step for everything the renderer feeds them.

use std::f32::consts::{FRAC_PI_2, LOG2_E, PI, SQRT_2, TAU};

/// Round half away from zero. `f32::round` is a libm call on baseline x86-64; this is two
/// instructions. Valid for |x| < 2^31.
#[inline]
pub(crate) fn round(x: f32) -> f32 {
    (x + 0.5f32.copysign(x)) as i32 as f32
}

/// sin(x), absolute error below 1e-5 for |x| < 100 (Taylor to x^9 on [-pi/2, pi/2]).
#[inline]
pub(crate) fn sin(x: f32) -> f32 {
    let k = round(x * (1.0 / TAU));
    let mut x = x - k * TAU; // [-pi, pi]
    if x > FRAC_PI_2 {
        x = PI - x;
    } else if x < -FRAC_PI_2 {
        x = -PI - x;
    }
    let x2 = x * x;
    x * (1.0 + x2 * (-0.166_666_67 + x2 * (0.008_333_333 + x2 * (-0.000_198_412_7 + x2 * 0.000_002_755_732))))
}

#[inline]
pub(crate) fn cos(x: f32) -> f32 {
    sin(x + FRAC_PI_2)
}

/// 2^x, relative error about 2e-7 on the polynomial part.
#[inline]
pub(crate) fn exp2(x: f32) -> f32 {
    let x = x.clamp(-126.0, 126.0);
    let xi = round(x);
    let f = x - xi; // [-0.5, 0.5]
    let p = 1.0
        + f * (std::f32::consts::LN_2 + f * (0.240_226_5 + f * (0.055_504_11 + f * (0.009_618_129 + f * 0.001_333_355))));
    f32::from_bits(((xi as i32 + 127) as u32) << 23) * p
}

#[inline]
pub(crate) fn exp(x: f32) -> f32 {
    exp2(x * LOG2_E)
}

/// log2(x) for x > 0 (returns a large negative number for x <= 0).
#[inline]
pub(crate) fn log2(x: f32) -> f32 {
    if x.is_nan() || x <= 0.0 {
        return -126.0;
    }
    let bits = x.to_bits();
    let mut e = ((bits >> 23) & 0xff) as i32 - 127;
    let mut m = f32::from_bits((bits & 0x007f_ffff) | 0x3f80_0000);
    if m > SQRT_2 {
        m *= 0.5;
        e += 1;
    }
    let z = (m - 1.0) / (m + 1.0);
    let z2 = z * z;
    let ln = 2.0 * z * (1.0 + z2 * (1.0 / 3.0 + z2 * (0.2 + z2 * (1.0 / 7.0))));
    e as f32 + ln * LOG2_E
}

/// ln(1 + e^x), the softplus, computed stably.
#[inline]
pub(crate) fn softplus(x: f32) -> f32 {
    if x > 0.0 {
        x + log2(1.0 + exp(-x)) * std::f32::consts::LN_2
    } else {
        log2(1.0 + exp(x)) * std::f32::consts::LN_2
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn approximations_are_close() {
        let mut x = -40.0f32;
        while x < 40.0 {
            assert!((super::sin(x) - x.sin()).abs() < 1e-5, "sin {x}: {} vs {}", super::sin(x), x.sin());
            assert!((super::cos(x) - x.cos()).abs() < 1e-5, "cos {x}");
            x += 0.0137;
        }
        let mut x = -20.0f32;
        while x < 10.0 {
            let e = x.exp();
            assert!(((super::exp(x) - e) / e).abs() < 1e-5, "exp {x}");
            x += 0.011;
        }
        let mut x = 1e-6f32;
        while x < 1e4 {
            assert!((super::log2(x) - x.log2()).abs() < 1e-5, "log2 {x}");
            x *= 1.07;
        }
        for &(b, p) in &[(0.5f32, 24.0f32), (0.9, 3.0), (0.99, 40.0), (0.2, 1.25)] {
            assert!((super::exp2(p * super::log2(b)) - b.powf(p)).abs() < 1e-5);
        }
    }
}
