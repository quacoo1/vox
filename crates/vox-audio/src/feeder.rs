//! The feed pipeline (ARCHITECTURE.md §5.1, §5.3): decoded chunk PCM → WSOLA (current rate) → resampler (device rate)
//! → channel map → ≈10 ms blocks tagged with `(generation, chunk, source frame)` → the output ring, kept ≈150 ms
//! ahead of the device.
//!
//! Gapless joins: when the current chunk runs out and the next playable chunk is decoded (same format), it is
//! attached as the second segment of the WSOLA view, so the stretch state and the resampler carry straight over the
//! join; the first segment is dropped once nothing reads it any more. Failed (undecodable) chunks are skipped. When
//! the next chunk is not there yet, the feeder holds the last few tens of milliseconds of the current chunk while the
//! ring still has ≥ 40 ms queued, then drains it (the end of the chunk is played in full) and later continues with
//! the next chunk in the same generation as soon as it is decoded.

use std::sync::Arc;

use crate::decode::DecodedAudio;
use crate::output::{BlockTag, Sink};
use crate::resample::Resampler;
use crate::stretch::{SrcView, Wsola};

/// Audio the feeder can ask for.
pub(crate) enum ChunkAudio {
    Ready(Arc<DecodedAudio>),
    /// Delivered, decode in progress.
    Pending,
    /// Undecodable: skipped.
    Failed,
    /// Not delivered (yet): `more` = synthesis may still deliver it.
    Missing {
        more: bool,
    },
}

/// The session's chunks as seen by the feeder (asking for audio requests its decode).
pub(crate) trait ChunkSource {
    fn audio(&mut self, index: usize) -> ChunkAudio;
}

enum Next {
    Ready(usize, Arc<DecodedAudio>),
    Wait,
    End,
}

const TARGET_MS: usize = 150;
const LOW_WATER_MS: usize = 40;

pub(crate) struct Feeder {
    active: bool,
    /// First chunk fed in this generation.
    first: usize,
    /// Chunk of segment `a`.
    cur: usize,
    /// Where to start in `cur` (seconds) until its audio is loaded.
    start_sec: f64,
    a: Option<Arc<DecodedAudio>>,
    b: Option<(usize, Arc<DecodedAudio>)>,
    /// Nominal source frame relative to the start of `a`.
    pos: f64,
    rate: f64,
    wsola: Option<Wsola>,
    resampler: Option<Resampler>,
    /// (source rate, source channels, device rate, device channels) of the pipeline.
    format: Option<(u32, u16, u32, u16)>,
    /// `cur` was rendered to its end; waiting for a following chunk.
    drained: bool,
    src_buf: Vec<f32>,
    rs_buf: Vec<f32>,
    out_buf: Vec<f32>,
    stash: Option<(BlockTag, Vec<f32>)>,
    generation: u64,
}

impl Feeder {
    pub(crate) fn new() -> Self {
        Self {
            active: false,
            first: 0,
            cur: 0,
            start_sec: 0.0,
            a: None,
            b: None,
            pos: 0.0,
            rate: 1.0,
            wsola: None,
            resampler: None,
            format: None,
            drained: false,
            src_buf: Vec::new(),
            rs_buf: Vec::new(),
            out_buf: Vec::new(),
            stash: None,
            generation: 0,
        }
    }

    /// Starts feeding chunk `chunk` at `start_sec` seconds into it, in output generation `generation` (the caller
    /// flushed the output).
    pub(crate) fn start(&mut self, generation: u64, chunk: usize, start_sec: f64, rate: f64) {
        self.active = true;
        self.generation = generation;
        self.first = chunk;
        self.cur = chunk;
        self.start_sec = if start_sec.is_finite() { start_sec.max(0.0) } else { 0.0 };
        self.a = None;
        self.b = None;
        self.pos = 0.0;
        self.rate = rate;
        self.drained = false;
        self.stash = None;
        if let Some(w) = self.wsola.as_mut() {
            w.set_rate(rate);
            w.reset();
        }
        if let Some(r) = self.resampler.as_mut() {
            r.reset();
        }
    }

    pub(crate) fn stop(&mut self) {
        self.active = false;
        self.a = None;
        self.b = None;
        self.stash = None;
        self.drained = false;
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active
    }

