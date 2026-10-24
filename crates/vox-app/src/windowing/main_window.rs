//! The main window (shell.md §6.2, §3.7, §5.6; BRIEF.md "Windows" and Product rulings "Main window"; ARCHITECTURE.md
//! §4.12, §6.5).
//!
//! - Created hidden with its theme, placement and maximized state set by the winit attributes hook
//!   (`windowing::window_attributes`) from `UiInit` — `window-state.json` when valid ([`initial_placement`]), else
//!   1180×780 DIP (min 940×640) centred on the primary monitor's work area. `DWMWA_CAPTION_COLOR` is applied in a
//!   `Timer::single_shot(0)` after the HWND exists and **before** the first show (`AppUi::create` shows it only after
//!   that when `UiInit::show_main`).
//! - `show(route)`: navigate, restore if minimized, show, raise, focus (`vox_win::window::bring_to_front`). Tray Open /
//!   OpenPage call it synchronously inside the tray event handler (the shell's foreground permission is short-lived).
//! - Close (title-bar X, Alt+F4, outside `WM_CLOSE` → winit `CloseRequested` → `on_close_requested`, always
//!   `KeepWindowShown`): `settings.app.closeToTray` (current value) → save the placement (`Command::SaveWindowState`)
//!   and hide; else commit a pending History undo batch and send `Command::Quit`.
//! - Placement is also saved 1 s after the last move/resize (debounced with a `slint::Timer`); the quit-time save is
//!   done synchronously by `AppUi::apply(Quit)` from [`current_state`] (`vox_win::window::normal_placement(hwnd)` +
//!   the monitor's scale; a window never shown this session — start minimized, `--hidden`, launch at login — keeps
//!   the saved maximized flag, which cannot be measured while hidden).
//! - Caption: `vox_win::window::set_caption_theme(hwnd, dark, backdrop rgb)` whenever the resolved theme changes
//!   (dark: `#05070b`; light: `#e9ecf1`, the `Theme.backdrop` token), so the native title bar reads as one surface
//!   with the backdrop (DESIGN.md "Screens").
//! - Reports `WindowEvent::MainState { visible, focused }` on show/hide/focus changes (toast routing, credits wake) and
//!   calls `ShellController::on_main_window_activity` (page wake).
//! - **One native event dispatcher** ([`on_created`] registers `on_winit_window_event` once): the hotkey recorder first
//!   (`ui::hotkey_recorder::on_winit_event`; `PreventDefault` while it consumes keys), then geometry tracking
//!   (`Moved`/`Resized` → the 1 s save debounce), then mouse X1/X2 (`MouseInput { state: Released, button: Back |
//!   Forward }` → `ShellController::back/forward`, ignored during onboarding; anywhere in the window, even over
//!   controls, which a Slint `TouchArea` could not guarantee).
//!
//! Implementation notes: the saved **outer** rectangle is applied exactly with `SetWindowPlacement` (hidden) once the
//! HWND exists ([`apply_placement`]); the saved maximized state is applied right after the first show (winit would
//! briefly show and activate a hidden window it maximizes). The dispatcher and the close handler live in `ui::mod`
//! (`AppUi::on_main_event`, `AppUi::on_main_close`) because they need the controllers.

use std::cell::Cell;

use slint::ComponentHandle;
use vox_store::window_state::WindowState;
use vox_win::window::{MonitorInfo, PhysRect};

use crate::ui::MainWindow;

/// Default size in DIP (shell.md §6.1).
pub const DEFAULT_SIZE: (f64, f64) = (1180.0, 780.0);
/// Minimum size in DIP (shell.md §6.1).
pub const MIN_SIZE: (f64, f64) = (940.0, 640.0);
/// A saved rect must overlap some work area by at least this much (physical px) to be reused.
pub const MIN_VISIBLE: i32 = 64;
/// Caption colours = the `Theme.backdrop` token (DESIGN.md).
pub const CAPTION_DARK: u32 = 0x05070b;
pub const CAPTION_LIGHT: u32 = 0xe9ecf1;

thread_local! {
    /// The saved maximized state, applied on the first show.
    static PENDING_MAXIMIZE: Cell<bool> = const { Cell::new(false) };
    /// The dispatcher and close handler are registered once.
    static HANDLERS: Cell<bool> = const { Cell::new(false) };
}

