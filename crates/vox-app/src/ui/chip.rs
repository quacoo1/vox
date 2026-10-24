//! The chip controller (shell.md §6.5.2, ui-settings-widget.md §7): `ChipShow{x, y, mode}` (physical px from the
//! selection service; ignored, with no bounds reported, until `AppUi::overlays_ready`) → cancel a pending hide, set
//! mode + label + voice-art dot, bump `show-key`, place with `vox_core::geometry::place_chip` (DIP of the target
//! monitor, converted back to physical; a DPI hop first moves the hidden window onto the target monitor, see
//! `windowing::chip_window`), show through Slint, `assert_topmost(raise)`, report the real bounds to the selection
//! service. `ChipHide` → bounds `None` at once, `hiding = true` (exit animation), click-through on (winit
//! `set_cursor_hittest(false)`), hide after 140 ms unless shown again. A press: left → hide +
//! `selection.chip_activated()`; other (incl. a right-button press) → hide + `selection.chip_dismissed()`; presses
//! while hiding are ignored ([`PressGate`], tested). Sizes are remembered per mode (from `size-changed`). The chip is
//! always dark (D-USW-20).
//!
//! ## How `AppUi` drives the chip (integration contract of this module)
//!
//! - `AppUi::create`: [`ChipController::init`] once the chip window exists (pushes the initial data: context mode,
//!   `Read with Vox`, the hidden exit look — so the window's first `size-changed` measures the real context label).
//! - `UiUpdate::ChipShow{x, y, mode}` → [`ChipController::show`] **only when `overlays_ready`** (otherwise drop it:
//!   no data, no bounds).
//! - `UiUpdate::ChipHide` → [`ChipController::hide`] (ignored when not shown or already hiding).
//! - `ChipWindow.pressed(left)` → [`ChipController::pressed`]; `ChipWindow.size-changed(w, h)` →
//!   [`ChipController::size_changed`] (re-places a visible chip whose pill changed size, e.g. the first show of a mode).
//!
//! The controller pushes `ChipWindow.data` itself and owns the 140 ms exit timer (it holds only a weak window handle
//! and a show generation, so a newer show cancels a pending hide).

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use slint::ComponentHandle;
use vox_core::app::ChipMode;
use vox_core::constants::{CHIP_EXIT_MS, CHIP_SIZE_MAX, CHIP_SIZE_MIN};
use vox_core::geometry::sanitize_size;
use vox_core::settings::Settings;
use vox_win::selection::SelectionService;

use super::art::{ArtCache, ArtSize};
use super::generated::{ChipData, ChipKind, ChipWindow};

/// What a chip press does (pure).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressGate {
    Activate,
    Dismiss,
    Ignore,
}

/// `pressed(left)` while `hiding` → `Ignore`; left → `Activate`; other → `Dismiss`.
pub fn press_gate(left: bool, hiding: bool) -> PressGate {
    if hiding {
        PressGate::Ignore
    } else if left {
        PressGate::Activate
    } else {
        PressGate::Dismiss
    }
}

/// The Slint kind of a chip mode.
pub fn chip_kind(mode: ChipMode) -> ChipKind {
    match mode {
        ChipMode::Context => ChipKind::Context,
        ChipMode::Selection => ChipKind::Selection,
    }
}

fn mode_index(mode: ChipMode) -> usize {
    match mode {
        ChipMode::Context => 0,
        ChipMode::Selection => 1,
    }
}

/// Where to place the chip: mode, cursor (physical px) and pill size (DIP).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChipPlacement {
    pub mode: ChipMode,
    pub cursor: (i32, i32),
    pub size: (f64, f64),
}

/// One started exit: the 140 ms timer hides the window only while no show (or newer exit) has happened since.
#[derive(Debug, Clone, PartialEq)]
pub struct ExitToken {
    latest: Rc<Cell<u64>>,
    generation: u64,
}

