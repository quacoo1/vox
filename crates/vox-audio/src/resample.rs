//! Sample-rate conversion from the chunk's rate (44.1 kHz / 22.05 kHz) to the device rate.
//!
//! A streaming polyphase windowed-sinc resampler (Blackman window, 256 interpolated phases, cutoff at 95 % of the lower
//! Nyquist frequency). Upsampling uses 32 taps; downsampling (e.g. to a 16 kHz hands-free headset) widens the kernel
//! by `from / to`, so it always spans 16 zero crossings of the output-rate sinc on each side and the transition band
//! stays a fixed fraction of the output Nyquist frequency (aliases at least 60 dB down). Output frame `j` is the
//! band-limited value of the input at position
//! `j × from / to`, so positions map exactly and the output length of a whole stream is `ceil(input × to / from)`.
//! Equal rates are a bit-exact pass-through. Works on interleaved frames of any channel count; it never allocates
//! after its history buffer has grown to its working size.
//!
//! (BRIEF.md allows "rubato or a good-quality polyphase/cubic resampler"; a small owned implementation keeps the
//! streaming position maths exact and has no chunk-size constraints.)

use std::f64::consts::PI;

/// Half the filter length in input frames (taps per side) when upsampling; downsampling scales it by `from / to`.
const HALF_TAPS: usize = 16;
/// Upper bound of the widened kernel (only extreme ratios such as 384 kHz to 8 kHz reach it).
const MAX_HALF_TAPS: usize = 256;
/// Filter phases per input frame (linear interpolation between them).
const PHASES: usize = 256;

/// Streaming resampler.
pub struct Resampler {
    from: u32,
    to: u32,
    channels: u16,
    /// Taps per side and in total.
    half_taps: usize,
    taps: usize,
    /// `PHASES + 1` rows of `taps` coefficients (row `PHASES` = row 0 shifted by one tap, for interpolation).
    table: Vec<f32>,
    /// Interleaved input history; frame 0 of `hist` is absolute input frame `hist_base`.
    hist: Vec<f32>,
    hist_base: i64,
    /// Index of the next output frame; its input position is `next_out × from / to` (exact integer maths).
    next_out: i64,
    /// Absolute input frames received so far (excluding the zero prefix).
    received: i64,
    flushed: bool,
}

/// `sin(πx)/(πx)`.
fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        (PI * x).sin() / (PI * x)
    }
}

/// Blackman window over `[-1, 1]`.
fn blackman(u: f64) -> f64 {
    if u.abs() >= 1.0 {
        return 0.0;
    }
    let x = (u + 1.0) / 2.0; // 0..1
    0.42 - 0.5 * (2.0 * PI * x).cos() + 0.08 * (4.0 * PI * x).cos()
}

fn build_table(cutoff: f64, half_taps: usize) -> Vec<f32> {
    // Row p holds the taps for a fractional position `frac = p / PHASES`: tap k multiplies input frame
    // `floor(pos) - half_taps + 1 + k`, whose distance from `pos` is `d = k - (half_taps - 1) - frac`.
    let taps = 2 * half_taps;
    let mut table = vec![0f32; (PHASES + 1) * taps];
    let mut vals = vec![0f64; taps];
    for p in 0..=PHASES {
        let frac = p as f64 / PHASES as f64;
        let row = &mut table[p * taps..(p + 1) * taps];
        let mut sum = 0.0;
        for (k, v) in vals.iter_mut().enumerate() {
            let d = k as f64 - (half_taps as f64 - 1.0) - frac;
            *v = cutoff * sinc(cutoff * d) * blackman(d / half_taps as f64);
            sum += *v;
        }
        // Unity DC gain for every phase (no ripple at the phase rate).
        for (dst, v) in row.iter_mut().zip(vals.iter()) {
            *dst = if sum.abs() > 1e-12 { (v / sum) as f32 } else { 0.0 };
        }
    }
    table
}

