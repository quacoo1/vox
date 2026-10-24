//! The WASAPI output (cpal, default device, shared mode, f32). The player thread pushes small tagged blocks (~10 ms)
//! into an `rtrb` ring; the real-time callback pops them, applies volume (atomic, ramped), drops blocks of an old
//! generation (pause/seek/stop flush instantly), mixes in the warmer's keep-alive signal (underneath speech too, as
//! the original's looping signal did) while the warmer is on, and publishes the tag of the last block played
//! (position), how far it has got into the block in progress (the "handed" position) and the total rendered frames
//! (the warmer's audio clock). Device loss / default-device change → the player reopens on the new default within
//! ~1 s (poll the default device every 2 s while a session or hold is active), keeping position.
//!
//! Real-time rules for the callback: no locks, no allocation, no logging; it is wrapped in `catch_unwind` (a panic
//! outputs silence and marks the stream lost). Three lock-free SPSC rings connect it to the player thread: interleaved
//! samples and block descriptors (player → callback) and finished-block reports with their device playback instants
//! (callback → player). A flush only bumps an atomic generation; the callback then fades the interrupted audio out
//! over the full 2 ms — through the rest of the block in progress and on into the stale blocks queued behind it, or
//! as a ramp of the last frame when nothing follows — discards the rest of the old generation and fades the new one
//! in over 2 ms, so pause/seek/rate changes never click. An empty ring while speech was playing (an under-run, or the
//! end of a read) also ramps the last frame down instead of cutting; under-runs while the player says it is feeding
//! are counted (with cpal's xruns) for the player's log. Volume changes ramp over 10 ms (no zipper noise).
//!
//! `SimOutput` is the same contract on a simulated clock (no device: `PlayerConfig::null_output`, or no output
//! device at all — ARCHITECTURE.md §5.8). Tests also get `RingSim`: the real rings and callback driven by a simulated
//! device clock, so the engine's scenarios exercise the exact real-time code.

use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{ErrorKind, FromSample, OutputCallbackInfo, SampleFormat, SizedSample};

use crate::warmer::keep_alive_sample;

/// A block's position tag: which chunk and source frame it ends at. When the callback pops a block it records the
/// device playback instant of the block (`cpal::OutputCallbackInfo::timestamp().playback`, on the stream's clock,
/// which the player thread reads with `Stream::now`); the **played** tag is the last block whose playback instant ≤ now (ARCHITECTURE.md §5.4), so
/// position, highlighting and the listening clock follow what is heard, including Bluetooth latency.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BlockTag {
    pub generation: u64,
    pub chunk: u32,
    pub source_frame_end: f64,
}

#[derive(Debug, thiserror::Error)]
pub enum OutputError {
    #[error("no audio output device")]
    NoDevice,
    #[error("audio output failed: {0}")]
    Stream(String),
}

/// Ring capacity and the fade applied around a flush.
const RING_MS: usize = 250;
const TAG_SLOTS: usize = 128;
const HISTORY_SLOTS: usize = 512;
const FADE_MS: usize = 2;
/// A full-scale volume change ramps over this long.
const VOLUME_RAMP_MS: usize = 10;

/// `Ctrl::handed` packs a block sequence number (high bits) and the frames of it handed to the device (low bits).
const HANDED_FRAME_BITS: u32 = 24;
const HANDED_FRAME_MASK: u64 = (1 << HANDED_FRAME_BITS) - 1;

fn pack_handed(seq: u64, frames: usize) -> u64 {
    (seq << HANDED_FRAME_BITS) | (frames as u64).min(HANDED_FRAME_MASK)
}

fn unpack_handed(v: u64) -> (u64, u32) {
    (v >> HANDED_FRAME_BITS, (v & HANDED_FRAME_MASK) as u32)
}

/// Under-run counters of an output (monotonic).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Underruns {
    /// Device frames output without speech while the player was feeding speech.
    pub frames: u64,
    /// Xruns reported by the audio backend.
    pub xruns: u64,
}

/// What the player needs from an output (real or simulated). All positions are of the current generation only.
pub(crate) trait Sink {
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> u16;
    /// Advances bookkeeping to "now" (drains the callback's reports / consumes simulated time).
    fn update(&mut self, now_ms: f64);
    /// Room in the ring (stale samples that the callback has not discarded yet still take room).
    fn free_frames(&self) -> usize;
    /// Frames of the current generation queued and not yet handed to the device.
    fn queued_frames(&self) -> usize;
    fn push(&mut self, tag: BlockTag, samples: &[f32]) -> bool;
    /// Starts generation `generation` (greater than every generation used before, on any output): queued blocks of
    /// older generations are dropped (faded out).
    fn start_generation(&mut self, generation: u64);
    /// Last block of the current generation whose playback instant has passed.
    fn played(&self) -> Option<BlockTag>;
    /// The source position handed to the device so far in the current generation (interpolated inside the block in
    /// progress): where a seamless restart continues.
    fn consumed(&self) -> Option<BlockTag>;
    /// Frames rendered since the stream last started.
    fn rendered(&self) -> u64;
    fn set_volume(&self, volume: f32);
    fn set_keep_alive(&self, on: bool);
    /// The player is feeding continuous speech: an empty ring is an under-run.
    fn set_feeding(&self, _on: bool) {}
    fn underruns(&self) -> Underruns {
        Underruns::default()
    }
    fn suspend(&mut self);
    fn resume(&mut self);
    fn is_running(&self) -> bool;
    /// The stream died; `Some(true)` = because no output device is left.
    fn lost(&self) -> Option<bool>;
    fn device_id(&self) -> Option<&str>;
    fn is_simulated(&self) -> bool;
    /// Tests: the simulated output behind the trait object.
    #[cfg(test)]
    fn as_sim(&mut self) -> Option<&mut SimOutput> {
        None
    }
    /// Tests: the simulated ring device behind the trait object.
    #[cfg(test)]
    fn as_ring_sim(&mut self) -> Option<&mut RingSim> {
        None
    }
}

// ───────────────────────────── shared control ─────────────────────────────

#[derive(Debug, Clone, Copy)]
struct RingTag {
    tag: BlockTag,
    frames: u32,
    /// Push order on this output (never reused, across generations).
    seq: u64,
}

#[derive(Debug, Clone, Copy)]
struct PlayedEntry {
    tag: BlockTag,
    /// Device playback instant of the block's last frame (stream clock, ns).
    end_ns: u64,
}

const LOST_NONE: u8 = 0;
const LOST_STREAM: u8 = 1;
const LOST_NO_DEVICE: u8 = 2;

struct Ctrl {
    generation: AtomicU64,
    volume_bits: AtomicU32,
    keep_alive: AtomicBool,
    feeding: AtomicBool,
    rendered: AtomicU64,
    underrun_frames: AtomicU64,
    xruns: AtomicU64,
    /// The current-generation block the callback is in (or finished last) and the frames of it handed to the device,
    /// as of the end of its last run (`pack_handed`; 0 = none).
    handed: AtomicU64,
    lost: AtomicU8,
}

impl Ctrl {
    fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            volume_bits: AtomicU32::new(1.0f32.to_bits()),
            keep_alive: AtomicBool::new(false),
            feeding: AtomicBool::new(false),
            rendered: AtomicU64::new(0),
            underrun_frames: AtomicU64::new(0),
            xruns: AtomicU64::new(0),
            handed: AtomicU64::new(0),
            lost: AtomicU8::new(LOST_NONE),
        }
    }
}