impl ExitToken {
    /// Whether this exit is still the latest (no show since).
    pub fn is_current(&self) -> bool {
        self.latest.get() == self.generation
    }
}

/// Chip state (module docs).
pub struct ChipController {
    pub mode: ChipMode,
    /// Hidden or in the exit animation (presses ignored). `true` before the first show.
    pub hiding: bool,
    /// The cursor of the last show (physical px).
    pub cursor: Option<(i32, i32)>,
    /// The pill size per mode (DIP; context, selection), from `size-changed` (defaults until reported).
    pub sizes: [(f64, f64); 2],
    show_key: i32,
    art: slint::Image,
    /// Bumped by every show and every exit start: a pending exit only hides the window while it still matches.
    generation: Rc<Cell<u64>>,
    exit_timer: slint::Timer,
}

impl Default for ChipController {
    fn default() -> Self {
        Self {
            mode: ChipMode::Context,
            hiding: true,
            cursor: None,
            sizes: [ChipMode::Context.default_size(), ChipMode::Selection.default_size()],
            show_key: 0,
            art: slint::Image::default(),
            generation: Rc::new(Cell::new(0)),
            exit_timer: slint::Timer::default(),
        }
    }
}

impl ChipController {
    /// The pill size used to place `mode` (DIP).
    pub fn size(&self, mode: ChipMode) -> (f64, f64) {
        self.sizes[mode_index(mode)]
    }

    /// `ChipData` for the window.
    pub fn data(&self) -> ChipData {
        ChipData {
            kind: chip_kind(self.mode),
            label: self.mode.label().into(),
            art: self.art.clone(),
            hiding: self.hiding,
            show_key: self.show_key,
        }
    }

    // ── pure state transitions (tested) ──

    /// A show: cancels a pending hide, sets the mode, bumps `show-key` (the enter animation replays on every show,
    /// even when already shown).
    pub fn plan_show(&mut self, mode: ChipMode, x: i32, y: i32) -> ChipPlacement {
        self.generation.set(self.generation.get().wrapping_add(1));
        self.mode = mode;
        self.hiding = false;
        self.cursor = Some((x, y));
        self.show_key = self.show_key.wrapping_add(1);
        ChipPlacement { mode, cursor: (x, y), size: self.size(mode) }
    }

    /// A hide: the exit that starts now; `None` when not shown (or already hiding).
    pub fn plan_hide(&mut self) -> Option<ExitToken> {
        if self.hiding {
            return None;
        }
        self.hiding = true;
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        Some(ExitToken { latest: Rc::clone(&self.generation), generation })
    }

    /// A press: the gate, and the exit it starts (anything but `Ignore` starts one).
    pub fn plan_press(&mut self, left: bool) -> (PressGate, Option<ExitToken>) {
        let gate = press_gate(left, self.hiding);
        let exit = if gate == PressGate::Ignore { None } else { self.plan_hide() };
        (gate, exit)
    }

    /// A size report for the current mode (sanitized 32×24 … 480×160; zero or duplicate reports change nothing):
    /// `Some(placement)` when a visible chip must be re-placed with the new size.
    pub fn plan_size(&mut self, width: f64, height: f64) -> Option<ChipPlacement> {
        if !(width > 0.0 && height > 0.0) {
            return None;
        }
        let size = sanitize_size(width.ceil(), height.ceil(), CHIP_SIZE_MIN, CHIP_SIZE_MAX)?;
        let size = (size.width, size.height);
        let slot = &mut self.sizes[mode_index(self.mode)];
        if *slot == size {
            return None;
        }
        *slot = size;
        match (self.hiding, self.cursor) {
            (false, Some(cursor)) => Some(ChipPlacement { mode: self.mode, cursor, size }),
            _ => None,
        }
    }

    // ── native side ──

    /// Pushes the initial data (once, from `AppUi::create`).
    pub fn init(&mut self, window: &ChipWindow, art: &mut ArtCache, settings: &Settings) {
        self.art = art.image(&settings.voice_id, ArtSize::Dot);
        window.set_data(self.data());
    }