/// Overlap of two rects (width, height); zero when they do not overlap. In i64: a saved rect may hold any i32.
fn overlap(a: &PhysRect, b: &PhysRect) -> (i64, i64) {
    let span = |start: i32, len: i32| (i64::from(start), i64::from(start) + i64::from(len));
    let ((ax0, ax1), (bx0, bx1)) = (span(a.x, a.width), span(b.x, b.width));
    let ((ay0, ay1), (by0, by1)) = (span(a.y, a.height), span(b.y, b.height));
    ((ax1.min(bx1) - ax0.max(bx0)).max(0), (ay1.min(by1) - ay0.max(by0)).max(0))
}

fn scaled(v: f64, scale: f64) -> i32 {
    vox_core::js::js_round(v * scale) as i32
}

/// The default placement: 1180×780 DIP centred on the primary work area (≤ the work area).
fn default_placement(monitors: &[MonitorInfo]) -> PhysRect {
    let Some(m) = monitors.iter().find(|m| m.primary).or_else(|| monitors.first()) else {
        return PhysRect { x: 0, y: 0, width: DEFAULT_SIZE.0 as i32, height: DEFAULT_SIZE.1 as i32 };
    };
    let wa = m.work_area;
    let width = scaled(DEFAULT_SIZE.0, m.scale).min(wa.width).max(1);
    let height = scaled(DEFAULT_SIZE.1, m.scale).min(wa.height).max(1);
    PhysRect { x: wa.x + (wa.width - width) / 2, y: wa.y + (wa.height - height) / 2, width, height }
}

/// Where the window opens (pure; tested): the saved state if its rect overlaps some monitor's work area by at least
/// 64×64 physical px — rescaled by `monitor.scale / saved.scale` around its top-left when the monitor's scale
/// changed, size clamped to ≥ the minimum (940×640 DIP) and ≤ the work area — else the default size centred on the
/// primary work area. Returns `(normal rect, maximized)`.
pub fn initial_placement(saved: Option<&WindowState>, monitors: &[MonitorInfo]) -> (PhysRect, bool) {
    let Some(s) = saved else { return (default_placement(monitors), false) };
    let rect = PhysRect { x: s.x, y: s.y, width: s.width, height: s.height };
    // The monitor whose work area overlaps the saved rect the most (at least 64×64).
    let best = monitors
        .iter()
        .map(|m| (m, overlap(&rect, &m.work_area)))
        .filter(|(_, (w, h))| *w >= i64::from(MIN_VISIBLE) && *h >= i64::from(MIN_VISIBLE))
        .max_by_key(|(_, (w, h))| w.saturating_mul(*h));
    let Some((m, _)) = best else { return (default_placement(monitors), s.maximized) };
    let ratio = if s.scale > 0.0 && m.scale > 0.0 { m.scale / s.scale } else { 1.0 };
    let (mut width, mut height) = (rect.width, rect.height);
    if (ratio - 1.0).abs() > 1e-9 {
        width = vox_core::js::js_round(rect.width as f64 * ratio) as i32;
        height = vox_core::js::js_round(rect.height as f64 * ratio) as i32;
    }
    let wa = m.work_area;
    let min_w = scaled(MIN_SIZE.0, m.scale).min(wa.width);
    let min_h = scaled(MIN_SIZE.1, m.scale).min(wa.height);
    width = width.max(min_w).min(wa.width).max(1);
    height = height.max(min_h).min(wa.height).max(1);
    (PhysRect { x: rect.x, y: rect.y, width, height }, s.maximized)
}

/// Applies the startup placement to the hidden window (exact outer rect; maximized on the first show).
pub fn apply_placement(window: &MainWindow, placement: (PhysRect, bool)) {
    let (rect, maximized) = placement;
    if let Some(hwnd) = super::hwnd_of(window.window()) {
        if rect.width > 0 && rect.height > 0 {
            vox_win::window::set_normal_placement(hwnd, rect, false, false);
        }
    }
    PENDING_MAXIMIZE.with(|p| p.set(maximized));
}

/// The caption colour of a theme.
pub fn caption_rgb(dark: bool) -> u32 {
    if dark {
        CAPTION_DARK
    } else {
        CAPTION_LIGHT
    }
}