/// The callback side: owns the consumer ends. Every buffer is allocated before the stream starts.
struct CallbackState {
    ctrl: Arc<Ctrl>,
    samples: rtrb::Consumer<f32>,
    tags: rtrb::Consumer<RingTag>,
    history: rtrb::Producer<PlayedEntry>,
    channels: usize,
    sample_rate: u32,
    /// The block being played and its frames left.
    cur: Option<RingTag>,
    remaining: usize,
    last_gen: u64,
    fade_len: usize,
    /// Frames left of the fade-in of the current generation.
    fade_in: usize,
    /// Frames left of the fade-out of interrupted (older-generation) audio.
    fade_out: usize,
    /// Frames left of the ramp of `tail_from` down to zero (speech stopped with no audio left to fade over); the
    /// ramp's gain is `tail / (tail_len + 1)`, so it continues a fade's slope and never repeats the last frame.
    tail: usize,
    tail_len: usize,
    tail_from: Vec<f32>,
    /// The last speech frame written (after fades and volume), per channel.
    last_out: Vec<f32>,
    /// `last_out` holds speech that has not been ramped down yet.
    speaking: bool,
    /// A block of the current generation has started (an empty ring after that is an under-run while feeding).
    gen_started: bool,
    /// `pack_handed` of the last current-generation block finished.
    last_done: u64,
    /// Applied volume, ramping towards the target (`NaN` until the first buffer).
    gain: f32,
    gain_step: f32,
    ka_index: u64,
}

impl CallbackState {
    fn new(
        ctrl: Arc<Ctrl>,
        samples: rtrb::Consumer<f32>,
        tags: rtrb::Consumer<RingTag>,
        history: rtrb::Producer<PlayedEntry>,
        channels: usize,
        sample_rate: u32,
    ) -> Self {
        let ch = channels.max(1);
        Self {
            ctrl,
            samples,
            tags,
            history,
            channels: ch,
            sample_rate,
            cur: None,
            remaining: 0,
            last_gen: 0,
            fade_len: (sample_rate as usize * FADE_MS / 1000).max(1),
            fade_in: 0,
            fade_out: 0,
            tail: 0,
            tail_len: 1,
            tail_from: vec![0.0; ch],
            last_out: vec![0.0; ch],
            speaking: false,
            gen_started: false,
            last_done: 0,
            gain: f32::NAN,
            gain_step: 1.0 / (sample_rate as usize * VOLUME_RAMP_MS / 1000).max(1) as f32,
            ka_index: 0,
        }
    }

    fn skip_samples(&mut self, n: usize) {
        let n = n.min(self.samples.slots());
        if let Ok(chunk) = self.samples.read_chunk(n) {
            chunk.commit_all();
        }
    }

    /// Ramps the last speech frame (plus whatever ramp is still running) down to zero over `len` frames.
    fn start_tail(&mut self, len: usize) {
        let g = self.tail as f32 / (self.tail_len + 1) as f32;
        for (from, last) in self.tail_from.iter_mut().zip(self.last_out.iter_mut()) {
            *from = *last + *from * g;
            *last = 0.0;
        }
        self.tail = len.max(1);
        self.tail_len = self.tail;
        self.speaking = false;
    }

    /// Pops the next block: the current generation's, or — while a fade-out runs — the interrupted audio queued
    /// behind the block that was playing (stale blocks are discarded otherwise).
    fn next_block(&mut self, gen: u64) {
        self.cur = None;
        self.remaining = 0;
        while let Ok(t) = self.tags.pop() {
            if t.tag.generation < gen {
                if self.fade_out > 0 && t.frames > 0 {
                    self.cur = Some(t);
                    self.remaining = t.frames as usize;
                    return;
                }
                self.skip_samples(t.frames as usize * self.channels);
                continue;
            }
            if self.fade_out > 0 {
                // The interrupted audio ran out before its fade did: finish the fade as a ramp of its last frame.
                if self.speaking {
                    self.start_tail(self.fade_out);
                }
                self.fade_out = 0;
            }
            self.cur = Some(t);
            self.remaining = t.frames as usize;
            self.gen_started = true;
            return;
        }
    }

