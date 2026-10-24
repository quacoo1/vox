//! The art cache (DESIGN.md "Art"). Images are rendered by vox-art at 2× on one background thread (a 220×164
//! thumbnail takes ~10 ms, the backdrop < 100 ms) and turned into `slint::Image`s on the UI thread.
//!
//! - Voice art: seed = voice id (Voices, Settings, the widget, History rows, Home). Source art: seed = app name.
//! - Sizes: `Large` 110×82 → 220×164; `Compact` 44×44 → 88×88; `Dot` 16×16 → 32×32; `Backdrop` = window size × scale
//!   (capped at 1 600 px on the long side: the backdrop is out of focus and Slint scales it up).
//! - A miss returns a placeholder (a flat image of the seed's palette accent) and queues the render; when it completes,
//!   the worker posts [`crate::ui::art_ready`] to the event loop (one post per batch), which moves the finished
//!   renders into the cache ([`ArtCache::take_ready`]) and rebuilds the visible models.
//! - The cache is bounded (oldest entries evicted first); the backdrop keeps only the latest size, and a stale backdrop
//!   of the same theme is shown while a new size renders (otherwise an empty image: `main-window.slint` then draws its
//!   own soft placeholder).
//! - Under `cfg(test)` the default cache has no worker (controller tests get placeholders and spawn no threads);
//!   [`ArtCache::with_worker`] turns it on explicitly.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crossbeam_channel::{unbounded, Sender};
use parking_lot::Mutex;

/// Art slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArtSize {
    Large,
    Compact,
    Dot,
}

impl ArtSize {
    /// Render size in pixels (2× the logical slot).
    pub fn pixels(self) -> (u32, u32) {
        match self {
            ArtSize::Large => (220, 164),
            ArtSize::Compact => (88, 88),
            ArtSize::Dot => (32, 32),
        }
    }
}

/// How many seed images the cache keeps (≈ 30 MB of `Large` art at most).
const MAX_IMAGES: usize = 240;
/// The backdrop's longest rendered side.
const BACKDROP_MAX_SIDE: u32 = 1600;

enum Job {
    Art { seed: String, size: ArtSize },
    Backdrop { width: u32, height: u32, dark: bool },
}

enum Done {
    Art { seed: String, size: ArtSize, art: vox_art::Art },
    Backdrop { key: (u32, u32, bool), art: vox_art::Art },
}

/// What the worker shares with the UI thread.
struct Mailbox {
    done: Mutex<Vec<Done>>,
    /// An `art_ready` post is on its way (one per batch).
    posted: AtomicBool,
}

/// UI-thread cache.
pub struct ArtCache {
    images: HashMap<(String, ArtSize), slint::Image>,
    order: VecDeque<(String, ArtSize)>,
    pending: HashSet<(String, ArtSize)>,
    accents: HashMap<String, slint::Image>,
    backdrop: Option<((u32, u32, bool), slint::Image)>,
    backdrop_pending: Option<(u32, u32, bool)>,
    worker: Option<Sender<Job>>,
    mailbox: Arc<Mailbox>,
    worker_enabled: bool,
}

impl Default for ArtCache {
    fn default() -> Self {
        Self::with_worker(!cfg!(test))
    }
}

impl std::fmt::Debug for ArtCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArtCache").field("images", &self.images.len()).field("pending", &self.pending.len()).finish()
    }
}

impl ArtCache {
    /// A cache whose misses are rendered on a background thread (`true`) or stay placeholders (`false`).
    pub fn with_worker(enabled: bool) -> Self {
        ArtCache {
            images: HashMap::new(),
            order: VecDeque::new(),
            pending: HashSet::new(),
            accents: HashMap::new(),
            backdrop: None,
            backdrop_pending: None,
            worker: None,
            mailbox: Arc::new(Mailbox { done: Mutex::new(Vec::new()), posted: AtomicBool::new(false) }),
            worker_enabled: enabled,
        }
    }