/// Applies the resolved theme to the native title bar (DWM) and to winit's own theme preference.
pub fn set_caption(window: &MainWindow, dark: bool) {
    use slint::winit_030::winit::window::Theme;
    if let Some(hwnd) = super::hwnd_of(window.window()) {
        vox_win::window::set_caption_theme(hwnd, dark, caption_rgb(dark));
    }
    super::with_winit(window.window(), |w| w.set_theme(Some(if dark { Theme::Dark } else { Theme::Light })));
}

/// Once the HWND exists (before the first show): caption colours, close handling, the native event dispatcher.
pub fn on_created(window: &MainWindow, dark: bool) {
    set_caption(window, dark);
    if HANDLERS.with(|h| h.replace(true)) {
        return;
    }
    use slint::winit_030::WinitWindowAccessor;
    window.window().on_winit_window_event(|_w, event| crate::ui::main_window_event(event));
    window.window().on_close_requested(|| {
        crate::ui::guard("main close", || crate::ui::with_app_later(|app| app.on_main_close()));
        slint::CloseRequestResponse::KeepWindowShown
    });
}

/// Shows and focuses the window (from the tray, a second instance, `ShowMain`).
pub fn show(window: &MainWindow) {
    if let Err(e) = window.show() {
        tracing::warn!(target: "window", "could not show the main window: {e}");
        return;
    }
    if PENDING_MAXIMIZE.with(|p| p.replace(false)) {
        window.window().set_maximized(true);
    }
    match super::hwnd_of(window.window()) {
        Some(hwnd) => vox_win::window::bring_to_front(hwnd),
        None => {
            if window.window().is_minimized() {
                window.window().set_minimized(false);
            }
        }
    }
}

/// Hides to the tray (after saving the placement).
pub fn hide(window: &MainWindow) {
    if let Err(e) = window.hide() {
        tracing::warn!(target: "window", "could not hide the main window: {e}");
    }
}

/// Whether the window is shown and not minimized.
pub fn is_visible(window: &MainWindow) -> bool {
    window.window().is_visible() && !window.window().is_minimized()
}

/// The current placement to persist (`None` before the HWND exists). A window never shown this session still carries
/// the saved maximized state (it cannot be measured on a hidden window; [`saved_maximized`]).
pub fn current_state(window: &MainWindow) -> Option<WindowState> {
    let hwnd = super::hwnd_of(window.window())?;
    let (rect, measured) = vox_win::window::normal_placement(hwnd)?;
    if rect.width <= 0 || rect.height <= 0 {
        return None;
    }
    let scale = vox_win::window::monitor_from_rect(rect).map(|m| m.scale).filter(|s| *s > 0.0).unwrap_or(1.0);
    let maximized = saved_maximized(measured, PENDING_MAXIMIZE.with(Cell::get));
    Some(WindowState { x: rect.x, y: rect.y, width: rect.width, height: rect.height, scale, maximized })
}

/// The maximized flag to save (pure; tested): measured, or still pending from startup (the window was never shown).
pub fn saved_maximized(measured: bool, pending: bool) -> bool {
    measured || pending
}