    fn fill<T: SizedSample + FromSample<f32>>(&mut self, data: &mut [T], info: &OutputCallbackInfo) {
        let ch = self.channels;
        let frames = data.len() / ch;
        // (A partial trailing frame never happens; it gets silence.)
        data[frames * ch..].iter_mut().for_each(|x| *x = T::EQUILIBRIUM);
        let gen = self.ctrl.generation.load(Ordering::Acquire);
        let target = f32::from_bits(self.ctrl.volume_bits.load(Ordering::Relaxed));
        let target = if target.is_finite() { target.clamp(0.0, 1.0) } else { 1.0 };
        if !self.gain.is_finite() {
            self.gain = target;
        }
        let ka = self.ctrl.keep_alive.load(Ordering::Relaxed);
        let feeding = self.ctrl.feeding.load(Ordering::Acquire);
        let base_ns = u64::try_from(info.timestamp().playback.as_nanos()).unwrap_or(u64::MAX);
        if gen != self.last_gen {
            self.last_gen = gen;
            // (A block of the new generation already in progress — popped during the previous buffer, right after
            // the flush — keeps playing untouched.)
            let current = self.cur.is_some_and(|c| c.tag.generation >= gen);
            if !current {
                // Whatever was playing fades out from the level it is at — full, part-way through its fade-in, or
                // already fading out after an earlier flush — at the full fade's slope; the new generation fades in.
                let fl = self.fade_len.max(1);
                let level = if self.fade_out > 0 {
                    self.fade_out
                } else if self.fade_in > 0 {
                    fl.saturating_sub(self.fade_in)
                } else {
                    fl
                };
                self.fade_out = if self.cur.is_some() || self.speaking { level } else { 0 };
                self.fade_in = self.fade_len;
                self.gen_started = false;
                self.last_done = 0;
            }
        }
        let fade_len = self.fade_len.max(1) as f32;
        let mut underrun = 0u64;
        let mut f = 0;
        while f < frames {
            if self.remaining == 0 {
                self.next_block(gen);
            }
            let Some(c) = self.cur else {
                // Nothing queued: keep-alive (or silence) for the rest of the buffer, under a ramp of the last speech
                // frame when speech stopped without anything to fade over (an under-run, or the end of a read).
                if self.speaking {
                    let len = if self.fade_out > 0 { self.fade_out } else { self.fade_len };
                    self.start_tail(len);
                }
                self.fade_out = 0;
                // Whatever plays next (after an under-run, or the next chunk after a drained one) fades in, and with
                // no speech to zipper the volume can jump to its target.
                self.fade_in = self.fade_len;
                self.gain = target;
                if feeding && self.gen_started {
                    underrun += (frames - f) as u64;
                }
                for frame in data[f * ch..frames * ch].chunks_mut(ch) {
                    let ka_s = if ka { keep_alive_sample(self.ka_index, self.sample_rate) } else { 0.0 };
                    self.ka_index = self.ka_index.wrapping_add(1);
                    let tg = self.tail as f32 / (self.tail_len + 1) as f32;
                    self.tail = self.tail.saturating_sub(1);
                    for (out, from) in frame.iter_mut().zip(self.tail_from.iter()) {
                        *out = T::from_sample((ka_s + from * tg).clamp(-1.0, 1.0));
                    }
                }
                break;
            };
            let stale = c.tag.generation < gen;
            if stale && self.fade_out == 0 {
                // The fade-out is over (it ended at silence): drop the rest of the interrupted audio.
                self.skip_samples(self.remaining * ch);
                self.cur = None;
                self.remaining = 0;
                self.speaking = false;
                self.last_out.iter_mut().for_each(|x| *x = 0.0);
                continue;
            }
            let mut n = self.remaining.min(frames - f);
            if stale {
                n = n.min(self.fade_out);
            }
            let got = (self.samples.slots() / ch).min(n);
            if let Ok(chunk) = self.samples.read_chunk(got * ch) {
                let (s1, s2) = chunk.as_slices();
                let mut k = 0;
                // (inline: `self.samples` is borrowed by the read chunk)
                for frame in data[f * ch..(f + got) * ch].chunks_mut(ch) {
                    let fade = if stale {
                        let g = self.fade_out as f32 / fade_len;
                        self.fade_out = self.fade_out.saturating_sub(1);
                        g
                    } else if self.fade_in > 0 {
                        let g = 1.0 - self.fade_in as f32 / fade_len;
                        self.fade_in -= 1;
                        g
                    } else {
                        1.0
                    };
                    self.gain += (target - self.gain).clamp(-self.gain_step, self.gain_step);
                    let g = fade * self.gain;
                    let ka_s = if ka { keep_alive_sample(self.ka_index, self.sample_rate) } else { 0.0 };
                    self.ka_index = self.ka_index.wrapping_add(1);
                    let tg = self.tail as f32 / (self.tail_len + 1) as f32;
                    self.tail = self.tail.saturating_sub(1);
                    let lanes = frame.iter_mut().zip(self.last_out.iter_mut()).zip(self.tail_from.iter());
                    for ((out, last), from) in lanes {
                        let s = if k < s1.len() { s1[k] } else { s2.get(k - s1.len()).copied().unwrap_or(0.0) };
                        k += 1;
                        let v = s * g;
                        *last = v;
                        *out = T::from_sample((v + ka_s + from * tg).clamp(-1.0, 1.0));
                    }
                }
                chunk.commit_all();
                if got > 0 {
                    self.speaking = true;
                }
            }
            if got < n {
                // Descriptor without its samples (cannot happen: samples are pushed first): drop the block.
                self.remaining = 0;
                self.cur = None;
                f += got;
                continue;
            }
            self.remaining -= n;
            f += n;
            if self.remaining == 0 {
                self.cur = None;
                if !stale {
                    let end_ns = base_ns
                        .saturating_add((f as u64).saturating_mul(1_000_000_000) / self.sample_rate.max(1) as u64);
                    // A full report ring only loses position precision, never audio.
                    let _ = self.history.push(PlayedEntry { tag: c.tag, end_ns });
                    self.last_done = pack_handed(c.seq, c.frames as usize);
                }
            }
        }
        let handed = match self.cur {
            Some(c) if c.tag.generation >= gen => {
                pack_handed(c.seq, (c.frames as usize).saturating_sub(self.remaining))
            }
            _ => self.last_done,
        };
        self.ctrl.handed.store(handed, Ordering::Release);
        if underrun > 0 {
            self.ctrl.underrun_frames.fetch_add(underrun, Ordering::Relaxed);
        }
        self.ctrl.rendered.fetch_add(frames as u64, Ordering::Relaxed);
    }
}

// ───────────────────────────── the player side of the rings ─────────────────────────────

/// A block of the current generation pushed and not yet fully handed to the device.
#[derive(Debug, Clone, Copy)]
struct Pushed {
    seq: u64,
    chunk: u32,
    /// Source frames the block starts and ends at (`start == end` when unknown: the first block of a feed).
    start: f64,
    end: f64,
    frames: u32,
}

/// The player-side ends of the rings plus the bookkeeping built on the callback's reports.
struct Rings {
    sample_rate: u32,
    channels: u16,
    ctrl: Arc<Ctrl>,
    samples: rtrb::Producer<f32>,
    tags: rtrb::Producer<RingTag>,
    history: rtrb::Consumer<PlayedEntry>,
    reports: VecDeque<PlayedEntry>,
    played: Option<BlockTag>,
    generation: u64,
    seq: u64,
    /// Current-generation blocks not yet fully handed to the device (oldest first) and their total frames.
    pushed: VecDeque<Pushed>,
    pushed_frames: usize,
    last_pushed: Option<BlockTag>,
    /// The last block dropped from `pushed` (fully handed).
    last_handed: Option<Pushed>,
}

/// A fresh set of rings for a stream (player side, callback side).
fn rings(sample_rate: u32, channels: u16) -> (Rings, CallbackState) {
    let ch = channels.max(1) as usize;
    let capacity_frames = (sample_rate as usize * RING_MS / 1000).max(64);
    let (samples_tx, samples_rx) = rtrb::RingBuffer::<f32>::new(capacity_frames * ch);
    let (tags_tx, tags_rx) = rtrb::RingBuffer::<RingTag>::new(TAG_SLOTS);
    let (history_tx, history_rx) = rtrb::RingBuffer::<PlayedEntry>::new(HISTORY_SLOTS);
    let ctrl = Arc::new(Ctrl::new());
    let state = CallbackState::new(ctrl.clone(), samples_rx, tags_rx, history_tx, ch, sample_rate);
    let rings = Rings {
        sample_rate,
        channels: channels.max(1),
        ctrl,
        samples: samples_tx,
        tags: tags_tx,
        history: history_rx,
        reports: VecDeque::with_capacity(HISTORY_SLOTS),
        played: None,
        generation: 0,
        seq: 0,
        pushed: VecDeque::with_capacity(TAG_SLOTS * 2),
        pushed_frames: 0,
        last_pushed: None,
        last_handed: None,
    };
    (rings, state)
}

impl Rings {
    fn ch(&self) -> usize {
        self.channels.max(1) as usize
    }

    fn free_frames(&self) -> usize {
        if self.tags.slots() == 0 {
            0
        } else {
            self.samples.slots() / self.ch()
        }
    }

    fn push(&mut self, tag: BlockTag, samples: &[f32]) -> bool {
        let ch = self.ch();
        let frames = samples.len() / ch;
        if frames == 0 || frames > u32::MAX as usize || self.samples.slots() < frames * ch || self.tags.slots() == 0 {
            return false;
        }
        if self.samples.push_entire_slice(&samples[..frames * ch]).is_err() {
            return false;
        }
        self.seq += 1;
        if self.tags.push(RingTag { tag, frames: frames as u32, seq: self.seq }).is_err() {
            return false;
        }
        let start = match self.last_pushed {
            Some(p) if p.chunk == tag.chunk => p.source_frame_end,
            // A block across a chunk join starts in the previous chunk: count it from the new chunk's start.
            Some(_) => 0.0,
            None => tag.source_frame_end,
        };
        self.pushed.push_back(Pushed {
            seq: self.seq,
            chunk: tag.chunk,
            start: start.min(tag.source_frame_end),
            end: tag.source_frame_end,
            frames: frames as u32,
        });
        self.pushed_frames += frames;
        self.last_pushed = Some(tag);
        true
    }