    /// Last chunk this generation has fed (or is feeding).
    fn last_fed(&self) -> usize {
        self.b.as_ref().map(|b| b.0).unwrap_or(self.cur)
    }

    /// The current generation has fed chunk `i` (it is streaming or streamed through it).
    pub(crate) fn covers(&self, i: usize) -> bool {
        self.active && self.first <= i && i <= self.last_fed()
    }

    /// Speech is flowing: the current chunk's audio is loaded and not yet rendered to its end (an empty ring now is
    /// an under-run, not the end of the audio).
    pub(crate) fn is_feeding(&self) -> bool {
        self.active && self.a.is_some() && !self.drained
    }

    /// The chunk the feeder waits for (its first audio is not loaded yet).
    pub(crate) fn waiting_for(&self) -> Option<usize> {
        (self.active && self.a.is_none()).then_some(self.cur)
    }

    fn setup(&mut self, audio: &DecodedAudio, out_sr: u32, out_ch: u16) {
        let format = (audio.sample_rate, audio.channels, out_sr, out_ch);
        if self.format != Some(format) || self.wsola.is_none() || self.resampler.is_none() {
            let mut w = Wsola::new(audio.sample_rate, audio.channels);
            w.set_rate(self.rate);
            self.wsola = Some(w);
            self.resampler = Some(Resampler::new(audio.sample_rate, out_sr, audio.channels));
            self.format = Some(format);
        } else {
            if let Some(w) = self.wsola.as_mut() {
                w.set_rate(self.rate);
                w.reset();
            }
            if let Some(r) = self.resampler.as_mut() {
                r.reset();
            }
        }
    }

    fn next_after(&self, after: usize, src: &mut dyn ChunkSource) -> Next {
        let mut i = after + 1;
        loop {
            match src.audio(i) {
                ChunkAudio::Ready(x) => return Next::Ready(i, x),
                ChunkAudio::Failed => i += 1,
                ChunkAudio::Pending | ChunkAudio::Missing { more: true } => return Next::Wait,
                ChunkAudio::Missing { more: false } => return Next::End,
            }
        }
    }

    fn same_format(&self, x: &DecodedAudio) -> bool {
        matches!(self.format, Some((sr, ch, _, _)) if sr == x.sample_rate && ch == x.channels)
    }

    /// Maps `rs_buf` (source channels) into `out_buf` (device channels). Mono goes to the first two channels.
    fn map_channels(&mut self, src_ch: usize, out_ch: usize) {
        self.out_buf.clear();
        let frames = self.rs_buf.len() / src_ch.max(1);
        if src_ch == out_ch {
            self.out_buf.extend_from_slice(&self.rs_buf[..frames * src_ch]);
            return;
        }
        self.out_buf.reserve(frames * out_ch);
        for f in 0..frames {
            let frame = &self.rs_buf[f * src_ch..(f + 1) * src_ch];
            if out_ch == 1 {
                self.out_buf.push(frame.iter().sum::<f32>() / src_ch as f32);
            } else if src_ch == 1 {
                self.out_buf.extend((0..out_ch).map(|c| if c < 2 { frame[0] } else { 0.0 }));
            } else {
                self.out_buf.extend((0..out_ch).map(|c| frame.get(c).copied().unwrap_or(0.0)));
            }
        }
    }

    fn push_out(&mut self, sink: &mut dyn Sink, tag: BlockTag) -> bool {
        if self.out_buf.is_empty() {
            return true;
        }
        if sink.push(tag, &self.out_buf) {
            true
        } else {
            self.stash = Some((tag, std::mem::take(&mut self.out_buf)));
            false
        }
    }