/// Whether the window has the keyboard focus right now (winit's state, updated synchronously by `WM_ACTIVATE` /
/// `WM_SETFOCUS` even while the app is busy and its `Focused` event cannot be handled); `None` before it exists.
pub fn has_focus(window: &MainWindow) -> Option<bool> {
    super::with_winit(window.window(), |w| w.has_focus())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(x: i32, y: i32, w: i32, h: i32, scale: f64, primary: bool) -> MonitorInfo {
        let r = PhysRect { x, y, width: w, height: h };
        MonitorInfo { bounds: r, work_area: PhysRect { height: h - 48, ..r }, scale, primary, id: x as isize }
    }

    fn saved(x: i32, y: i32, w: i32, h: i32, scale: f64, maximized: bool) -> WindowState {
        WindowState { x, y, width: w, height: h, scale, maximized }
    }

    #[test]
    fn default_is_centred_on_the_primary_work_area() {
        let ms = [mon(-1920, 0, 1920, 1080, 1.0, false), mon(0, 0, 2880, 1800, 1.5, true)];
        let (r, max) = initial_placement(None, &ms);
        assert_eq!((r.width, r.height), (1770, 1170));
        assert_eq!((r.x, r.y), ((2880 - 1770) / 2, (1752 - 1170) / 2));
        assert!(!max);
        // A small screen clamps the default to the work area.
        let small = [mon(0, 0, 1024, 700, 1.0, true)];
        let (r, _) = initial_placement(None, &small);
        assert_eq!((r.x, r.y, r.width, r.height), (0, 0, 1024, 652));
        // No monitors at all.
        assert_eq!(initial_placement(None, &[]).0.width, 1180);
    }

    #[test]
    fn a_visible_saved_rect_is_kept() {
        let ms = [mon(0, 0, 1920, 1080, 1.0, true)];
        let s = saved(100, 50, 1300, 900, 1.0, true);
        assert_eq!(initial_placement(Some(&s), &ms), (PhysRect { x: 100, y: 50, width: 1300, height: 900 }, true));
        // Too small → clamped up to the minimum; too big → clamped to the work area.
        let tiny = saved(100, 50, 300, 200, 1.0, false);
        assert_eq!(initial_placement(Some(&tiny), &ms).0, PhysRect { x: 100, y: 50, width: 940, height: 640 });
        let huge = saved(0, 0, 4000, 3000, 1.0, false);
        assert_eq!(initial_placement(Some(&huge), &ms).0, PhysRect { x: 0, y: 0, width: 1920, height: 1032 });
    }

    #[test]
    fn a_saved_rect_mostly_off_screen_falls_back_to_the_default() {
        let ms = [mon(0, 0, 1920, 1080, 1.0, true)];
        // Only 40 px visible horizontally.
        let s = saved(1880, 100, 1200, 800, 1.0, true);
        let (r, max) = initial_placement(Some(&s), &ms);
        assert_eq!((r.width, r.height), (1180, 780));
        assert!(max, "the maximized flag survives");
        // A monitor that went away.
        let gone = saved(-2500, 100, 1200, 800, 1.0, false);
        assert_eq!(initial_placement(Some(&gone), &ms).0.width, 1180);
    }

    #[test]
    fn extreme_saved_coordinates_never_overflow() {
        let ms = [mon(1920, 0, 1920, 1080, 1.0, true), mon(-1920, -1080, 1920, 1080, 1.25, false)];
        for (x, y, w, h) in [
            (i32::MIN, 0, 1, 1),
            (i32::MAX, i32::MAX, i32::MAX, i32::MAX),
            (i32::MIN, i32::MIN, i32::MAX, i32::MAX),
            (0, 0, i32::MIN, -5),
            (-1920, -1080, i32::MAX, i32::MAX),
        ] {
            let (r, _) = initial_placement(Some(&saved(x, y, w, h, 1.0, false)), &ms);
            assert!(r.width > 0 && r.height > 0, "{x} {y} {w} {h}");
        }
        assert_eq!(overlap(&PhysRect { x: i32::MIN, y: 0, width: 1, height: 1 }, &ms[0].work_area).0, 0);
        assert_eq!(overlap(&PhysRect { x: i32::MAX, y: i32::MAX, width: i32::MAX, height: i32::MAX }, &ms[0].work_area), (0, 0));
        // A saved scale that is not a number keeps the size.
        let (r, _) = initial_placement(Some(&saved(2000, 100, 1200, 800, f64::NAN, false)), &ms);
        assert_eq!((r.width, r.height), (1200, 800));
    }

    #[test]
    fn a_never_shown_window_keeps_the_saved_maximized_state() {
        assert!(saved_maximized(false, true));
        assert!(saved_maximized(true, false));
        assert!(!saved_maximized(false, false));
    }

    #[test]
    fn a_changed_scale_rescales_around_the_top_left() {
        let ms = [mon(0, 0, 2880, 1800, 1.5, true)];
        let s = saved(200, 100, 1200, 800, 1.0, false);
        assert_eq!(initial_placement(Some(&s), &ms).0, PhysRect { x: 200, y: 100, width: 1800, height: 1200 });
        // The best-overlapping monitor decides the scale.
        let two = [mon(0, 0, 1920, 1080, 1.0, true), mon(1920, 0, 2880, 1800, 2.0, false)];
        let s = saved(2000, 100, 1500, 1000, 2.0, false);
        assert_eq!(initial_placement(Some(&s), &two).0, PhysRect { x: 2000, y: 100, width: 1880, height: 1280 });
    }
}