    /// The image for `seed` at `size` (placeholder on a miss; the render is queued).
    pub fn image(&mut self, seed: &str, size: ArtSize) -> slint::Image {
        let key = (seed.to_string(), size);
        if let Some(img) = self.images.get(&key) {
            return img.clone();
        }
        if !self.pending.contains(&key) && self.queue(Job::Art { seed: seed.to_string(), size }) {
            self.pending.insert(key);
        }
        self.placeholder(seed)
    }

    /// The backdrop for a window of `width`×`height` physical pixels.
    pub fn backdrop(&mut self, width: u32, height: u32, dark: bool) -> slint::Image {
        let key = backdrop_key(width, height, dark);
        if let Some((k, img)) = &self.backdrop {
            if *k == key {
                return img.clone();
            }
        }
        if self.backdrop_pending != Some(key) && self.queue(Job::Backdrop { width: key.0, height: key.1, dark }) {
            self.backdrop_pending = Some(key);
        }
        match &self.backdrop {
            // A stale size of the same theme stays up while the new one renders.
            Some(((_, _, d), img)) if *d == dark => img.clone(),
            // An empty image: the main window draws its own soft placeholder while the backdrop is missing.
            _ => slint::Image::default(),
        }
    }

    /// Moves finished renders into the cache (UI thread). True when anything new arrived (the caller rebuilds).
    pub fn take_ready(&mut self) -> bool {
        self.mailbox.posted.store(false, Ordering::SeqCst);
        let done: Vec<Done> = std::mem::take(&mut *self.mailbox.done.lock());
        if done.is_empty() {
            return false;
        }
        for d in done {
            match d {
                Done::Art { seed, size, art } => {
                    let key = (seed, size);
                    self.pending.remove(&key);
                    if art.rgba.is_empty() {
                        continue;
                    }
                    if self.images.insert(key.clone(), to_image(&art)).is_none() {
                        self.order.push_back(key);
                    }
                    while self.order.len() > MAX_IMAGES {
                        if let Some(old) = self.order.pop_front() {
                            self.images.remove(&old);
                        }
                    }
                }
                Done::Backdrop { key, art } => {
                    if self.backdrop_pending == Some(key) {
                        self.backdrop_pending = None;
                    }
                    if !art.rgba.is_empty() {
                        self.backdrop = Some((key, to_image(&art)));
                    }
                }
            }
        }
        true
    }

    /// Whether a render is outstanding (or finished but not taken yet). The shots harness waits for this before a
    /// capture.
    pub fn busy(&self) -> bool {
        !self.pending.is_empty() || self.backdrop_pending.is_some()
    }

    /// The flat accent image of a seed (cached). The colour is the seed's palette accent
    /// (`vox_art::palette_for(seed).accent()`, no rendering on the UI thread; `vox_art::accent` would render a sample).
    fn placeholder(&mut self, seed: &str) -> slint::Image {
        if let Some(img) = self.accents.get(seed) {
            return img.clone();
        }
        let img = flat_image(vox_art::palette_for(seed).accent());
        if self.accents.len() >= MAX_IMAGES * 2 {
            self.accents.clear();
        }
        self.accents.insert(seed.to_string(), img.clone());
        img
    }

    /// Sends a job to the worker (spawned on first use). False when there is no worker.
    fn queue(&mut self, job: Job) -> bool {
        if !self.worker_enabled {
            return false;
        }
        if self.worker.is_none() {
            self.worker = spawn_worker(self.mailbox.clone());
            if self.worker.is_none() {
                self.worker_enabled = false;
                return false;
            }
        }
        match self.worker.as_ref().map(|w| w.send(job)) {
            Some(Ok(())) => true,
            _ => {
                self.worker = None;
                self.worker_enabled = false;
                false
            }
        }
    }
}

/// The backdrop render size: the window size capped at [`BACKDROP_MAX_SIDE`] on the long side (pure; tested).
pub fn backdrop_key(width: u32, height: u32, dark: bool) -> (u32, u32, bool) {
    let (w, h) = (width.max(1), height.max(1));
    let long = w.max(h);
    if long <= BACKDROP_MAX_SIDE {
        return (w, h, dark);
    }
    let f = BACKDROP_MAX_SIDE as f64 / long as f64;
    (((w as f64 * f).round() as u32).max(1), ((h as f64 * f).round() as u32).max(1), dark)
}