    /// Produces blocks until ≈150 ms are queued or the source runs dry.
    pub(crate) fn pump(&mut self, sink: &mut dyn Sink, src: &mut dyn ChunkSource) {
        if !self.active {
            return;
        }
        let out_sr = sink.sample_rate();
        let out_ch = sink.channels();
        if out_sr == 0 || out_ch == 0 {
            return;
        }
        let target = out_sr as usize * TARGET_MS / 1000;
        let low_water = out_sr as usize * LOW_WATER_MS / 1000;
        let block = (out_sr as usize / 100).max(1);
        if let Some((tag, samples)) = self.stash.take() {
            if !sink.push(tag, &samples) {
                self.stash = Some((tag, samples));
                return;
            }
        }
        for _ in 0..64 {
            if sink.queued_frames() >= target || sink.free_frames() < 2 * block + 64 {
                break;
            }
            if self.a.is_none() {
                match src.audio(self.cur) {
                    ChunkAudio::Ready(x) => {
                        self.setup(&x, out_sr, out_ch);
                        self.pos = (self.start_sec * x.sample_rate as f64).round().clamp(0.0, x.frames() as f64);
                        self.a = Some(x);
                    }
                    // Not decoded yet, or undecodable (the player advances past it).
                    _ => break,
                }
            }
            if self.drained {
                match self.next_after(self.cur, src) {
                    Next::Ready(i, x) => {
                        self.setup(&x, out_sr, out_ch);
                        self.cur = i;
                        self.a = Some(x);
                        self.b = None;
                        self.pos = 0.0;
                        self.drained = false;
                    }
                    Next::Wait | Next::End => break,
                }
            }
            let Some(a) = self.a.clone() else { break };
            let (src_sr, src_ch) = (a.sample_rate, a.channels as usize);
            let a_frames = a.frames();
            let b = self.b.clone();
            let n_src = ((block as f64 * src_sr as f64 / out_sr as f64).ceil() as usize).max(1);
            self.src_buf.resize(n_src * src_ch, 0.0);
            if self.wsola.is_none() {
                break;
            }
            let view =
                SrcView { a: &a.samples, b: b.as_ref().map(|(_, x)| &x.samples[..]).unwrap_or(&[]), channels: src_ch };
            let mut produced = match self.wsola.as_mut() {
                Some(w) => w.render_view(&view, &mut self.pos, &mut self.src_buf, false),
                None => break,
            };
            if produced == 0 {
                let eos;
                if let Some((bi, bx)) = &b {
                    let needed = self.wsola.as_ref().map(|w| w.earliest_needed(self.pos)).unwrap_or(i64::MAX);
                    if needed >= a_frames as i64 && self.pos >= a_frames as f64 {
                        // Segment `a` is done: `b` becomes the current chunk.
                        if let Some(w) = self.wsola.as_mut() {
                            w.rebase(a_frames);
                        }
                        self.pos -= a_frames as f64;
                        self.cur = *bi;
                        self.a = Some(bx.clone());
                        self.b = None;
                        continue;
                    }
                    eos = true;
                } else {
                    match self.next_after(self.cur, src) {
                        Next::Ready(i, x) if self.same_format(&x) => {
                            self.b = Some((i, x));
                            continue;
                        }
                        Next::Ready(..) | Next::End => eos = true,
                        Next::Wait => {
                            if sink.queued_frames() > low_water {
                                break;
                            }
                            eos = true;
                        }
                    }
                }
                if eos {
                    produced = match self.wsola.as_mut() {
                        Some(w) => w.render_view(&view, &mut self.pos, &mut self.src_buf, true),
                        None => 0,
                    };
                    if produced == 0 {
                        // Fully rendered: flush the resampler's tail and mark the chunk drained.
                        self.rs_buf.clear();
                        if let Some(r) = self.resampler.as_mut() {
                            r.flush(&mut self.rs_buf);
                        }
                        let (chunk, frame) = match &b {
                            Some((bi, bx)) => (*bi, bx.frames() as f64),
                            None => (self.cur, a_frames as f64),
                        };
                        if let Some((bi, bx)) = b {
                            self.cur = bi;
                            self.a = Some(bx);
                            self.b = None;
                        }
                        self.drained = true;
                        self.map_channels(src_ch, out_ch as usize);
                        if self.out_buf.is_empty() {
                            // Always leave a block carrying the end tag, so the end is reported as played.
                            self.out_buf.resize(out_ch as usize, 0.0);
                        }
                        let tag =
                            BlockTag { generation: self.generation, chunk: chunk as u32, source_frame_end: frame };
                        if !self.push_out(sink, tag) {
                            break;
                        }
                        continue;
                    }
                }
            }
            self.rs_buf.clear();
            if let Some(r) = self.resampler.as_mut() {
                r.process(&self.src_buf[..produced * src_ch], &mut self.rs_buf);
            }
            self.map_channels(src_ch, out_ch as usize);
            let (chunk, frame) = match &b {
                Some((bi, bx)) if self.pos >= a_frames as f64 => {
                    (*bi, (self.pos - a_frames as f64).min(bx.frames() as f64))
                }
                _ => (self.cur, self.pos.clamp(0.0, a_frames as f64)),
            };
            let tag = BlockTag { generation: self.generation, chunk: chunk as u32, source_frame_end: frame };
            if !self.push_out(sink, tag) {
                break;
            }
            // Drop segment `a` once nothing reads it any more.
            if let (Some((bi, bx)), Some(w)) = (&b, self.wsola.as_mut()) {
                if self.pos >= a_frames as f64 && w.earliest_needed(self.pos) >= a_frames as i64 {
                    w.rebase(a_frames);
                    self.pos -= a_frames as f64;
                    self.cur = *bi;
                    self.a = Some(bx.clone());
                    self.b = None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::SimOutput;
    use std::f64::consts::PI;

    struct Chunks {
        audio: Vec<Option<Arc<DecodedAudio>>>, // None = failed
        pending: Vec<bool>,
        more: bool,
    }

    impl ChunkSource for Chunks {
        fn audio(&mut self, i: usize) -> ChunkAudio {
            match self.audio.get(i) {
                None => ChunkAudio::Missing { more: self.more },
                Some(_) if self.pending.get(i).copied().unwrap_or(false) => ChunkAudio::Pending,
                Some(Some(x)) => ChunkAudio::Ready(x.clone()),
                Some(None) => ChunkAudio::Failed,
            }
        }
    }

    fn tone(sr: u32, freq: f64, sec: f64, phase0: f64) -> Arc<DecodedAudio> {
        let frames = (sr as f64 * sec).round() as usize;
        Arc::new(DecodedAudio {
            sample_rate: sr,
            channels: 1,
            samples: (0..frames)
                .map(|i| ((2.0 * PI * freq * i as f64 / sr as f64 + phase0).sin() * 0.5) as f32)
                .collect(),
        })
    }

    /// Runs the feeder against a simulated device for `ms` milliseconds; returns the captured blocks.
    fn run(feeder: &mut Feeder, sink: &mut SimOutput, src: &mut Chunks, from_ms: f64, ms: f64) -> f64 {
        let mut now = from_ms;
        while now < from_ms + ms {
            sink.update(now);
            feeder.pump(sink, src);
            now += 10.0;
        }
        now
    }

    fn captured(sink: &SimOutput) -> Vec<(BlockTag, Vec<f32>)> {
        sink.capture.clone().unwrap_or_default()
    }

    #[test]
    fn gapless_join_has_no_silent_gap() {
        // Two chunks of one continuous 220 Hz tone, split mid-signal: the rendered stream must not contain any
        // silent run longer than 1 ms at the join (at rate 1 and with WSOLA at 1.5×).
        for &rate in &[1.0, 1.5] {
            let sr = 22_050;
            let whole = tone(sr, 220.0, 1.0, 0.0);
            let split = 9_000;
            let a = Arc::new(DecodedAudio { sample_rate: sr, channels: 1, samples: whole.samples[..split].to_vec() });
            let b = Arc::new(DecodedAudio { sample_rate: sr, channels: 1, samples: whole.samples[split..].to_vec() });
            let mut src = Chunks { audio: vec![Some(a), Some(b)], pending: vec![false, false], more: false };
            let mut sink = SimOutput::new(48_000, 2);
            sink.capture = Some(Vec::new());
            sink.resume();
            let mut f = Feeder::new();
            let gen = 1;
            sink.start_generation(gen);
            f.start(gen, 0, 0.0, rate);
            run(&mut f, &mut sink, &mut src, 0.0, 1500.0);
            let blocks = captured(&sink);
            assert!(blocks.iter().any(|(t, _)| t.chunk == 1), "rate {rate}: the second chunk was fed");
            let left: Vec<f32> = blocks.iter().flat_map(|(_, s)| s.iter().step_by(2).copied()).collect();
            // Expected output length ≈ 1 s / rate at 48 kHz.
            let expected = 48_000.0 / rate;
            assert!(
                (left.len() as f64 - expected).abs() / expected < 0.03,
                "rate {rate}: {} vs {expected}",
                left.len()
            );
            // Longest run of near-silence inside the signal (ignore the first 5 ms and the last 15 ms, where the last
            // WSOLA frame fades out at the end of the stream).
            let inner = &left[240..left.len() - 720];
            let mut run_len = 0;
            let mut longest = (0, 0);
            for (i, x) in inner.iter().enumerate() {
                if x.abs() < 1e-3 {
                    run_len += 1;
                    if run_len > longest.0 {
                        longest = (run_len, i);
                    }
                } else {
                    run_len = 0;
                }
            }
            let longest = longest.0;
            // A 220 Hz sine at 0.5 stays below 1e-3 for ≈ 1.4 µs around zero crossings only: < 1 ms ≪ 48 samples.
            assert!(longest < 48, "rate {rate}: silent run of {longest} samples");
            // Tags are monotonic and end exactly at the end of the last chunk.
            let last = blocks.last().map(|(t, _)| *t).unwrap_or_default();
            assert_eq!(last.chunk, 1);
            assert_eq!(last.source_frame_end, (whole.samples.len() - split) as f64);
            let mut prev = (0u32, 0f64);
            for (t, _) in &blocks {
                assert!((t.chunk, t.source_frame_end) >= prev, "rate {rate}: tags go backwards");
                prev = (t.chunk, t.source_frame_end);
            }
        }
    }

    #[test]
    fn skips_failed_chunks_and_waits_for_pending_ones() {
        let sr = 22_050;
        let mut src = Chunks {
            audio: vec![Some(tone(sr, 200.0, 0.3, 0.0)), None, Some(tone(sr, 300.0, 0.3, 0.0))],
            pending: vec![false, false, true],
            more: false,
        };
        let mut sink = SimOutput::new(48_000, 1);
        sink.capture = Some(Vec::new());
        sink.resume();
        let mut f = Feeder::new();
        let gen = 1;
        sink.start_generation(gen);
        f.start(gen, 0, 0.0, 1.0);
        let now = run(&mut f, &mut sink, &mut src, 0.0, 600.0);
        // Chunk 2 is still decoding: chunk 0 was drained to its end, nothing of chunk 2 yet.
        let blocks = captured(&sink);
        assert!(blocks.iter().all(|(t, _)| t.chunk == 0));
        assert_eq!(blocks.last().map(|(t, _)| t.source_frame_end), Some(6615.0));
        assert!(f.covers(0) && !f.covers(2));
        // Decoded: continues with chunk 2 (chunk 1 failed and is skipped) in the same generation.
        src.pending[2] = false;
        run(&mut f, &mut sink, &mut src, now, 600.0);
        let blocks = captured(&sink);
        assert!(blocks.iter().any(|(t, _)| t.chunk == 2));
        assert!(!blocks.iter().any(|(t, _)| t.chunk == 1));
        assert!(blocks.iter().all(|(t, _)| t.generation == gen));
        assert!(f.covers(2));
    }

    #[test]
    fn starts_at_an_offset_and_waits_for_the_first_decode() {
        let sr = 22_050;
        let mut src = Chunks { audio: vec![Some(tone(sr, 200.0, 1.0, 0.0))], pending: vec![true], more: true };
        let mut sink = SimOutput::new(22_050, 1);
        sink.capture = Some(Vec::new());
        sink.resume();
        let mut f = Feeder::new();
        let gen = 1;
        sink.start_generation(gen);
        f.start(gen, 0, 0.5, 1.0);
        assert_eq!(f.waiting_for(), Some(0));
        run(&mut f, &mut sink, &mut src, 0.0, 50.0);
        assert!(captured(&sink).is_empty());
        src.pending[0] = false;
        run(&mut f, &mut sink, &mut src, 50.0, 20.0);
        assert_eq!(f.waiting_for(), None);
        let blocks = captured(&sink);
        let first = &blocks[0];
        // Same rate, rate 1: the first samples are the source at 0.5 s.
        let src0 = match src.audio(0) {
            ChunkAudio::Ready(x) => x,
            _ => unreachable!(),
        };
        assert_eq!(first.1[..10], src0.samples[11_025..11_035]);
        assert!(first.0.source_frame_end > 11_025.0);
    }

    #[test]
    fn stop_and_covers() {
        let mut f = Feeder::new();
        assert!(!f.is_active());
        f.start(3, 2, 0.0, 1.0);
        assert!(f.is_active());
        assert!(f.covers(2) && !f.covers(1) && !f.covers(3));
        f.stop();
        assert!(!f.covers(2));
    }
}