    fn handed(&self) -> (u64, u32) {
        unpack_handed(self.ctrl.handed.load(Ordering::Acquire))
    }

    fn queued_frames(&self) -> usize {
        let (seq, done) = self.handed();
        let mut handed = 0usize;
        for e in &self.pushed {
            if e.seq < seq {
                handed += e.frames as usize;
            } else {
                if e.seq == seq {
                    handed += done.min(e.frames) as usize;
                }
                break;
            }
        }
        self.pushed_frames.saturating_sub(handed)
    }

    fn handed_tag(&self) -> Option<BlockTag> {
        let (seq, done) = self.handed();
        let e = self.pushed.iter().find(|e| e.seq == seq).or(self.last_handed.as_ref().filter(|e| e.seq == seq))?;
        let frac = done.min(e.frames) as f64 / e.frames.max(1) as f64;
        let source_frame_end = e.start + (e.end - e.start) * frac;
        Some(BlockTag { generation: self.generation, chunk: e.chunk, source_frame_end })
    }

    /// Forgets the blocks the device has been handed completely.
    fn trim_handed(&mut self) {
        let (seq, done) = self.handed();
        while let Some(front) = self.pushed.front().copied() {
            if front.seq > seq || (front.seq == seq && done < front.frames) {
                break;
            }
            self.pushed_frames = self.pushed_frames.saturating_sub(front.frames as usize);
            self.last_handed = Some(front);
            self.pushed.pop_front();
        }
    }

    fn start_generation(&mut self, generation: u64) {
        self.generation = generation;
        self.ctrl.generation.store(generation, Ordering::Release);
        self.reports.clear();
        self.played = None;
        self.pushed.clear();
        self.pushed_frames = 0;
        self.last_pushed = None;
        self.last_handed = None;
    }

    /// Collects the callback's finished-block reports and advances the played tag to `now_ns` (stream clock).
    fn drain_reports(&mut self, now_ns: u64) {
        while let Ok(entry) = self.history.pop() {
            if entry.tag.generation != self.generation {
                continue;
            }
            if self.reports.len() >= HISTORY_SLOTS * 4 {
                // The player stopped draining for a long time: the oldest reports have long been played.
                if let Some(old) = self.reports.pop_front() {
                    self.played = Some(old.tag);
                }
            }
            self.reports.push_back(entry);
        }
        while let Some(front) = self.reports.front() {
            if front.end_ns > now_ns {
                break;
            }
            self.played = Some(front.tag);
            self.reports.pop_front();
        }
        self.trim_handed();
    }

    fn set_volume(&self, volume: f32) {
        let v = if volume.is_finite() { volume.clamp(0.0, 1.0) } else { 1.0 };
        self.ctrl.volume_bits.store(v.to_bits(), Ordering::Relaxed);
    }

    fn underruns(&self) -> Underruns {
        Underruns {
            frames: self.ctrl.underrun_frames.load(Ordering::Relaxed),
            xruns: self.ctrl.xruns.load(Ordering::Relaxed),
        }
    }
}

// ───────────────────────────── the real output ─────────────────────────────

/// The open output (lives on the player thread).
pub struct AudioOutput {
    stream: cpal::Stream,
    rings: Rings,
    running: bool,
    rendered_base: u64,
    device_id: Option<String>,
}

fn map_build_error(e: cpal::Error) -> OutputError {
    match e.kind() {
        ErrorKind::DeviceNotAvailable | ErrorKind::HostUnavailable => OutputError::NoDevice,
        _ => OutputError::Stream(e.to_string()),
    }
}

impl AudioOutput {
    /// Opens the default output device (stream starts suspended).
    pub fn open() -> Result<AudioOutput, OutputError> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or(OutputError::NoDevice)?;
        let device_id = device.id().ok().map(|id| id.to_string());
        let config = device.default_output_config().map_err(map_build_error)?;
        let sample_rate = config.sample_rate();
        let channels = config.channels();
        if sample_rate == 0 || channels == 0 {
            return Err(OutputError::Stream("the device reported an empty format".into()));
        }
        let stream_config = cpal::StreamConfig { channels, sample_rate, buffer_size: cpal::BufferSize::Default };
        // WASAPI shared mode converts any PCM format (AUTOCONVERTPCM), so f32 is asked for first; the device's own
        // format is the fallback. Each attempt gets fresh rings (a failed build drops its callback state).
        let mut formats = vec![SampleFormat::F32];
        if config.sample_format() != SampleFormat::F32 {
            formats.push(config.sample_format());
        }
        let mut last_error = None;
        for format in formats {
            let (rings, state) = rings(sample_rate, channels);
            let built = match format {
                SampleFormat::F32 => build_stream::<f32>(&device, &stream_config, state),
                SampleFormat::I16 => build_stream::<i16>(&device, &stream_config, state),
                SampleFormat::I32 => build_stream::<i32>(&device, &stream_config, state),
                SampleFormat::U16 => build_stream::<u16>(&device, &stream_config, state),
                SampleFormat::F64 => build_stream::<f64>(&device, &stream_config, state),
                other => {
                    last_error = Some(OutputError::Stream(format!("unsupported sample format {other}")));
                    continue;
                }
            };
            match built {
                Ok(stream) => {
                    return Ok(AudioOutput { stream, rings, running: false, rendered_base: 0, device_id });
                }
                Err(e) => last_error = Some(map_build_error(e)),
            }
        }
        Err(last_error.unwrap_or_else(|| OutputError::Stream("no usable sample format".into())))
    }

    pub fn sample_rate(&self) -> u32 {
        self.rings.sample_rate
    }
    pub fn channels(&self) -> u16 {
        self.rings.channels
    }
    /// Free space in the ring, in frames.
    pub fn free_frames(&self) -> usize {
        self.rings.free_frames()
    }
    /// Queues one interleaved block (device rate and channel count).
    pub fn push(&mut self, tag: BlockTag, samples: &[f32]) -> bool {
        self.rings.push(tag, samples)
    }
    /// Tag of the last block whose device playback instant is ≤ now (see [`BlockTag`]), as of the last
    /// [`AudioOutput::refresh`]; the default tag before any block of the current generation played.
    pub fn played_tag(&self) -> BlockTag {
        self.rings.played.unwrap_or_default()
    }

    /// Collects the callback's finished-block reports and advances the played tag to the stream clock's "now".
    pub fn refresh(&mut self) {
        let now_ns = u64::try_from(self.stream.now().as_nanos()).unwrap_or(u64::MAX);
        self.rings.drain_reports(now_ns);
    }
    /// Total frames the device consumed since the stream last started (warmer clock).
    pub fn rendered_frames(&self) -> u64 {
        self.rings.ctrl.rendered.load(Ordering::Relaxed).saturating_sub(self.rendered_base)
    }
    /// Linear 0..1 (the callback ramps to it).
    pub fn set_volume(&self, volume: f32) {
        self.rings.set_volume(volume);
    }
    /// Starts a new generation: queued blocks of older generations are dropped by the callback.
    pub fn flush(&mut self) -> u64 {
        let generation = self.rings.generation + 1;
        self.rings.start_generation(generation);
        generation
    }
    /// Keep-alive signal on/off when nothing is queued.
    pub fn set_keep_alive(&self, on: bool) {
        self.rings.ctrl.keep_alive.store(on, Ordering::Relaxed);
    }
    pub fn suspend(&mut self) {
        if self.running {
            if let Err(e) = self.stream.pause() {
                tracing::warn!(target: "audio", "pausing the output stream failed: {e}");
                self.rings.ctrl.lost.store(LOST_STREAM, Ordering::Relaxed);
            }
            self.running = false;
        }
    }
    pub fn resume(&mut self) {
        if !self.running {
            self.rendered_base = self.rings.ctrl.rendered.load(Ordering::Relaxed);
            match self.stream.play() {
                Ok(()) => self.running = true,
                Err(e) => {
                    tracing::warn!(target: "audio", "starting the output stream failed: {e}");
                    let kind = if e.kind() == ErrorKind::DeviceNotAvailable { LOST_NO_DEVICE } else { LOST_STREAM };
                    self.rings.ctrl.lost.store(kind, Ordering::Relaxed);
                }
            }
        }
    }
    /// The stream reported an error (device removed).
    pub fn is_lost(&self) -> bool {
        self.rings.ctrl.lost.load(Ordering::Relaxed) != LOST_NONE
    }
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut state: CallbackState,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + FromSample<f32> + Send + 'static,
{
    let ctrl_err = state.ctrl.clone();
    let data_cb = move |data: &mut [T], info: &OutputCallbackInfo| {
        // No panic may cross into cpal's audio thread: a panicking fill outputs silence and marks the stream lost.
        if catch_unwind(AssertUnwindSafe(|| state.fill(data, info))).is_err() {
            data.iter_mut().for_each(|x| *x = T::EQUILIBRIUM);
            state.ctrl.lost.store(LOST_STREAM, Ordering::Relaxed);
        }
    };
    let error_cb = move |err: cpal::Error| match err.kind() {
        ErrorKind::Xrun => {
            ctrl_err.xruns.fetch_add(1, Ordering::Relaxed);
        }
        ErrorKind::RealtimeDenied => {}
        ErrorKind::DeviceNotAvailable => ctrl_err.lost.store(LOST_NO_DEVICE, Ordering::Relaxed),
        _ => ctrl_err.lost.store(LOST_STREAM, Ordering::Relaxed),
    };
    device.build_output_stream::<T, _, _>(*config, data_cb, error_cb, Some(Duration::from_secs(5)))
}

