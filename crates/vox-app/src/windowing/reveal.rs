//! First-frame reveal of the widget: render off-screen, then move into place.
//!
//! A hidden window keeps whatever its swap chain last presented (the previous read's card, the idle card drawn at the
//! pre-show size, a black buffer after a resize). Slint renders the new data and size only on the next event-loop
//! turn after `show()`, so a window shown in place displays the stale surface for one or more frames.
//!
//! Instead a hidden widget is shown **parked** at [`super::OFFSCREEN`] with its final size ([`begin`]). The window's
//! winit dispatcher sees each `RedrawRequested` before Slint draws and presents the frame synchronously in that same
//! dispatch ([`redraw_requested`]); for the first one at the target size, a zero timer — which runs after that
//! present — moves the window to its target in one `set_bounds`, and DWM composes the new frame there. A fallback
//! timer moves it after [`REVEAL_FALLBACK_MS`] whatever happens, and hiding cancels, so the widget can never stay
//! parked. While parked, a resize or a re-placement only changes the target ([`retarget`]).
//!
//! Why not Slint's rendering notifier: with the Skia renderer on wgpu it is never called unless slint's
//! `unstable-wgpu-30` feature is on (`WGPUSurface::with_graphics_api` is compiled only then) — measured: it never
//! fired. Why not `DWMWA_CLOAK` (with that notifier): the same measurement found nothing to uncloak on. Off-screen
//! windows do render (the startup pre-show relies on it).
//!
//! Parking moves the window off every monitor, so Windows may give it the DPI of the monitor nearest
//! [`super::OFFSCREEN`]; the caller parks only when that monitor's scale equals the target monitor's ([`can_park`]),
//! else it shows in place as before.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use vox_win::window::{MonitorInfo, PhysRect};

/// The latest a parked window waits for its first frame.
pub const REVEAL_FALLBACK_MS: u64 = 250;
/// Frames rendered at another size before the reveal stops waiting for the expected size.
const MAX_MISSIZED_FRAMES: u32 = 2;

/// Per-window reveal state.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Reveal {
    /// The rendering notifier is installed (else shows are never parked).
    pub installed: bool,
    /// Parked, waiting for the first frame.
    pub pending: bool,
    /// Bumped by every begin / cancel (a stale timer does nothing).
    pub generation: u64,
    /// Where the window goes once its first frame was presented.
    pub target: Option<PhysRect>,
    /// Frames rendered at another size since the reveal began.
    pub missized: u32,
    /// When the reveal began (for the debug log).
    pub began: Option<Instant>,
}

impl Reveal {
    /// A show of a hidden window towards `target`: `Some(generation)` when it must be parked now.
    pub fn begin(&mut self, target: PhysRect) -> Option<u64> {
        self.generation = self.generation.wrapping_add(1);
        if !self.installed {
            self.pending = false;
            self.target = None;
            return None;
        }
        self.pending = true;
        self.target = Some(target);
        self.missized = 0;
        self.began = Some(Instant::now());
        Some(self.generation)
    }

    /// The target changed while parked (resize, re-placement): true when it was taken.
    pub fn retarget(&mut self, target: PhysRect) -> bool {
        if !self.pending {
            return false;
        }
        self.target = Some(target);
        self.missized = 0;
        true
    }

    /// A frame was rendered at `size` (physical px): `Some(generation)` when the window can be moved into place (on
    /// the next turn).
    pub fn frame(&mut self, size: Option<(u32, u32)>) -> Option<u64> {
        if !self.pending {
            return None;
        }
        let expected = self.target.map(|t| (t.width.max(0) as u32, t.height.max(0) as u32));
        let fits = match (size, expected) {
            (Some(s), Some(e)) => s == e,
            _ => true,
        };
        if !fits {
            self.missized += 1;
            if self.missized < MAX_MISSIZED_FRAMES {
                return None;
            }
        }
        Some(self.generation)
    }

    /// The move for `generation` is due: the target when it is still the current reveal.
    pub fn finish(&mut self, generation: u64) -> Option<PhysRect> {
        if self.pending && self.generation == generation {
            self.pending = false;
            return self.target.take();
        }
        None
    }

    /// Hidden: nothing pending any more.
    pub fn cancel(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.pending = false;
        self.target = None;
    }
}

/// Whether a window can be parked off-screen without changing its DPI (pure; tested): the monitor nearest
/// [`super::OFFSCREEN`] has the target monitor's scale.
pub fn can_park(target: &MonitorInfo, monitors: &[MonitorInfo]) -> bool {
    super::dpi::nearest_monitor_phys(super::OFFSCREEN, super::OFFSCREEN, monitors).is_some_and(|m| (m.scale - target.scale).abs() < 1e-6)
}

/// The parked bounds for a target (same size, off every monitor).
pub fn parked(target: PhysRect) -> PhysRect {
    PhysRect { x: super::OFFSCREEN, y: super::OFFSCREEN, ..target }
}

thread_local! {
    static REVEALS: RefCell<HashMap<isize, Reveal>> = RefCell::new(HashMap::new());
}

fn with_reveal<R>(hwnd: isize, f: impl FnOnce(&mut Reveal) -> R) -> Option<R> {
    REVEALS.with(|r| r.try_borrow_mut().ok().map(|mut map| f(map.entry(hwnd).or_default())))
}

