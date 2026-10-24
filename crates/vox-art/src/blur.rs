//! Separable Gaussian blur on f32 buffers with clamp-to-edge borders.
//! Small radii use an exact Gaussian kernel; larger ones use three running box passes
//! (a standard, very close approximation that costs O(1) per pixel). Vertical passes walk
//! whole rows at a time so memory access stays sequential.

// Per-channel loops over `N` read best as index loops.
#![allow(clippy::needless_range_loop)]

/// Blurs an interleaved `N`-channel image in place. `sigma` is in pixels.
pub(crate) fn gaussian<const N: usize>(buf: &mut [[f32; N]], w: usize, h: usize, sigma: f32) {
    if w == 0 || h == 0 || sigma.is_nan() || sigma <= 0.25 {
        return;
    }
    let mut tmp = vec![[0.0f32; N]; w * h];
    if sigma < 1.25 {
        let kernel = gauss_kernel(sigma);
        for y in 0..h {
            convolve_row(&buf[y * w..(y + 1) * w], &mut tmp[y * w..(y + 1) * w], &kernel);
        }
        convolve_cols(&tmp, buf, w, h, &kernel);
    } else {
        let radii = box_radii(sigma);
        for &r in &radii {
            for y in 0..h {
                box_row(&buf[y * w..(y + 1) * w], &mut tmp[y * w..(y + 1) * w], r);
            }
            buf.copy_from_slice(&tmp);
        }
        let mut acc = vec![[0.0f32; N]; w];
        for &r in &radii {
            box_cols(buf, &mut tmp, &mut acc, w, h, r);
            buf.copy_from_slice(&tmp);
        }
    }
}

/// Like [`gaussian`], but large radii are computed on a 2x or 4x downsampled copy and
/// upsampled again (visually identical for wide blurs, and much cheaper).
pub(crate) fn gaussian_fast<const N: usize>(buf: &mut [[f32; N]], w: usize, h: usize, sigma: f32) {
    let f = if sigma >= 8.0 { 4 } else if sigma >= 4.0 { 2 } else { 1 };
    if f == 1 || w < 4 * f || h < 4 * f {
        gaussian(buf, w, h, sigma);
        return;
    }
    let (mut low, lw, lh) = downsample(buf, w, h, f);
    // The box downsample and bilinear upsample add roughly (f/2)^2 of variance.
    let s = (sigma * sigma - (f * f) as f32 * 0.25).max(0.0).sqrt() / f as f32;
    gaussian(&mut low, lw, lh, s);
    upsample_into(&low, lw, lh, f, buf, w, h);
}

/// Box-averages `f`x`f` blocks (edge blocks average what exists).
pub(crate) fn downsample<const N: usize>(src: &[[f32; N]], w: usize, h: usize, f: usize) -> (Vec<[f32; N]>, usize, usize) {
    let (lw, lh) = (w.div_ceil(f), h.div_ceil(f));
    let mut out = vec![[0.0f32; N]; lw * lh];
    let mut cnt = vec![0u32; lw * lh];
    for y in 0..h {
        let ly = y / f;
        let row = &src[y * w..(y + 1) * w];
        for (x, p) in row.iter().enumerate() {
            let i = ly * lw + x / f;
            for c in 0..N {
                out[i][c] += p[c];
            }
            cnt[i] += 1;
        }
    }
    for (o, n) in out.iter_mut().zip(cnt.iter()) {
        let k = 1.0 / (*n).max(1) as f32;
        for c in 0..N {
            o[c] *= k;
        }
    }
    (out, lw, lh)
}

/// Bilinear upsample of a grid produced by [`downsample`] (factor `f`) back to `w`x`h`.
pub(crate) fn upsample_into<const N: usize>(
    low: &[[f32; N]],
    lw: usize,
    lh: usize,
    f: usize,
    out: &mut [[f32; N]],
    w: usize,
    h: usize,
) {
    let inv = 1.0 / f as f32;
    let taps = |n: usize, ln: usize| -> Vec<(usize, usize, f32)> {
        (0..n)
            .map(|x| {
                let u = ((x as f32 + 0.5) * inv - 0.5).max(0.0);
                let i0 = (u as usize).min(ln - 1);
                let i1 = (i0 + 1).min(ln - 1);
                (i0, i1, u - i0 as f32)
            })
            .collect()
    };
    let tx = taps(w, lw);
    let ty = taps(h, lh);
    for (y, &(y0, y1, fy)) in ty.iter().enumerate() {
        let r0 = &low[y0 * lw..(y0 + 1) * lw];
        let r1 = &low[y1 * lw..(y1 + 1) * lw];
        let row = &mut out[y * w..(y + 1) * w];
        for (o, &(x0, x1, fx)) in row.iter_mut().zip(tx.iter()) {
            for c in 0..N {
                let a = r0[x0][c] + (r0[x1][c] - r0[x0][c]) * fx;
                let b = r1[x0][c] + (r1[x1][c] - r1[x0][c]) * fx;
                o[c] = a + (b - a) * fy;
            }
        }
    }
}