impl Sink for AudioOutput {
    fn sample_rate(&self) -> u32 {
        self.rings.sample_rate
    }
    fn channels(&self) -> u16 {
        self.rings.channels
    }
    fn update(&mut self, _now_ms: f64) {
        self.refresh();
    }
    fn free_frames(&self) -> usize {
        self.rings.free_frames()
    }
    fn queued_frames(&self) -> usize {
        self.rings.queued_frames()
    }
    fn push(&mut self, tag: BlockTag, samples: &[f32]) -> bool {
        self.rings.push(tag, samples)
    }
    fn start_generation(&mut self, generation: u64) {
        self.rings.start_generation(generation);
    }
    fn played(&self) -> Option<BlockTag> {
        self.rings.played
    }
    fn consumed(&self) -> Option<BlockTag> {
        self.rings.handed_tag()
    }
    fn rendered(&self) -> u64 {
        self.rendered_frames()
    }
    fn set_volume(&self, volume: f32) {
        self.rings.set_volume(volume)
    }
    fn set_keep_alive(&self, on: bool) {
        AudioOutput::set_keep_alive(self, on)
    }
    fn set_feeding(&self, on: bool) {
        self.rings.ctrl.feeding.store(on, Ordering::Release);
    }
    fn underruns(&self) -> Underruns {
        self.rings.underruns()
    }
    fn suspend(&mut self) {
        AudioOutput::suspend(self)
    }
    fn resume(&mut self) {
        AudioOutput::resume(self)
    }
    fn is_running(&self) -> bool {
        self.running
    }
    fn lost(&self) -> Option<bool> {
        match self.rings.ctrl.lost.load(Ordering::Relaxed) {
            LOST_NONE => None,
            LOST_NO_DEVICE => Some(true),
            _ => Some(false),
        }
    }
    fn device_id(&self) -> Option<&str> {
        self.device_id.as_deref()
    }
    fn is_simulated(&self) -> bool {
        false
    }
}

/// The id of the current default output device (`None` = no device).
pub(crate) fn default_device_id() -> Option<String> {
    let host = cpal::default_host();
    let device = host.default_output_device()?;
    Some(device.id().map(|id| id.to_string()).unwrap_or_default())
}

// ───────────────────────────── the simulated output ─────────────────────────────

/// A device on a simulated clock: blocks are "played" in real time from the moment they are queued (no latency).
pub(crate) struct SimOutput {
    sample_rate: u32,
    channels: u16,
    capacity_frames: usize,
    queue: VecDeque<(BlockTag, usize)>,
    queued: usize,
    played: Option<BlockTag>,
    running: bool,
    last_ms: Option<f64>,
    carry: f64,
    rendered_total: u64,
    rendered_base: u64,
    /// Every pushed sample, for tests that inspect the rendered stream.
    #[cfg(test)]
    pub(crate) capture: Option<Vec<(BlockTag, Vec<f32>)>>,
    /// Tests: behave like a real device (warm-up applies), report a device id, simulate a loss.
    #[cfg(test)]
    pub(crate) test_real: bool,
    #[cfg(test)]
    pub(crate) test_device: Option<String>,
    #[cfg(test)]
    pub(crate) test_lost: Option<bool>,
    /// Tests: a device that exists but never renders anything.
    #[cfg(test)]
    pub(crate) test_frozen: bool,
}

impl SimOutput {
    pub(crate) fn new(sample_rate: u32, channels: u16) -> Self {
        Self {
            sample_rate: sample_rate.max(1),
            channels: channels.max(1),
            capacity_frames: (sample_rate.max(1) as usize * RING_MS / 1000).max(64),
            queue: VecDeque::new(),
            queued: 0,
            played: None,
            running: false,
            last_ms: None,
            carry: 0.0,
            rendered_total: 0,
            rendered_base: 0,
            #[cfg(test)]
            capture: None,
            #[cfg(test)]
            test_real: false,
            #[cfg(test)]
            test_device: None,
            #[cfg(test)]
            test_lost: None,
            #[cfg(test)]
            test_frozen: false,
        }
    }
}