    /// Re-resolves the voice-art dot (`AppUi::on_art_ready`, and `UiUpdate::Settings`, which also queues the render
    /// for a new voice before the next show). A shown chip gets the new data when the image changed; a hidden one
    /// picks it up at its next show.
    pub fn refresh_art(&mut self, window: &ChipWindow, art: &mut ArtCache, settings: &Settings) {
        let image = art.image(&settings.voice_id, ArtSize::Dot);
        if image == self.art {
            return;
        }
        self.art = image;
        if !self.hiding {
            window.set_data(self.data());
        }
    }

    /// `UiUpdate::ChipShow{x, y, mode}` (only once `overlays_ready`; module docs).
    #[allow(clippy::too_many_arguments)] // the window, the service, the art and the settings are `AppUi`'s fields
    pub fn show(&mut self, window: &ChipWindow, selection: &SelectionService, art: &mut ArtCache, settings: &Settings, mode: ChipMode, x: i32, y: i32) {
        self.exit_timer.stop();
        let placement = self.plan_show(mode, x, y);
        self.art = art.image(&settings.voice_id, ArtSize::Dot);
        window.set_data(self.data());
        self.place(window, selection, placement);
    }

    fn place(&mut self, window: &ChipWindow, selection: &SelectionService, p: ChipPlacement) {
        let rect = crate::windowing::chip_window::show_at(window, p.mode, p.cursor, p.size);
        if rect.is_none() {
            tracing::debug!(target: "window", "chip could not be placed");
        }
        selection.set_chip_bounds(rect);
    }

    /// `UiUpdate::ChipHide` (and every press): bounds `None` at once, the exit look, click-through, and the window
    /// hides 140 ms later unless shown again. Ignored when not shown or already hiding.
    pub fn hide(&mut self, window: &ChipWindow, selection: &SelectionService) {
        if let Some(exit) = self.plan_hide() {
            self.begin_exit(window, selection, exit);
        }
    }

    /// The native side of a started exit ([`ChipController::plan_hide`] / [`ChipController::plan_press`]).
    fn begin_exit(&mut self, window: &ChipWindow, selection: &SelectionService, exit: ExitToken) {
        selection.set_chip_bounds(None);
        window.set_data(self.data());
        crate::windowing::chip_window::begin_hide(window);
        let weak = window.as_weak();
        self.exit_timer.start(slint::TimerMode::SingleShot, Duration::from_millis(CHIP_EXIT_MS), move || {
            super::shell::guarded("chip exit", || {
                if exit.is_current() {
                    if let Some(w) = weak.upgrade() {
                        crate::windowing::chip_window::finish_hide(&w);
                    }
                }
            });
        });
    }

    /// `ChipWindow.pressed(left)`: left → hide + `chip_activated()`; other → hide + `chip_dismissed()`; ignored while
    /// hiding.
    pub fn pressed(&mut self, window: &ChipWindow, selection: &SelectionService, left: bool) -> PressGate {
        let (gate, exit) = self.plan_press(left);
        if let Some(exit) = exit {
            self.begin_exit(window, selection, exit);
        }
        match gate {
            PressGate::Activate => selection.chip_activated(),
            PressGate::Dismiss => selection.chip_dismissed(),
            PressGate::Ignore => {}
        }
        gate
    }