fn gauss_kernel(sigma: f32) -> Vec<f32> {
    let r = (sigma * 3.0).ceil() as i32;
    let mut k: Vec<f32> = (-r..=r).map(|i| (-(i * i) as f32 / (2.0 * sigma * sigma)).exp()).collect();
    let s: f32 = k.iter().sum();
    for v in &mut k {
        *v /= s;
    }
    k
}

fn convolve_row<const N: usize>(src: &[[f32; N]], dst: &mut [[f32; N]], kernel: &[f32]) {
    let n = src.len() as isize;
    let r = (kernel.len() / 2) as isize;
    for x in 0..n {
        let mut acc = [0.0f32; N];
        for (j, kv) in kernel.iter().enumerate() {
            let p = &src[(x + j as isize - r).clamp(0, n - 1) as usize];
            for c in 0..N {
                acc[c] += p[c] * kv;
            }
        }
        dst[x as usize] = acc;
    }
}

fn convolve_cols<const N: usize>(src: &[[f32; N]], dst: &mut [[f32; N]], w: usize, h: usize, kernel: &[f32]) {
    let r = (kernel.len() / 2) as isize;
    for y in 0..h as isize {
        let out = &mut dst[y as usize * w..(y as usize + 1) * w];
        for o in out.iter_mut() {
            *o = [0.0; N];
        }
        for (j, kv) in kernel.iter().enumerate() {
            let sy = (y + j as isize - r).clamp(0, h as isize - 1) as usize;
            let row = &src[sy * w..(sy + 1) * w];
            for (o, p) in out.iter_mut().zip(row.iter()) {
                for c in 0..N {
                    o[c] += p[c] * kv;
                }
            }
        }
    }
}

/// Radii of three box filters whose cascade approximates a Gaussian of `sigma`.
fn box_radii(sigma: f32) -> [usize; 3] {
    let n = 3.0f32;
    let w_ideal = (12.0 * sigma * sigma / n + 1.0).sqrt();
    let mut wl = w_ideal.floor() as i32;
    if wl % 2 == 0 {
        wl -= 1;
    }
    let wl = wl.max(1);
    let wu = wl + 2;
    let wlf = wl as f32;
    let m_ideal = (12.0 * sigma * sigma - n * wlf * wlf - 4.0 * n * wlf - 3.0 * n) / (-4.0 * wlf - 4.0);
    let m = m_ideal.round() as i32;
    let mut out = [0usize; 3];
    for (i, o) in out.iter_mut().enumerate() {
        let size = if (i as i32) < m { wl } else { wu };
        *o = ((size - 1) / 2) as usize;
    }
    out
}

fn box_row<const N: usize>(src: &[[f32; N]], dst: &mut [[f32; N]], r: usize) {
    if r == 0 {
        dst.copy_from_slice(src);
        return;
    }
    let n = src.len() as isize;
    let r = r as isize;
    let inv = 1.0 / (2 * r + 1) as f32;
    let get = |i: isize| &src[i.clamp(0, n - 1) as usize];
    let mut acc = [0.0f32; N];
    for i in -r..=r {
        let p = get(i);
        for c in 0..N {
            acc[c] += p[c];
        }
    }
    for x in 0..n {
        let o = &mut dst[x as usize];
        for c in 0..N {
            o[c] = acc[c] * inv;
        }
        let add = get(x + r + 1);
        let sub = get(x - r);
        for c in 0..N {
            acc[c] += add[c] - sub[c];
        }
    }
}

fn box_cols<const N: usize>(
    src: &[[f32; N]],
    dst: &mut [[f32; N]],
    acc: &mut [[f32; N]],
    w: usize,
    h: usize,
    r: usize,
) {
    if r == 0 {
        dst.copy_from_slice(src);
        return;
    }
    let hh = h as isize;
    let r = r as isize;
    let inv = 1.0 / (2 * r + 1) as f32;
    let row = |y: isize| {
        let y = y.clamp(0, hh - 1) as usize;
        &src[y * w..(y + 1) * w]
    };
    for a in acc.iter_mut() {
        *a = [0.0; N];
    }
    for i in -r..=r {
        for (a, p) in acc.iter_mut().zip(row(i).iter()) {
            for c in 0..N {
                a[c] += p[c];
            }
        }
    }
    for y in 0..hh {
        let out = &mut dst[y as usize * w..(y as usize + 1) * w];
        for (o, a) in out.iter_mut().zip(acc.iter()) {
            for c in 0..N {
                o[c] = a[c] * inv;
            }
        }
        let add = row(y + r + 1);
        let sub = row(y - r);
        for ((a, p), q) in acc.iter_mut().zip(add.iter()).zip(sub.iter()) {
            for c in 0..N {
                a[c] += p[c] - q[c];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blur_preserves_constant_and_mass() {
        let (w, h) = (37, 23);
        for sigma in [0.8f32, 4.0] {
            let mut buf = vec![[0.5f32]; w * h];
            gaussian(&mut buf, w, h, sigma);
            assert!(buf.iter().all(|p| (p[0] - 0.5).abs() < 1e-4));

            let mut buf = vec![[0.0f32]; w * h];
            buf[11 * w + 18] = [1.0];
            gaussian(&mut buf, w, h, sigma);
            let s: f32 = buf.iter().map(|p| p[0]).sum();
            assert!((s - 1.0).abs() < 1e-3, "mass {s}");
        }
    }
}