impl Sink for SimOutput {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn channels(&self) -> u16 {
        self.channels
    }
    fn update(&mut self, now_ms: f64) {
        let last = self.last_ms.replace(now_ms);
        if !self.running {
            return;
        }
        #[cfg(test)]
        if self.test_frozen {
            return;
        }
        let Some(last) = last else { return };
        let elapsed = (now_ms - last).max(0.0) * self.sample_rate as f64 / 1000.0 + self.carry;
        let mut frames = elapsed.floor() as usize;
        self.carry = elapsed - frames as f64;
        self.rendered_total += frames as u64;
        while frames > 0 {
            let Some(front) = self.queue.front_mut() else { break };
            let take = front.1.min(frames);
            front.1 -= take;
            frames -= take;
            self.queued -= take;
            if front.1 == 0 {
                self.played = Some(front.0);
                self.queue.pop_front();
            }
        }
    }
    fn free_frames(&self) -> usize {
        self.capacity_frames.saturating_sub(self.queued)
    }
    fn queued_frames(&self) -> usize {
        self.queued
    }
    fn push(&mut self, tag: BlockTag, samples: &[f32]) -> bool {
        let frames = samples.len() / self.channels as usize;
        if frames == 0 || frames > self.free_frames() {
            return false;
        }
        #[cfg(test)]
        if let Some(c) = self.capture.as_mut() {
            c.push((tag, samples.to_vec()));
        }
        self.queue.push_back((tag, frames));
        self.queued += frames;
        true
    }
    fn start_generation(&mut self, _generation: u64) {
        self.queue.clear();
        self.queued = 0;
        self.played = None;
    }
    fn played(&self) -> Option<BlockTag> {
        self.played
    }
    fn consumed(&self) -> Option<BlockTag> {
        self.played
    }
    fn rendered(&self) -> u64 {
        self.rendered_total - self.rendered_base
    }
    fn set_volume(&self, _volume: f32) {}
    fn set_keep_alive(&self, _on: bool) {}
    fn suspend(&mut self) {
        self.running = false;
    }
    fn resume(&mut self) {
        if !self.running {
            self.running = true;
            self.rendered_base = self.rendered_total;
        }
    }
    fn is_running(&self) -> bool {
        self.running
    }
    fn lost(&self) -> Option<bool> {
        #[cfg(test)]
        if self.test_lost.is_some() {
            return self.test_lost;
        }
        None
    }
    fn device_id(&self) -> Option<&str> {
        #[cfg(test)]
        if self.test_device.is_some() {
            return self.test_device.as_deref();
        }
        None
    }
    fn is_simulated(&self) -> bool {
        #[cfg(test)]
        if self.test_real {
            return false;
        }
        true
    }
    #[cfg(test)]
    fn as_sim(&mut self) -> Option<&mut SimOutput> {
        Some(self)
    }
}

// ───────────────────────────── tests: the real rings on a simulated device ─────────────────────────────

/// The real rings and callback, with the callback run by a simulated device clock: every `period` frames of device
/// time, playing `latency_ms` after the callback. Records every rendered sample and every pushed tag.
#[cfg(test)]
pub(crate) struct RingSim {
    rings: Rings,
    cb: CallbackState,
    period: usize,
    latency_ns: u64,
    next_cb_ns: Option<u64>,
    running: bool,
    rendered_base: u64,
    buf: Vec<f32>,
    /// Every rendered sample (interleaved).
    pub(crate) rendered_out: Vec<f32>,
    /// Every pushed block tag.
    pub(crate) pushed_tags: Vec<BlockTag>,
    pub(crate) device: Option<String>,
    pub(crate) test_lost: Option<bool>,
}

#[cfg(test)]
impl RingSim {
    pub(crate) fn new(sample_rate: u32, channels: u16, period: usize, latency_ms: f64) -> Self {
        let (rings, cb) = rings(sample_rate, channels);
        Self {
            rings,
            cb,
            period: period.max(1),
            latency_ns: (latency_ms.max(0.0) * 1e6) as u64,
            next_cb_ns: None,
            running: false,
            rendered_base: 0,
            buf: vec![0.0; period.max(1) * channels.max(1) as usize],
            rendered_out: Vec::new(),
            pushed_tags: Vec::new(),
            device: None,
            test_lost: None,
        }
    }

    fn run_callbacks(&mut self, now_ns: u64) {
        let Some(mut t) = self.next_cb_ns else { return };
        let period_ns = self.period as u64 * 1_000_000_000 / self.rings.sample_rate.max(1) as u64;
        while t <= now_ns {
            let ts = cpal::OutputStreamTimestamp {
                callback: cpal::StreamInstant::from_nanos(t),
                playback: cpal::StreamInstant::from_nanos(t + self.latency_ns),
            };
            self.cb.fill(&mut self.buf, &OutputCallbackInfo::new(ts));
            self.rendered_out.extend_from_slice(&self.buf);
            t += period_ns.max(1);
        }
        self.next_cb_ns = Some(t);
    }
}

#[cfg(test)]
impl Sink for RingSim {
    fn sample_rate(&self) -> u32 {
        self.rings.sample_rate
    }
    fn channels(&self) -> u16 {
        self.rings.channels
    }
    fn update(&mut self, now_ms: f64) {
        let now_ns = (now_ms.max(0.0) * 1e6) as u64;
        if self.running {
            if self.next_cb_ns.is_none() {
                self.next_cb_ns = Some(now_ns);
            }
            self.run_callbacks(now_ns);
        }
        self.rings.drain_reports(now_ns);
    }
    fn free_frames(&self) -> usize {
        self.rings.free_frames()
    }
    fn queued_frames(&self) -> usize {
        self.rings.queued_frames()
    }
    fn push(&mut self, tag: BlockTag, samples: &[f32]) -> bool {
        let ok = self.rings.push(tag, samples);
        if ok {
            self.pushed_tags.push(tag);
        }
        ok
    }
    fn start_generation(&mut self, generation: u64) {
        self.rings.start_generation(generation);
    }
    fn played(&self) -> Option<BlockTag> {
        self.rings.played
    }
    fn consumed(&self) -> Option<BlockTag> {
        self.rings.handed_tag()
    }
    fn rendered(&self) -> u64 {
        self.rings.ctrl.rendered.load(Ordering::Relaxed).saturating_sub(self.rendered_base)
    }
    fn set_volume(&self, volume: f32) {
        self.rings.set_volume(volume);
    }
    fn set_keep_alive(&self, on: bool) {
        self.rings.ctrl.keep_alive.store(on, Ordering::Relaxed);
    }
    fn set_feeding(&self, on: bool) {
        self.rings.ctrl.feeding.store(on, Ordering::Release);
    }
    fn underruns(&self) -> Underruns {
        self.rings.underruns()
    }
    fn suspend(&mut self) {
        self.running = false;
        self.next_cb_ns = None;
    }
    fn resume(&mut self) {
        if !self.running {
            self.running = true;
            self.rendered_base = self.rings.ctrl.rendered.load(Ordering::Relaxed);
        }
    }
    fn is_running(&self) -> bool {
        self.running
    }
    fn lost(&self) -> Option<bool> {
        self.test_lost
    }
    fn device_id(&self) -> Option<&str> {
        self.device.as_deref()
    }
    fn is_simulated(&self) -> bool {
        false
    }
    fn as_ring_sim(&mut self) -> Option<&mut RingSim> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(gen: u64, chunk: u32, end: f64) -> BlockTag {
        BlockTag { generation: gen, chunk, source_frame_end: end }
    }

    fn info_at(ms: u64) -> OutputCallbackInfo {
        OutputCallbackInfo::new(cpal::OutputStreamTimestamp {
            callback: cpal::StreamInstant::from_millis(ms),
            playback: cpal::StreamInstant::from_millis(ms),
        })
    }

