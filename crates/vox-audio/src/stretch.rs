//! WSOLA time-stretch: tempo 0.5×–3× without pitch change, speech quality (≈20–30 ms frames, overlap-add with a
//! similarity search window). At rate 1.0 it is an exact pass-through.
//!
//! Algorithm (Verhelst & Roelands' WSOLA, output-driven and streaming): 20 ms periodic-Hann frames are overlap-added
//! every 10 ms of output (synthesis hop `Hs`; 50 % overlap sums to exactly 1). Frame *k* is taken from the source
//! near its nominal position `p0 + k × Hs × rate`, shifted by up to ±10 ms to the offset whose content best matches
//! the natural continuation of frame *k − 1* (normalised cross-correlation; coarse search on a decimated grid, then
//! a fine search around the winner). A start primes the overlap buffer so the first hop reproduces the source
//! exactly (no fade-in after a seek); the end of a finite source is drained at the original speed (the last few
//! tens of milliseconds, i.e. trailing silence of a TTS chunk).
//!
//! Streaming across chunks: [`Wsola::render_view`] reads a two-segment [`SrcView`] (the current chunk and, once
//! decoded, the next one), so windows straddle a chunk join and the state carries over (gapless, ARCHITECTURE.md
//! §5.3). The caller rebases (`Wsola::rebase`) when it drops the first segment.
//!
//! Positions: `*pos` is the nominal source position (frames) reached by the output produced so far; it advances by
//! exactly `rate` source frames per output frame, so the player can tag output blocks with source positions.

use vox_core::constants::{RATE_MAX, RATE_MIN};

/// Two consecutive interleaved PCM segments seen as one stream (`b` may be empty). Frames outside read as silence.
#[derive(Debug, Clone, Copy)]
pub struct SrcView<'a> {
    pub a: &'a [f32],
    pub b: &'a [f32],
    pub channels: usize,
}

impl<'a> SrcView<'a> {
    pub fn single(src: &'a [f32], channels: usize) -> Self {
        Self { a: src, b: &[], channels: channels.max(1) }
    }

    fn a_frames(&self) -> usize {
        self.a.len() / self.channels
    }

    /// Total frames in both segments.
    pub fn frames(&self) -> usize {
        self.a_frames() + self.b.len() / self.channels
    }