/// A 2×2 image of one colour (Slint stretches it).
fn flat_image(rgb: [u8; 3]) -> slint::Image {
    let px = [rgb[0], rgb[1], rgb[2], 255];
    let rgba: Vec<u8> = px.iter().copied().cycle().take(16).collect();
    super::convert::rgba_image(&rgba, 2, 2).unwrap_or_default()
}

fn spawn_worker(mailbox: Arc<Mailbox>) -> Option<Sender<Job>> {
    let (tx, rx) = unbounded::<Job>();
    let spawned = std::thread::Builder::new().name("vox-art".into()).spawn(move || {
        let mut panic_logged = false;
        for job in rx {
            let rendered = std::panic::catch_unwind(move || match job {
                Job::Art { seed, size } => {
                    let (w, h) = size.pixels();
                    let art = vox_art::thumbnail(&seed, w, h);
                    Done::Art { seed, size, art }
                }
                Job::Backdrop { width, height, dark } => {
                    Done::Backdrop { key: (width, height, dark), art: vox_art::backdrop(width, height, dark) }
                }
            });
            match rendered {
                Ok(done) => mailbox.done.lock().push(done),
                Err(_) => {
                    // The image keeps its placeholder (it stays "pending", so it is not asked for again).
                    if !panic_logged {
                        panic_logged = true;
                        tracing::error!(target: "ui", "art render panicked; images keep their placeholder colour");
                    }
                    continue;
                }
            }
            if !mailbox.posted.swap(true, Ordering::SeqCst) {
                // Fails only when there is no event loop (tests, after quit): the renders wait in the mailbox.
                if slint::invoke_from_event_loop(crate::ui::art_ready).is_err() {
                    mailbox.posted.store(false, Ordering::SeqCst);
                }
            }
        }
    });
    match spawned {
        Ok(_) => Some(tx),
        Err(e) => {
            tracing::warn!(target: "ui", "art worker could not start: {e}");
            None
        }
    }
}

/// Converts rendered art to a Slint image (UI thread).
pub fn to_image(art: &vox_art::Art) -> slint::Image {
    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(&art.rgba, art.width, art.height);
    slint::Image::from_rgba8(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn dims(img: &slint::Image) -> (u32, u32) {
        let s = img.size();
        (s.width, s.height)
    }

    #[test]
    fn misses_return_the_accent_placeholder_without_a_worker() {
        let mut cache = ArtCache::default();
        let img = cache.image("voice-1", ArtSize::Large);
        assert_eq!(dims(&img), (2, 2));
        assert!(!cache.busy(), "no worker in tests by default");
        assert!(!cache.take_ready());
        let backdrop = cache.backdrop(1180, 780, true);
        assert_eq!(dims(&backdrop), (0, 0), "the window's own placeholder shows");
    }

    #[test]
    fn the_worker_fills_the_cache() {
        let mut cache = ArtCache::with_worker(true);
        let _ = cache.image("voice-1", ArtSize::Dot);
        let _ = cache.image("voice-1", ArtSize::Dot); // not queued twice
        let _ = cache.backdrop(320, 200, false);
        assert!(cache.busy());
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut arrived = false;
        while cache.busy() && Instant::now() < deadline {
            arrived |= cache.take_ready();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!cache.busy());
        assert!(arrived);
        assert_eq!(dims(&cache.image("voice-1", ArtSize::Dot)), (32, 32));
        assert_eq!(dims(&cache.backdrop(320, 200, false)), (320, 200));
        // Another size of the same theme shows the stale backdrop meanwhile.
        assert_eq!(dims(&cache.backdrop(640, 400, false)), (320, 200));
    }

    #[test]
    fn backdrop_sizes_are_capped() {
        assert_eq!(backdrop_key(1180, 780, true), (1180, 780, true));
        assert_eq!(backdrop_key(3840, 2160, false), (1600, 900, false));
        assert_eq!(backdrop_key(0, 0, true), (1, 1, true));
        assert_eq!(ArtSize::Large.pixels(), (220, 164));
    }
}