    #[test]
    fn sim_plays_in_real_time() {
        let mut s = SimOutput::new(1000, 1);
        assert_eq!(s.free_frames(), 250);
        s.resume();
        s.update(0.0);
        assert!(s.push(tag(0, 0, 10.0), &[0.0; 10]));
        assert!(s.push(tag(0, 0, 20.0), &[0.0; 10]));
        assert_eq!(s.queued_frames(), 20);
        s.update(5.0);
        assert_eq!(s.played(), None);
        s.update(10.0);
        assert_eq!(s.played(), Some(tag(0, 0, 10.0)));
        s.update(25.0);
        assert_eq!(s.played(), Some(tag(0, 0, 20.0)));
        assert_eq!(s.queued_frames(), 0);
        assert_eq!(s.rendered(), 25);
        // Suspended: time does not advance the device.
        s.suspend();
        assert!(s.push(tag(0, 1, 5.0), &[0.0; 5]));
        s.update(100.0);
        assert_eq!(s.queued_frames(), 5);
        // Resumed: the warmer clock restarts.
        s.resume();
        assert_eq!(s.rendered(), 0);
        s.update(103.0);
        assert_eq!(s.rendered(), 3);
    }

    #[test]
    fn sim_flush_drops_queued_and_resets_positions() {
        let mut s = SimOutput::new(1000, 2);
        s.resume();
        s.update(0.0);
        assert!(s.push(tag(0, 0, 10.0), &[0.0; 20]));
        s.update(10.0);
        assert!(s.played().is_some());
        assert!(s.push(tag(0, 0, 20.0), &[0.0; 20]));
        s.start_generation(1);
        assert_eq!(s.queued_frames(), 0);
        assert_eq!(s.played(), None);
        assert_eq!(s.consumed(), None);
        // Over-capacity pushes are refused.
        assert!(!s.push(tag(1, 0, 0.0), &vec![0.0; 2 * 300]));
        assert!(!s.push(tag(1, 0, 0.0), &[]));
    }

    /// A callback state on fresh rings, with a given fade length (frames).
    fn callback(sample_rate: u32, channels: u16, fade_len: usize) -> (Rings, CallbackState) {
        let (r, mut cb) = rings(sample_rate, channels);
        cb.fade_len = fade_len;
        (r, cb)
    }

    fn fill(cb: &mut CallbackState, frames: usize, channels: usize, out: &mut Vec<f32>) {
        let mut buf = vec![0f32; frames * channels];
        cb.fill(&mut buf, &info_at(100));
        out.extend_from_slice(&buf);
    }