impl Resampler {
    pub fn new(from: u32, to: u32, channels: u16) -> Self {
        let from_r = from.max(1);
        let to_r = to.max(1);
        let ratio = (to_r as f64 / from_r as f64).min(1.0);
        let cutoff = 0.95 * ratio;
        // Downsampling: as many input taps as 16 output-rate zero crossings per side need.
        let half_taps = ((HALF_TAPS as f64 / ratio).ceil() as usize).clamp(HALF_TAPS, MAX_HALF_TAPS);
        let table = if from_r == to_r { Vec::new() } else { build_table(cutoff, half_taps) };
        let mut r = Self {
            from: from_r,
            to: to_r,
            channels: channels.max(1),
            half_taps,
            taps: 2 * half_taps,
            table,
            hist: Vec::new(),
            hist_base: 0,
            next_out: 0,
            received: 0,
            flushed: false,
        };
        r.reset();
        r
    }

    fn passthrough(&self) -> bool {
        self.from == self.to
    }

    fn ch(&self) -> usize {
        self.channels as usize
    }

    /// Frames currently held in the history.
    fn hist_frames(&self) -> i64 {
        (self.hist.len() / self.ch()) as i64
    }

    /// `(floor, fraction)` of the input position of output frame `j`.
    fn position_of(&self, j: i64) -> (i64, f64) {
        let num = j as i128 * self.from as i128;
        let to = self.to as i128;
        let base = num.div_euclid(to);
        let rem = num.rem_euclid(to);
        (base as i64, rem as f64 / to as f64)
    }

    /// Computes every output frame whose filter support lies within the history and, when `end` is given, whose
    /// position is before absolute input frame `end`.
    fn produce(&mut self, out: &mut Vec<f32>, end: Option<i64>) {
        let ch = self.ch();
        let (half_taps, taps) = (self.half_taps, self.taps);
        let hist_end = self.hist_base + self.hist_frames();
        let mut acc = [0f32; 8];
        loop {
            let (base, frac) = self.position_of(self.next_out);
            if let Some(end) = end {
                // position < end  ⟺  next_out × from < end × to
                if self.next_out as i128 * self.from as i128 >= end as i128 * self.to as i128 {
                    break;
                }
            }
            let first = base - half_taps as i64 + 1;
            if first + taps as i64 > hist_end {
                break;
            }
            let fp = frac * PHASES as f64;
            let p0 = (fp.floor() as usize).min(PHASES - 1);
            let w1 = (fp - p0 as f64) as f32;
            let w0 = 1.0 - w1;
            let r0 = &self.table[p0 * taps..(p0 + 1) * taps];
            let r1 = &self.table[(p0 + 1) * taps..(p0 + 2) * taps];
            let start = ((first - self.hist_base) as usize) * ch;
            if ch <= acc.len() {
                acc[..ch].iter_mut().for_each(|a| *a = 0.0);
                for k in 0..taps {
                    let c = r0[k] * w0 + r1[k] * w1;
                    let frame = &self.hist[start + k * ch..start + (k + 1) * ch];
                    for (a, s) in acc[..ch].iter_mut().zip(frame) {
                        *a += c * s;
                    }
                }
                out.extend_from_slice(&acc[..ch]);
            } else {
                for c_i in 0..ch {
                    let mut a = 0f32;
                    for k in 0..taps {
                        let c = r0[k] * w0 + r1[k] * w1;
                        a += c * self.hist[start + k * ch + c_i];
                    }
                    out.push(a);
                }
            }
            self.next_out += 1;
        }
        // Drop history no longer needed by the next output.
        let (next_base, _) = self.position_of(self.next_out);
        let keep_from = next_base - half_taps as i64 + 1;
        let drop = (keep_from - self.hist_base).clamp(0, self.hist_frames());
        if drop > 0 {
            self.hist.drain(..drop as usize * ch);
            self.hist_base += drop;
        }
    }