    /// `ChipWindow.size-changed(w, h)` (logical px): remembered for the current mode; a visible chip is re-placed.
    pub fn size_changed(&mut self, window: &ChipWindow, selection: &SelectionService, width: f32, height: f32) {
        if let Some(p) = self.plan_size(f64::from(width), f64::from(height)) {
            self.place(window, selection, p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn press_gates() {
        assert_eq!(press_gate(true, false), PressGate::Activate);
        assert_eq!(press_gate(false, false), PressGate::Dismiss, "a right press dismisses");
        assert_eq!(press_gate(true, true), PressGate::Ignore);
        assert_eq!(press_gate(false, true), PressGate::Ignore);
    }

    #[test]
    fn starts_hidden_in_context_mode() {
        let c = ChipController::default();
        assert!(c.hiding);
        let d = c.data();
        assert_eq!((d.kind, d.label.as_str(), d.hiding, d.show_key), (ChipKind::Context, "Read with Vox", true, 0));
        assert_eq!(c.size(ChipMode::Context), (153.0, 38.0));
        assert_eq!(c.size(ChipMode::Selection), (82.0, 38.0));
    }

    #[test]
    fn show_hide_and_presses() {
        let mut c = ChipController::default();
        // Presses before any show are ignored; a hide while hidden does nothing.
        assert_eq!(c.plan_press(true), (PressGate::Ignore, None));
        assert_eq!(c.plan_hide(), None);
        let p = c.plan_show(ChipMode::Selection, 300, 600);
        assert_eq!(p, ChipPlacement { mode: ChipMode::Selection, cursor: (300, 600), size: (82.0, 38.0) });
        let d = c.data();
        assert_eq!((d.kind, d.label.as_str(), d.hiding, d.show_key), (ChipKind::Selection, "Read", false, 1));
        // Every show replays the enter animation, even when already shown.
        c.plan_show(ChipMode::Context, 310, 600);
        assert_eq!(c.data().show_key, 2);
        assert_eq!(c.data().label, "Read with Vox");
        // A left press activates and starts the exit; presses during the exit are ignored.
        let (gate, exit) = c.plan_press(true);
        assert_eq!(gate, PressGate::Activate);
        assert!(exit.expect("a press starts the exit").is_current());
        assert!(c.hiding && c.data().hiding);
        assert_eq!(c.plan_press(false), (PressGate::Ignore, None));
        assert_eq!(c.plan_hide(), None, "already hiding");
        // A right press dismisses.
        c.plan_show(ChipMode::Context, 1, 1);
        assert_eq!(c.plan_press(false).0, PressGate::Dismiss);
    }

    #[test]
    fn a_show_cancels_a_pending_exit() {
        let mut c = ChipController::default();
        c.plan_show(ChipMode::Context, 0, 0);
        let g = c.plan_hide().expect("exit");
        assert!(g.is_current());
        c.plan_show(ChipMode::Context, 5, 5);
        assert!(!g.is_current(), "the old exit must not hide the new chip");
        let g2 = c.plan_hide().expect("exit");
        assert_ne!(g, g2);
        assert!(g2.is_current());
    }

    #[test]
    fn sizes_per_mode_and_replacement() {
        let mut c = ChipController::default();
        // Hidden: remembered, nothing to re-place.
        assert_eq!(c.plan_size(140.0, 34.0), None);
        assert_eq!(c.size(ChipMode::Context), (140.0, 34.0));
        // Zero / non-finite reports are ignored.
        assert_eq!(c.plan_size(0.0, 34.0), None);
        assert_eq!(c.plan_size(f64::NAN, 34.0), None);
        assert_eq!(c.size(ChipMode::Context), (140.0, 34.0));
        // Shown in selection mode: the first report of that mode re-places the chip at the same cursor.
        c.plan_show(ChipMode::Selection, 300, 600);
        let p = c.plan_size(64.0, 34.0).expect("re-place");
        assert_eq!(p, ChipPlacement { mode: ChipMode::Selection, cursor: (300, 600), size: (64.0, 34.0) });
        assert_eq!(c.plan_size(64.0, 34.0), None, "duplicate");
        assert_eq!(c.size(ChipMode::Context), (140.0, 34.0), "the other mode keeps its size");
        // Clamped to 480×160.
        let p = c.plan_size(900.0, 34.0).expect("re-place");
        assert_eq!(p.size, (480.0, 34.0));
    }
}