    fn max_step(x: &[f32]) -> f32 {
        x.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max)
    }

    /// Exercises the real callback logic without a device: blocks, a flush mid-block with fades, keep-alive.
    #[test]
    fn callback_pops_blocks_fades_and_reports() {
        let (mut r, mut st) = callback(1000, 1, 4);
        r.set_volume(0.5);
        assert!(r.push(tag(0, 0, 10.0), &[1.0; 10]));
        assert!(r.push(tag(0, 0, 20.0), &[1.0; 10]));
        let mut buf = [0f32; 15];
        st.fill(&mut buf, &info_at(100));
        assert!(buf.iter().all(|x| (*x - 0.5).abs() < 1e-6), "{buf:?}");
        // The first block finished at frame 10 → playback instant 100 ms + 10 ms.
        let e = r.history.pop().expect("report");
        assert_eq!(e.tag, tag(0, 0, 10.0));
        assert_eq!(e.end_ns, 110_000_000);
        assert!(r.history.pop().is_err());
        // Five frames of the second block handed: the handed position interpolates inside it.
        assert_eq!(r.handed_tag(), Some(tag(0, 0, 15.0)));
        assert_eq!(r.queued_frames(), 5);
        // Flush mid-block: the old block fades out over 4 frames, then the new generation fades in.
        r.start_generation(2);
        assert_eq!(r.queued_frames(), 0, "stale samples do not count as queued");
        assert!(r.push(tag(2, 3, 100.0), &[1.0; 10]));
        assert_eq!(r.queued_frames(), 10);
        let mut buf = [0f32; 12];
        st.fill(&mut buf, &info_at(100));
        assert!((buf[0] - 0.5).abs() < 1e-6 && buf[3] < buf[0] && buf[3] > 0.0, "fade out {buf:?}");
        assert_eq!(buf[4], 0.0, "fade-in starts from silence: {buf:?}");
        assert!(buf[5] > 0.0 && buf[5] < 0.5);
        assert!((buf[11] - 0.5).abs() < 1e-6);
        // The interrupted block is not reported as played; the new one is once finished.
        let mut buf = [0f32; 4];
        st.fill(&mut buf, &info_at(100));
        let e = r.history.pop().expect("report");
        assert_eq!(e.tag, tag(2, 3, 100.0));
        assert!(r.history.pop().is_err(), "only the new block is reported");
        assert_eq!(r.queued_frames(), 0);
        // Nothing queued: keep-alive when on, silence otherwise (after the 4-frame ramp of the last frame).
        r.ctrl.keep_alive.store(true, Ordering::Relaxed);
        let mut buf = [0f32; 8];
        st.fill(&mut buf, &info_at(100));
        assert!(buf[4..].iter().all(|x| x.abs() == 1.0 / 32768.0), "{buf:?}");
        r.ctrl.keep_alive.store(false, Ordering::Relaxed);
        st.fill(&mut buf, &info_at(100));
        assert!(buf.iter().all(|x| *x == 0.0));
        assert_eq!(r.ctrl.rendered.load(Ordering::Relaxed), 15 + 12 + 4 + 8 + 8);
    }

    /// Finding: a flush that lands in the last frames of a block (or at its very end) must still fade over the full
    /// 2 ms, on into the stale blocks behind it.
    #[test]
    fn flush_always_fades_over_the_full_length() {
        let fade = 96;
        let level = 0.5f32;
        for remaining in [0usize, 1, 10, 48, fade - 1, fade, fade + 1, 200] {
            for stale_behind in [true, false] {
                let (mut r, mut cb) = callback(48_000, 1, fade);
                r.start_generation(1);
                let blocks = if stale_behind { 3 } else { 1 };
                for b in 0..blocks {
                    assert!(r.push(tag(1, 0, (b + 1) as f64 * 481.0), &[level; 481]));
                }
                let mut out = Vec::new();
                // The block in progress has `remaining` frames left when the flush lands.
                fill(&mut cb, 481 - remaining, 1, &mut out);
                r.start_generation(2);
                for b in 0..3 {
                    assert!(r.push(tag(2, 0, (b + 1) as f64 * 481.0), &[level; 481]));
                }
                fill(&mut cb, 480, 1, &mut out);
                fill(&mut cb, 480, 1, &mut out);
                let step = max_step(&out);
                assert!(
                    step <= level / fade as f32 + 1e-5,
                    "remaining {remaining}, stale blocks behind: {stale_behind}: step {step}"
                );
                // The new generation reaches full level.
                assert!((out[out.len() - 1] - level).abs() < 1e-6);
            }
        }
    }

    /// Two flushes within one fade (a rate change then a seek, quickly): the second fade continues from the level the
    /// first one (or the fade-in after it) had reached, never jumping back to full level.
    #[test]
    fn a_flush_during_a_fade_continues_from_its_level() {
        let fade = 96;
        let level = 0.5f32;
        // `first` frames are played after the first flush before the second one lands: within the fade-out, at the
        // switch, or within the fade-in of the second generation.
        for first in [10usize, 50, 95, 96, 97, 130, 191] {
            let (mut r, mut cb) = callback(48_000, 1, fade);
            let mut out = Vec::new();
            r.start_generation(1);
            for b in 0..3 {
                assert!(r.push(tag(1, 0, (b + 1) as f64 * 480.0), &[level; 480]));
            }
            fill(&mut cb, 400, 1, &mut out);
            r.start_generation(2);
            for b in 0..3 {
                assert!(r.push(tag(2, 0, (b + 1) as f64 * 480.0), &[level; 480]));
            }
            fill(&mut cb, first, 1, &mut out);
            r.start_generation(3);
            for b in 0..3 {
                assert!(r.push(tag(3, 0, (b + 1) as f64 * 480.0), &[level; 480]));
            }
            fill(&mut cb, 480, 1, &mut out);
            let step = max_step(&out[fade..]);
            assert!(step <= level / fade as f32 + 1e-5, "second flush {first} frames after the first: step {step}");
            assert!((out[out.len() - 1] - level).abs() < 1e-6);
        }
    }

    /// After an under-run the speech fades back in (and the ramp down before it never clicks either).
    #[test]
    fn speech_after_an_under_run_fades_in() {
        let fade = 96;
        let (mut r, mut cb) = callback(48_000, 1, fade);
        let mut out = Vec::new();
        r.start_generation(1);
        assert!(r.push(tag(1, 0, 480.0), &[0.5; 480]));
        fill(&mut cb, 480 + 200, 1, &mut out);
        assert!(r.push(tag(1, 0, 960.0), &[0.5; 480]));
        fill(&mut cb, 480, 1, &mut out);
        assert!(max_step(&out) <= 0.5 / fade as f32 + 1e-5, "{}", max_step(&out));
        assert!((out[out.len() - 1] - 0.5).abs() < 1e-6);
    }

    /// An empty ring while speech plays (an under-run, or the end of a read) ramps down instead of cutting.
    #[test]
    fn running_dry_ramps_the_last_frame_down() {
        let fade = 96;
        let (mut r, mut cb) = callback(48_000, 2, fade);
        r.start_generation(1);
        assert!(r.push(tag(1, 0, 481.0), &[0.5; 962]));
        let mut out = Vec::new();
        fill(&mut cb, 480, 2, &mut out);
        fill(&mut cb, 480, 2, &mut out);
        let left: Vec<f32> = out.iter().step_by(2).copied().collect();
        assert!(max_step(&left) <= 0.5 / fade as f32 + 1e-5, "{}", max_step(&left));
        assert_eq!(left[481 + fade], 0.0);
        assert_eq!(*left.last().expect("frames"), 0.0);
    }

    #[test]
    fn under_runs_count_only_while_feeding() {
        let (mut r, mut cb) = callback(48_000, 1, 96);
        r.start_generation(1);
        let mut out = Vec::new();
        // Feeding, but nothing of this generation has started yet: not an under-run (start of a feed).
        r.ctrl.feeding.store(true, Ordering::Release);
        fill(&mut cb, 480, 1, &mut out);
        assert_eq!(r.underruns().frames, 0);
        assert!(r.push(tag(1, 0, 100.0), &[0.1; 100]));
        fill(&mut cb, 480, 1, &mut out);
        assert_eq!(r.underruns().frames, 380, "the ring ran dry mid-feed");
        // The player stopped feeding (drained end of a read): an empty ring is expected.
        r.ctrl.feeding.store(false, Ordering::Release);
        fill(&mut cb, 480, 1, &mut out);
        assert_eq!(r.underruns().frames, 380);
        // A new generation starts counting afresh only after its first block.
        r.ctrl.feeding.store(true, Ordering::Release);
        r.start_generation(2);
        fill(&mut cb, 480, 1, &mut out);
        assert_eq!(r.underruns().frames, 380);
    }

    #[test]
    fn volume_changes_ramp() {
        let (mut r, mut cb) = callback(48_000, 1, 96);
        r.start_generation(1);
        for b in 0..4 {
            assert!(r.push(tag(1, 0, (b + 1) as f64 * 480.0), &[0.5; 480]));
        }
        let mut out = Vec::new();
        fill(&mut cb, 480 + 96, 1, &mut out);
        // Volume 1 → 0 and back: 10 ms per full-scale ramp, never a step.
        r.set_volume(0.0);
        fill(&mut cb, 480, 1, &mut out);
        r.set_volume(1.0);
        fill(&mut cb, 480, 1, &mut out);
        let after_fade_in = &out[96..];
        assert!(max_step(after_fade_in) <= 0.5 / 480.0 + 1e-6, "{}", max_step(after_fade_in));
        assert!(out[480 + 96 + 479].abs() < 1e-6, "reaches the target within 10 ms");
    }

    #[test]
    fn handed_position_and_queue_follow_the_callback() {
        let (mut r, mut cb) = callback(1000, 1, 2);
        r.start_generation(5);
        assert_eq!(r.handed_tag(), None);
        // Blocks of 10 frames covering source frames 0–20 of chunk 2 (the first block's start is unknown) and a join
        // into chunk 3.
        assert!(r.push(tag(5, 2, 10.0), &[0.2; 10]));
        assert!(r.push(tag(5, 2, 20.0), &[0.2; 10]));
        assert!(r.push(tag(5, 3, 4.0), &[0.2; 10]));
        assert_eq!(r.queued_frames(), 30);
        let mut out = Vec::new();
        fill(&mut cb, 13, 1, &mut out);
        assert_eq!(r.queued_frames(), 17);
        assert_eq!(r.handed_tag(), Some(tag(5, 2, 13.0)));
        fill(&mut cb, 7, 1, &mut out);
        r.drain_reports(u64::MAX);
        assert_eq!(r.handed_tag(), Some(tag(5, 2, 20.0)), "a finished block: its end");
        fill(&mut cb, 5, 1, &mut out);
        assert_eq!(r.handed_tag(), Some(tag(5, 3, 2.0)));
        // A flush forgets the old generation's progress at once.
        r.start_generation(6);
        assert_eq!(r.handed_tag(), None);
        assert_eq!(r.queued_frames(), 0);
    }

    /// Opens the real default device. Needs a Windows desktop with an output device:
    /// `cargo test -p vox-audio -- --ignored real_output_opens`.
    #[test]
    #[ignore]
    fn real_output_opens_and_plays_keep_alive() {
        let mut out = AudioOutput::open().expect("open the default output");
        assert!(out.sample_rate() > 0 && out.channels() > 0);
        out.set_keep_alive(true);
        out.resume();
        std::thread::sleep(Duration::from_millis(300));
        assert!(out.rendered_frames() > 0);
        let ch = out.channels() as usize;
        let block = vec![0.0f32; out.sample_rate() as usize / 100 * ch];
        assert!(out.push(BlockTag { generation: 0, chunk: 0, source_frame_end: 1.0 }, &block));
        std::thread::sleep(Duration::from_millis(400));
        out.refresh();
        assert_eq!(out.played_tag().source_frame_end, 1.0);
        out.suspend();
        assert!(!out.is_lost());
    }
}