    /// Converts `input` (interleaved) and appends to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let ch = self.ch();
        let frames = input.len() / ch;
        if frames == 0 {
            return;
        }
        if self.passthrough() {
            out.extend_from_slice(&input[..frames * ch]);
            self.received += frames as i64;
            return;
        }
        self.flushed = false;
        self.hist.extend_from_slice(&input[..frames * ch]);
        self.received += frames as i64;
        self.produce(out, None);
    }

    /// Emits the tail: every output frame whose position lies before the end of the input received so far (the
    /// filter reads zeros past it). The stream can continue afterwards only after [`Resampler::reset`].
    pub fn flush(&mut self, out: &mut Vec<f32>) {
        if self.passthrough() || self.flushed {
            return;
        }
        self.flushed = true;
        let ch = self.ch();
        self.hist.extend(std::iter::repeat_n(0.0, self.taps * ch));
        self.produce(out, Some(self.received));
    }

    pub fn reset(&mut self) {
        let ch = self.ch();
        self.hist.clear();
        // Zero history before the first input frame so output 0 is centred on input 0.
        self.hist.extend(std::iter::repeat_n(0.0, (self.half_taps - 1) * ch));
        self.hist_base = -(self.half_taps as i64 - 1);
        self.next_out = 0;
        self.received = 0;
        self.flushed = false;
    }

    pub fn ratio(&self) -> f64 {
        self.to as f64 / self.from as f64
    }

    pub fn from_rate(&self) -> u32 {
        self.from
    }

    pub fn to_rate(&self) -> u32 {
        self.to
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Input frames received but not yet represented in the output (the filter's look-ahead), in input frames.
    pub fn latency_frames(&self) -> f64 {
        if self.passthrough() {
            0.0
        } else {
            let (base, frac) = self.position_of(self.next_out);
            (self.received as f64 - (base as f64 + frac)).max(0.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(sr: u32, freq: f64, frames: usize, channels: u16) -> Vec<f32> {
        let mut v = Vec::with_capacity(frames * channels as usize);
        for i in 0..frames {
            let s = (2.0 * PI * freq * i as f64 / sr as f64).sin() as f32 * 0.5;
            for _ in 0..channels {
                v.push(s);
            }
        }
        v
    }

    /// Frequency by counting rising zero crossings (mono).
    fn zero_cross_freq(x: &[f32], sr: u32) -> f64 {
        let mut crossings = Vec::new();
        for i in 1..x.len() {
            if x[i - 1] < 0.0 && x[i] >= 0.0 {
                // Linear interpolation of the crossing instant.
                let t = (i - 1) as f64 + (-x[i - 1] as f64) / ((x[i] - x[i - 1]) as f64);
                crossings.push(t);
            }
        }
        let n = crossings.len();
        assert!(n >= 3);
        (n - 1) as f64 / ((crossings[n - 1] - crossings[0]) / sr as f64)
    }

    #[test]
    fn equal_rates_are_bit_exact() {
        let input = sine(48_000, 440.0, 1000, 2);
        let mut r = Resampler::new(48_000, 48_000, 2);
        let mut out = Vec::new();
        r.process(&input[..600], &mut out);
        r.process(&input[600..], &mut out);
        r.flush(&mut out);
        assert_eq!(out, input);
        assert_eq!(r.ratio(), 1.0);
    }

    #[test]
    fn length_ratio_streaming() {
        for &(from, to) in
            &[(22_050u32, 48_000u32), (44_100, 48_000), (48_000, 44_100), (22_050, 44_100), (44_100, 22_050)]
        {
            let frames = 22_050usize;
            let input = sine(from, 300.0, frames, 1);
            let mut r = Resampler::new(from, to, 1);
            let mut out = Vec::new();
            // Uneven pieces.
            let mut i = 0;
            let mut piece = 1;
            while i < frames {
                let n = piece.min(frames - i);
                r.process(&input[i..i + n], &mut out);
                i += n;
                piece = piece * 3 % 997 + 1;
            }
            r.flush(&mut out);
            let expected = (frames as f64 * to as f64 / from as f64).ceil() as usize;
            assert_eq!(out.len(), expected, "{from} → {to}");
            assert!((r.ratio() - to as f64 / from as f64).abs() < 1e-12);
        }
    }

    #[test]
    fn keeps_frequency_and_level() {
        let from = 22_050;
        let to = 48_000;
        let input = sine(from, 220.0, 22_050, 1);
        let mut r = Resampler::new(from, to, 1);
        let mut out = Vec::new();
        r.process(&input, &mut out);
        r.flush(&mut out);
        // Ignore the edges (filter ramp-in/out).
        let mid = &out[2000..out.len() - 2000];
        let f = zero_cross_freq(mid, to);
        assert!((f - 220.0).abs() < 0.5, "{f}");
        let peak = mid.iter().fold(0f32, |m, x| m.max(x.abs()));
        assert!((peak - 0.5).abs() < 0.01, "{peak}");
    }

    #[test]
    fn positions_are_centred() {
        // An impulse at input frame 100 appears at output ≈ 100 × ratio.
        let from = 22_050;
        let to = 44_100;
        let mut input = vec![0f32; 400];
        input[100] = 1.0;
        let mut r = Resampler::new(from, to, 1);
        let mut out = Vec::new();
        r.process(&input, &mut out);
        r.flush(&mut out);
        let (argmax, _) =
            out.iter().enumerate().fold((0, 0f32), |(bi, bv), (i, v)| if *v > bv { (i, *v) } else { (bi, bv) });
        assert_eq!(argmax, 200);
    }

    #[test]
    fn stereo_channels_stay_separate() {
        let from = 44_100;
        let to = 48_000;
        let mut input = Vec::new();
        for i in 0..4410 {
            input.push((2.0 * PI * 200.0 * i as f64 / from as f64).sin() as f32 * 0.5);
            input.push(0.0);
        }
        let mut r = Resampler::new(from, to, 2);
        let mut out = Vec::new();
        r.process(&input, &mut out);
        r.flush(&mut out);
        assert_eq!(out.len() % 2, 0);
        let right_peak = out.iter().skip(1).step_by(2).fold(0f32, |m, x| m.max(x.abs()));
        let left_peak = out.iter().step_by(2).fold(0f32, |m, x| m.max(x.abs()));
        assert!(right_peak < 1e-6, "{right_peak}");
        assert!(left_peak > 0.45);
    }

    /// Peak of the middle of `x` (skipping the filter ramp-in and ramp-out).
    fn mid_peak(x: &[f32]) -> f32 {
        let edge = x.len() / 5;
        x[edge..x.len() - edge].iter().fold(0f32, |m, v| m.max(v.abs()))
    }

    fn convert(from: u32, to: u32, freq: f64) -> Vec<f32> {
        let input = sine(from, freq, from as usize / 2, 1);
        let mut r = Resampler::new(from, to, 1);
        let mut out = Vec::new();
        r.process(&input, &mut out);
        r.flush(&mut out);
        out
    }

    /// Finding: downsampling to a 16 kHz headset must not fold sibilants above 8 kHz back into the band.
    #[test]
    fn downsampling_rejects_aliases() {
        for &from in &[44_100u32, 48_000] {
            let to = 16_000;
            // Above the output Nyquist frequency (8 kHz): 10 kHz would alias to 6 kHz.
            for &freq in &[9_500.0, 10_000.0, 12_000.0] {
                let peak = mid_peak(&convert(from, to, freq));
                let db = 20.0 * (peak as f64 / 0.5).log10();
                assert!(db <= -60.0, "{from} -> {to}: {freq} Hz only {db:.1} dB down");
            }
            // The speech band passes unchanged.
            for &freq in &[300.0, 1_000.0, 4_000.0, 6_500.0] {
                let peak = mid_peak(&convert(from, to, freq));
                let db = 20.0 * (peak as f64 / 0.5).log10();
                assert!(db.abs() < 0.1, "{from} -> {to}: {freq} Hz at {db:.2} dB");
            }
        }
        // Upsampling keeps its 32 taps.
        assert_eq!(Resampler::new(22_050, 48_000, 1).taps, 32);
        assert_eq!(Resampler::new(44_100, 16_000, 1).taps, 2 * 45);
    }

    #[test]
    fn reset_restarts_the_stream() {
        let input = sine(22_050, 300.0, 1000, 1);
        let mut r = Resampler::new(22_050, 48_000, 1);
        let mut a = Vec::new();
        r.process(&input, &mut a);
        r.flush(&mut a);
        r.reset();
        let mut b = Vec::new();
        r.process(&input, &mut b);
        r.flush(&mut b);
        assert_eq!(a, b);
        // A second flush adds nothing.
        let before = b.len();
        r.flush(&mut b);
        assert_eq!(b.len(), before);
    }
}