    fn frame(&self, i: i64) -> Option<&'a [f32]> {
        if i < 0 {
            return None;
        }
        let i = i as usize;
        let c = self.channels;
        let af = self.a_frames();
        if i < af {
            self.a.get(i * c..(i + 1) * c)
        } else {
            self.b.get((i - af) * c..(i - af + 1) * c)
        }
    }

    /// Mono mix of frame `i` (silence outside).
    fn mono(&self, i: i64) -> f32 {
        match self.frame(i) {
            Some(f) if f.len() == 1 => f[0],
            Some(f) => f.iter().sum::<f32>() / f.len() as f32,
            None => 0.0,
        }
    }

    /// Copies `n` interleaved frames starting at `start` into `dst` (silence outside).
    pub fn copy_frames(&self, start: i64, n: usize, dst: &mut [f32]) {
        let c = self.channels;
        for k in 0..n {
            let d = &mut dst[k * c..(k + 1) * c];
            match self.frame(start + k as i64) {
                Some(f) => d.copy_from_slice(f),
                None => d.iter_mut().for_each(|x| *x = 0.0),
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// No frame placed since the last reset.
    Fresh,
    Running,
    /// The tail of a finite source is in `pending`.
    Draining,
    /// Everything of a finite source has been rendered.
    Done,
}

#[derive(Debug, Clone, Copy)]
struct Drain {
    start_pos: f64,
    end_pos: f64,
    total: usize,
    delivered: usize,
}

/// Streaming WSOLA over one PCM buffer.
pub struct Wsola {
    sample_rate: u32,
    channels: u16,
    rate: f64,
    /// Frame length (even) and synthesis hop (half of it), in frames.
    n: usize,
    hs: usize,
    /// Search tolerance ± frames and the coarse search step.
    delta: usize,
    coarse: usize,
    window: Vec<f32>,
    phase: Phase,
    /// Overlap accumulator (`n` interleaved frames); `[0, hs)` holds the previous frame's windowed second half.
    acc: Vec<f32>,
    /// Source start of the last frame placed.
    prev_p: i64,
    pending: Vec<f32>,
    pending_off: usize,
    drain: Option<Drain>,
    tgt: Vec<f32>,
    reg: Vec<f32>,
}

impl Wsola {
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        let sr = sample_rate.max(1);
        let ch = channels.max(1);
        let hs = ((sr as f64 * 0.010).round() as usize).max(4);
        let n = 2 * hs;
        let window =
            (0..n).map(|i| (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n as f64).cos()) as f32).collect();
        Self {
            sample_rate: sr,
            channels: ch,
            rate: 1.0,
            n,
            hs,
            delta: hs,
            coarse: ((sr / 11_025) as usize).clamp(1, 8),
            window,
            phase: Phase::Fresh,
            acc: vec![0.0; n * ch as usize],
            prev_p: 0,
            pending: Vec::new(),
            pending_off: 0,
            drain: None,
            tgt: Vec::new(),
            reg: Vec::new(),
        }
    }

    /// Clamped to 0.5..3. Moving to or away from exactly 1 (the pass-through) resets the state.
    pub fn set_rate(&mut self, rate: f64) {
        let r = if rate.is_finite() { rate.clamp(RATE_MIN, RATE_MAX) } else { 1.0 };
        if (r == 1.0) != (self.rate == 1.0) {
            self.reset();
        }
        self.rate = r;
    }

    pub fn rate(&self) -> f64 {
        self.rate
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Drops internal overlap state (after a seek or chunk change).
    pub fn reset(&mut self) {
        self.phase = Phase::Fresh;
        self.acc.iter_mut().for_each(|x| *x = 0.0);
        self.prev_p = 0;
        self.pending.clear();
        self.pending_off = 0;
        self.drain = None;
    }

    /// The source has been rendered to its end (a finite source was drained).
    pub fn is_done(&self) -> bool {
        self.phase == Phase::Done
    }

    /// The first source frame still needed for the next output (`i64::MAX` when nothing more is read), given the
    /// caller's nominal position. The caller may drop source frames before it.
    pub fn earliest_needed(&self, pos: f64) -> i64 {
        if self.rate == 1.0 {
            return pos.round() as i64;
        }
        match self.phase {
            Phase::Fresh => pos.round() as i64,
            Phase::Running => (self.prev_p + self.hs as i64).min(pos.round() as i64 - self.delta as i64).max(0),
            Phase::Draining | Phase::Done => i64::MAX,
        }
    }

    /// Shifts internal source positions after the caller dropped `frames` frames from the front of its view.
    pub fn rebase(&mut self, frames: usize) {
        self.prev_p -= frames as i64;
        if let Some(d) = &mut self.drain {
            d.start_pos -= frames as f64;
            d.end_pos -= frames as f64;
        }
    }

    /// Renders up to `out.len() / channels` frames from `src` (interleaved) starting at source frame `*pos`,
    /// advancing `*pos` by the source frames consumed. Returns the frames written (0 at the end of `src`).
    pub fn render(&mut self, src: &[f32], pos: &mut f64, out: &mut [f32]) -> usize {
        let view = SrcView::single(src, self.channels as usize);
        self.render_view(&view, pos, out, true)
    }

    /// Like [`Wsola::render`] over a two-segment view. `eos`: no more source will follow the view (drain the tail);
    /// otherwise rendering stops where a frame would need source that is not there yet.
    pub fn render_view(&mut self, view: &SrcView<'_>, pos: &mut f64, out: &mut [f32], eos: bool) -> usize {
        let c = self.channels as usize;
        let want = out.len() / c;
        if want == 0 {
            return 0;
        }
        if self.rate == 1.0 {
            let p = pos.round().max(0.0) as i64;
            let avail = view.frames() as i64;
            let n = (avail - p).clamp(0, want as i64) as usize;
            view.copy_frames(p, n, &mut out[..n * c]);
            *pos = (p + n as i64) as f64;
            return n;
        }
        let mut produced = 0;
        loop {
            if self.pending_off < self.pending.len() {
                let n_samp = (self.pending.len() - self.pending_off).min((want - produced) * c);
                out[produced * c..produced * c + n_samp]
                    .copy_from_slice(&self.pending[self.pending_off..self.pending_off + n_samp]);
                self.pending_off += n_samp;
                produced += n_samp / c;
                if let Some(d) = &mut self.drain {
                    d.delivered += n_samp / c;
                    let f = if d.total > 0 { d.delivered as f64 / d.total as f64 } else { 1.0 };
                    *pos = d.start_pos + (d.end_pos - d.start_pos) * f.min(1.0);
                }
                if self.pending_off >= self.pending.len() {
                    self.pending.clear();
                    self.pending_off = 0;
                    if self.drain.take().is_some() {
                        self.phase = Phase::Done;
                    }
                }
                if produced == want {
                    break;
                }
                continue;
            }
            if !self.produce_hop(view, pos, eos) {
                break;
            }
        }
        produced
    }

    /// Produces the next hop (or the drained tail) into `pending`. False when nothing could be produced.
    fn produce_hop(&mut self, view: &SrcView<'_>, pos: &mut f64, eos: bool) -> bool {
        let c = self.channels as usize;
        let n = self.n as i64;
        let hs = self.hs;
        let avail = view.frames() as i64;
        let ha = self.hs as f64 * self.rate;
        match self.phase {
            Phase::Done | Phase::Draining => false,
            Phase::Fresh => {
                let p = pos.round().max(0.0) as i64;
                if avail >= p + n {
                    // Prime: a virtual previous frame whose second half reproduces [p, p + hs) together with frame
                    // 0's first half (falling + rising Hann halves sum to 1).
                    self.acc.iter_mut().for_each(|x| *x = 0.0);
                    for i in 0..hs {
                        let w = self.window[hs + i];
                        if let Some(f) = view.frame(p + i as i64) {
                            for (k, s) in f.iter().enumerate() {
                                self.acc[i * c + k] = s * w;
                            }
                        }
                    }
                    self.place(view, p);
                    self.emit_hop();
                    self.prev_p = p;
                    *pos += ha;
                    self.phase = Phase::Running;
                    true
                } else if eos {
                    // Too short for one frame: the rest at the original speed.
                    let len = (avail - p).max(0) as usize;
                    self.start_drain_plain(view, p, len, pos, avail);
                    len > 0
                } else {
                    false
                }
            }
            Phase::Running => {
                let nominal = pos.round() as i64;
                let full = avail >= nominal + self.delta as i64 + n && avail >= self.prev_p + hs as i64 + n;
                if full || (eos && nominal < avail) {
                    // At the end of a finite source, frames may run past it (reading silence) as long as they start
                    // inside it, so the end is time-scaled like the rest.
                    let lo = (nominal - self.delta as i64).max(0);
                    let hi =
                        if full { nominal + self.delta as i64 } else { (nominal + self.delta as i64).min(avail - 1) }
                            .max(lo);
                    let best = self.search(view, nominal.clamp(lo, hi), lo, hi);
                    self.place(view, best);
                    self.emit_hop();
                    self.prev_p = best;
                    *pos += ha;
                    true
                } else if eos {
                    // Drain: the last frame's second half, completed by the source that naturally follows it.
                    let from = self.prev_p + hs as i64;
                    self.pending.clear();
                    self.pending_off = 0;
                    self.pending.resize(hs * c, 0.0);
                    for i in 0..hs {
                        let w = self.window[i];
                        let f = view.frame(from + i as i64).filter(|_| from + (i as i64) < avail);
                        for k in 0..c {
                            let s = f.map(|f| f[k]).unwrap_or(0.0);
                            self.pending[i * c + k] = self.acc[i * c + k] + s * w;
                        }
                    }
                    let end = avail as f64;
                    self.drain = Some(Drain { start_pos: pos.min(end), end_pos: end, total: hs, delivered: 0 });
                    self.phase = Phase::Draining;
                    true
                } else {
                    false
                }
            }
        }
    }

    fn start_drain_plain(&mut self, view: &SrcView<'_>, p: i64, len: usize, pos: &mut f64, avail: i64) {
        let c = self.channels as usize;
        self.pending.clear();
        self.pending_off = 0;
        self.pending.resize(len * c, 0.0);
        view.copy_frames(p, len, &mut self.pending);
        if len == 0 {
            self.phase = Phase::Done;
            *pos = avail.max(p) as f64;
            return;
        }
        self.drain = Some(Drain { start_pos: *pos, end_pos: avail as f64, total: len, delivered: 0 });
        self.phase = Phase::Draining;
    }

    /// Adds the windowed frame starting at source frame `p` into the accumulator.
    fn place(&mut self, view: &SrcView<'_>, p: i64) {
        let c = self.channels as usize;
        for i in 0..self.n {
            let w = self.window[i];
            if let Some(f) = view.frame(p + i as i64) {
                for (k, s) in f.iter().enumerate() {
                    self.acc[i * c + k] += s * w;
                }
            }
        }
    }

    /// Moves the completed first hop of the accumulator to `pending` and shifts the accumulator.
    fn emit_hop(&mut self) {
        let c = self.channels as usize;
        let h = self.hs * c;
        self.pending.clear();
        self.pending_off = 0;
        self.pending.extend_from_slice(&self.acc[..h]);
        self.acc.copy_within(h.., 0);
        let len = self.acc.len();
        self.acc[len - h..].iter_mut().for_each(|x| *x = 0.0);
    }

    /// The candidate start in `[lo, hi]` whose content best continues the previous frame.
    fn search(&mut self, view: &SrcView<'_>, nominal: i64, lo: i64, hi: i64) -> i64 {
        let n = self.n;
        let tgt_start = self.prev_p + self.hs as i64;
        self.tgt.clear();
        self.tgt.extend((0..n as i64).map(|i| view.mono(tgt_start + i)));
        let reg_len = (hi - lo) as usize + n;
        self.reg.clear();
        self.reg.extend((0..reg_len as i64).map(|i| view.mono(lo + i)));
        let score = |reg: &[f32], tgt: &[f32], off: usize, step: usize| -> f64 {
            let mut dot = 0f64;
            let mut energy = 0f64;
            let mut i = 0;
            while i < n {
                let x = reg[off + i] as f64;
                dot += x * tgt[i] as f64;
                energy += x * x;
                i += step;
            }
            dot / (energy + 1e-9).sqrt()
        };
        let (reg, tgt) = (&self.reg, &self.tgt);
        let mut best = nominal;
        let mut best_score = score(reg, tgt, (nominal - lo) as usize, 2);
        let step = self.coarse;
        let mut p = lo;
        while p <= hi {
            let s = score(reg, tgt, (p - lo) as usize, 2);
            if s > best_score {
                best_score = s;
                best = p;
            }
            p += step as i64;
        }
        // Fine search around the coarse winner at full resolution.
        let mut fine_best = best;
        let mut fine_score = score(reg, tgt, (best - lo) as usize, 1);
        for q in (best - step as i64).max(lo)..=(best + step as i64).min(hi) {
            let s = score(reg, tgt, (q - lo) as usize, 1);
            if s > fine_score {
                fine_score = s;
                fine_best = q;
            }
        }
        fine_best
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    fn tone(sr: u32, freq: f64, frames: usize) -> Vec<f32> {
        (0..frames).map(|i| ((2.0 * PI * freq * i as f64 / sr as f64).sin() * 0.5) as f32).collect()
    }

    fn render_all(w: &mut Wsola, src: &[f32], block: usize) -> (Vec<f32>, f64) {
        let mut out = Vec::new();
        let mut pos = 0.0;
        let mut buf = vec![0f32; block * w.channels() as usize];
        loop {
            let n = w.render(src, &mut pos, &mut buf);
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n * w.channels() as usize]);
        }
        (out, pos)
    }

    fn zero_cross_freq(x: &[f32], sr: u32) -> f64 {
        let mut crossings = Vec::new();
        for i in 1..x.len() {
            if x[i - 1] < 0.0 && x[i] >= 0.0 {
                let t = (i - 1) as f64 + (-x[i - 1] as f64) / ((x[i] - x[i - 1]) as f64);
                crossings.push(t);
            }
        }
        let n = crossings.len();
        assert!(n >= 3);
        (n - 1) as f64 / ((crossings[n - 1] - crossings[0]) / sr as f64)
    }

    #[test]
    fn rate_one_is_bit_exact() {
        let src = tone(22_050, 220.0, 10_000);
        let mut w = Wsola::new(22_050, 1);
        w.set_rate(1.0);
        let (out, pos) = render_all(&mut w, &src, 333);
        assert_eq!(out, src);
        assert_eq!(pos, 10_000.0);
        // Stereo too.
        let stereo: Vec<f32> = src.iter().flat_map(|s| [*s, -*s]).collect();
        let mut w = Wsola::new(22_050, 2);
        let (out, _) = render_all(&mut w, &stereo, 100);
        assert_eq!(out, stereo);
    }

    #[test]
    fn rate_is_clamped() {
        let mut w = Wsola::new(22_050, 1);
        w.set_rate(10.0);
        assert_eq!(w.rate(), 3.0);
        w.set_rate(0.1);
        assert_eq!(w.rate(), 0.5);
        w.set_rate(f64::NAN);
        assert_eq!(w.rate(), 1.0);
    }

    #[test]
    fn output_length_follows_the_rate() {
        // The un-stretched end (about one hop) is a constant ~10–20 ms, so use a source of realistic chunk length.
        let sr = 22_050;
        let src = tone(sr, 220.0, sr as usize * 8);
        for &rate in &[0.5, 0.75, 1.25, 1.5, 2.0, 3.0] {
            let mut w = Wsola::new(sr, 1);
            w.set_rate(rate);
            let (out, pos) = render_all(&mut w, &src, 220);
            let expected = src.len() as f64 / rate;
            let err = (out.len() as f64 - expected).abs() / expected;
            assert!(err < 0.01, "rate {rate}: {} vs {expected} ({:.3} %)", out.len(), err * 100.0);
            // The nominal position reaches the end of the source.
            assert!((pos - src.len() as f64).abs() < 1.0, "rate {rate}: pos {pos}");
            assert!(w.is_done());
        }
    }

    #[test]
    fn pitch_is_preserved() {
        for &(sr, rate) in &[(22_050u32, 0.5), (22_050, 2.0), (44_100, 0.5), (44_100, 2.0), (44_100, 1.5)] {
            let src = tone(sr, 220.0, sr as usize * 3);
            let mut w = Wsola::new(sr, 1);
            w.set_rate(rate);
            let (out, _) = render_all(&mut w, &src, 441);
            let mid = &out[out.len() / 5..out.len() * 4 / 5];
            let f = zero_cross_freq(mid, sr);
            assert!((f - 220.0).abs() / 220.0 < 0.01, "sr {sr} rate {rate}: {f} Hz");
            // Level stays (Hann overlap-add of aligned frames).
            let peak = mid.iter().fold(0f32, |m, x| m.max(x.abs()));
            assert!((0.45..=0.55).contains(&peak), "sr {sr} rate {rate}: peak {peak}");
        }
    }

    #[test]
    fn first_hop_reproduces_the_source() {
        // After a reset (seek) the output starts exactly with the source: no fade-in.
        let src = tone(22_050, 330.0, 5000);
        let mut w = Wsola::new(22_050, 1);
        w.set_rate(1.5);
        let mut pos = 1000.0;
        let mut out = vec![0f32; 100];
        let n = w.render(&src, &mut pos, &mut out);
        assert_eq!(n, 100);
        for i in 0..100 {
            assert!((out[i] - src[1000 + i]).abs() < 1e-5, "{i}");
        }
    }

    #[test]
    fn two_segment_view_streams_across_the_join() {
        // Rendering a + b through the view equals rendering the concatenation.
        let sr = 22_050;
        let whole = tone(sr, 200.0, 20_000);
        let (a, b) = whole.split_at(7_777);
        let mut w1 = Wsola::new(sr, 1);
        w1.set_rate(1.25);
        let (expect, _) = render_all(&mut w1, &whole, 256);

        let mut w2 = Wsola::new(sr, 1);
        w2.set_rate(1.25);
        let mut out = Vec::new();
        let mut buf = vec![0f32; 256];
        let mut pos = 0.0;
        // First only `a` is known (not the end): rendering stalls before the join.
        loop {
            let n = w2.render_view(&SrcView::single(a, 1), &mut pos, &mut buf, false);
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        assert!(pos < a.len() as f64);
        // Then `b` arrives.
        loop {
            let view = SrcView { a, b, channels: 1 };
            let n = w2.render_view(&view, &mut pos, &mut buf, true);
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        assert_eq!(out.len(), expect.len());
        for (i, (x, y)) in out.iter().zip(expect.iter()).enumerate() {
            assert!((x - y).abs() < 1e-6, "sample {i}");
        }
    }

    #[test]
    fn rebase_after_dropping_the_first_segment() {
        let sr = 22_050;
        let whole = tone(sr, 250.0, 12_000);
        let (a, b) = whole.split_at(4_000);
        let mut w1 = Wsola::new(sr, 1);
        w1.set_rate(0.75);
        let (expect, _) = render_all(&mut w1, &whole, 200);

        let mut w2 = Wsola::new(sr, 1);
        w2.set_rate(0.75);
        let mut out = Vec::new();
        let mut buf = vec![0f32; 200];
        let mut pos = 0.0;
        let mut dropped = false;
        loop {
            let view = if dropped { SrcView::single(b, 1) } else { SrcView { a, b, channels: 1 } };
            let n = w2.render_view(&view, &mut pos, &mut buf, true);
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
            if !dropped && w2.earliest_needed(pos) >= a.len() as i64 {
                w2.rebase(a.len());
                pos -= a.len() as f64;
                dropped = true;
            }
        }
        assert!(dropped);
        assert_eq!(out.len(), expect.len());
        for (x, y) in out.iter().zip(expect.iter()) {
            assert!((x - y).abs() < 1e-6);
        }
    }

    #[test]
    fn short_and_empty_sources() {
        let mut w = Wsola::new(22_050, 1);
        w.set_rate(2.0);
        let (out, pos) = render_all(&mut w, &[], 64);
        assert!(out.is_empty());
        assert_eq!(pos, 0.0);
        // Shorter than one frame: played as is.
        let src = tone(22_050, 220.0, 100);
        let mut w = Wsola::new(22_050, 1);
        w.set_rate(2.0);
        let (out, pos) = render_all(&mut w, &src, 64);
        assert_eq!(out, src);
        assert_eq!(pos, 100.0);
        // Silence never panics and keeps its length roughly.
        let silence = vec![0f32; 22_050];
        let mut w = Wsola::new(22_050, 1);
        w.set_rate(0.5);
        let (out, _) = render_all(&mut w, &silence, 512);
        assert!((out.len() as f64 - 44_100.0).abs() / 44_100.0 < 0.01);
        assert!(out.iter().all(|x| *x == 0.0));
    }

    #[test]
    fn set_rate_across_one_resets() {
        let src = tone(22_050, 220.0, 5000);
        let mut w = Wsola::new(22_050, 1);
        w.set_rate(2.0);
        let mut pos = 0.0;
        let mut out = vec![0f32; 1000];
        let _ = w.render(&src, &mut pos, &mut out);
        w.set_rate(1.0);
        let mut pos = 0.0;
        let n = w.render(&src, &mut pos, &mut out);
        assert_eq!(&out[..n], &src[..n]);
    }
}