/// Enables the reveal for a window whose winit dispatcher forwards `RedrawRequested` to [`redraw_requested`] (once,
/// after its HWND exists).
pub fn install(hwnd: isize) {
    with_reveal(hwnd, |r| r.installed = true);
}

/// The window's `RedrawRequested` (from its winit dispatcher, before Slint draws and presents the frame in the same
/// dispatch): a parked window at its target size moves into place on the next turn.
pub fn redraw_requested(window: &slint::Window) {
    let Some(hwnd) = super::hwnd_of(window) else { return };
    let size = window.size();
    let Some(Some(generation)) = with_reveal(hwnd, |r| r.frame(Some((size.width, size.height)))) else { return };
    slint::Timer::single_shot(Duration::ZERO, move || finish(hwnd, generation, "first frame"));
}

/// Before `show()` of a hidden window: parks it at `target`'s size and moves it to `target` once its first frame was
/// presented. False when it cannot be parked (not installed): the caller places it itself.
pub fn begin(window: &slint::Window, hwnd: isize, target: PhysRect) -> bool {
    let Some(Some(generation)) = with_reveal(hwnd, |r| r.begin(target)) else { return false };
    vox_win::window::set_bounds(hwnd, parked(target));
    window.request_redraw();
    slint::Timer::single_shot(Duration::from_millis(REVEAL_FALLBACK_MS), move || finish(hwnd, generation, "fallback"));
    true
}

/// While parked: the new target (the parked window takes its size). False when not parked.
pub fn retarget(hwnd: isize, target: PhysRect) -> bool {
    if with_reveal(hwnd, |r| r.retarget(target)) != Some(true) {
        return false;
    }
    vox_win::window::set_bounds(hwnd, parked(target));
    true
}

/// The target of a parked window (`None` when not parked).
pub fn target(hwnd: isize) -> Option<PhysRect> {
    with_reveal(hwnd, |r| if r.pending { r.target } else { None }).flatten()
}

/// After a hide (or a failed show): nothing pending any more.
pub fn cancel(hwnd: isize) {
    with_reveal(hwnd, Reveal::cancel);
}

fn finish(hwnd: isize, generation: u64, why: &str) {
    let Some((Some(target), began)) = with_reveal(hwnd, |r| {
        let began = r.began;
        (r.finish(generation), began)
    }) else {
        return;
    };
    vox_win::window::set_bounds(hwnd, target);
    vox_win::window::assert_topmost(hwnd, false);
    let ms = began.map(|b| b.elapsed().as_millis()).unwrap_or(0);
    tracing::debug!(target: "window", "widget moved into place {ms} ms after the show ({why})");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(w: i32, h: i32) -> PhysRect {
        PhysRect { x: 1415, y: 743, width: w, height: h }
    }

    fn mon(x: i32, scale: f64) -> MonitorInfo {
        let r = PhysRect { x, y: 0, width: 1920, height: 1080 };
        MonitorInfo { bounds: r, work_area: r, scale, primary: x == 0, id: x as isize }
    }

    #[test]
    fn a_reveal_waits_for_a_frame_at_the_target_size() {
        let mut r = Reveal::default();
        assert_eq!(r.begin(rect(475, 248)), None, "not installed: shown in place");
        r.installed = true;
        let g = r.begin(rect(475, 248)).expect("park");
        assert_eq!(r.frame(Some((282, 103))), None, "a stale-size frame does not reveal");
        assert_eq!(r.frame(Some((475, 248))), Some(g));
        assert_eq!(r.finish(g), Some(rect(475, 248)));
        assert_eq!(r.finish(g), None, "only once");
        assert_eq!(r.frame(Some((475, 248))), None, "nothing pending");
    }

    #[test]
    fn retargets_missized_frames_and_stale_timers() {
        let mut r = Reveal { installed: true, ..Reveal::default() };
        let g = r.begin(rect(475, 248)).unwrap();
        // The card reported another size while parked: the frame at the new size is the one that counts.
        assert!(r.retarget(rect(475, 165)));
        assert_eq!(r.frame(Some((475, 248))), None);
        assert_eq!(r.frame(Some((475, 165))), Some(g));
        // Two missized frames reveal anyway.
        let g2 = r.begin(rect(100, 50)).unwrap();
        assert_eq!(r.frame(Some((99, 50))), None);
        assert_eq!(r.frame(Some((99, 50))), Some(g2));
        // A newer show supersedes older timers; a hide cancels.
        let g3 = r.begin(rect(100, 50)).unwrap();
        assert_eq!(r.finish(g2), None);
        r.cancel();
        assert_eq!(r.finish(g3), None);
        assert!(!r.retarget(rect(1, 1)), "nothing parked");
    }

    #[test]
    fn parking_never_changes_the_dpi() {
        // The monitor nearest the off-screen point is the left one here.
        let same = [mon(0, 1.5), mon(-1920, 1.5)];
        assert!(can_park(&same[0], &same));
        let mixed = [mon(0, 1.5), mon(-1920, 1.0)];
        assert!(!can_park(&mixed[0], &mixed), "the target is 150 %, the parking spot's nearest monitor 100 %");
        assert!(can_park(&mixed[1], &mixed));
        assert!(!can_park(&same[0], &[]));
        assert_eq!(parked(rect(475, 248)), PhysRect { x: crate::windowing::OFFSCREEN, y: crate::windowing::OFFSCREEN, width: 475, height: 248 });
    }
}
