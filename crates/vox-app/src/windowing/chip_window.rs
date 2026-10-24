//! The chip window (shell.md §6.5): no-activate topmost tool window (styles enforced by the overlay subclass), shown
//! through Slint and then raised to the top of the topmost band (`assert_topmost(hwnd, true)`, above an open context
//! menu); click-through only during the 140 ms exit (winit `set_cursor_hittest(false)`; the subclass keeps the layered
//! window rendering — if it does not render, the chip hides at once when the exit starts); placed with
//! `geometry::place_chip` in the target monitor's DIP (a DPI hop first moves the hidden window onto that monitor);
//! position and size applied in one `set_bounds` (the `.slint` window does not bind its own size); its real window rect
//! is handed to `SelectionService::set_chip_bounds` (physical px). Nothing happens before `AppUi::overlays_ready`.
//!
//! The show/hide sequencing (pending hide, sizes per mode, the press gate) is driven by `ui::AppUi` with the
//! `ui::chip::ChipController` state; this module is the native side.

use slint::ComponentHandle;
use vox_core::app::ChipMode;
use vox_core::geometry::{place_chip, Point, Size};
use vox_win::window::{MonitorInfo, PhysRect};

use super::dpi;
use crate::ui::ChipWindow;

/// The chip bounds for a cursor (pure; tested): placed in the cursor monitor's DIP, converted back with its scale.
pub fn placement(mode: ChipMode, cursor_phys: (i32, i32), size_dip: (f64, f64), m: &MonitorInfo) -> PhysRect {
    let scale = if m.scale > 0.0 { m.scale } else { 1.0 };
    let cursor = Point { x: cursor_phys.0 as f64 / scale, y: cursor_phys.1 as f64 / scale };
    let rect = place_chip(mode, cursor, Size { width: size_dip.0, height: size_dip.1 }, dpi::work_area_dip(m));
    dpi::to_phys(rect, scale)
}

fn set_hittest(window: &ChipWindow, on: bool) {
    if let Some(Err(e)) = super::with_winit(window.window(), |w| w.set_cursor_hittest(on)) {
        tracing::debug!(target: "window", "chip hit-test change failed: {e}");
    }
}

pub fn show_at(window: &ChipWindow, mode: ChipMode, cursor_phys: (i32, i32), size_dip: (f64, f64)) -> Option<vox_win::window::PhysRect> {
    let hwnd = super::hwnd_of(window.window())?;
    let m = vox_win::window::monitor_from_point(cursor_phys.0, cursor_phys.1)?;
    let rect = placement(mode, cursor_phys, size_dip, &m);
    let visible = window.window().is_visible();
    if !visible {
        let current = super::with_winit(window.window(), |w| w.scale_factor()).unwrap_or(m.scale);
        if (current - m.scale).abs() > 1e-6 {
            // DPI hop: onto the target monitor first, then the final bounds.
            vox_win::window::set_bounds(hwnd, PhysRect { x: m.work_area.x, y: m.work_area.y, width: rect.width.max(1), height: rect.height.max(1) });
        }
    }
    vox_win::window::set_bounds(hwnd, rect);
    // A pill: clipped to its shape (the swap chain is opaque, `windowing` docs "Per-pixel alpha").
    super::native::set_round_region(hwnd, rect.width, rect.height, f64::from(rect.height) / 2.0);
    set_hittest(window, true);
    if !visible {
        if let Err(e) = window.show() {
            tracing::warn!(target: "window", "could not show the chip: {e}");
            return None;
        }
    }
    vox_win::window::assert_topmost(hwnd, true);
    Some(vox_win::window::window_rect(hwnd).unwrap_or(rect))
}

/// Starts the exit: click-through on (the window stays visible while the exit animation runs).
pub fn begin_hide(window: &ChipWindow) {
    set_hittest(window, false);
}

/// The exit finished: hide the window (hit-testing is restored for the next show).
pub fn finish_hide(window: &ChipWindow) {
    if let Err(e) = window.hide() {
        tracing::warn!(target: "window", "could not hide the chip: {e}");
    }
    set_hittest(window, true);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(scale: f64) -> MonitorInfo {
        let r = PhysRect { x: 0, y: 0, width: 2880, height: 1800 };
        MonitorInfo { bounds: r, work_area: PhysRect { height: 1752, ..r }, scale, primary: true, id: 1 }
    }

    #[test]
    fn selection_chip_sits_above_right_of_the_cursor() {
        let r = placement(ChipMode::Selection, (300, 600), (82.0, 38.0), &mon(1.0));
        assert_eq!(r, PhysRect { x: 320, y: 600 - 20 - 38, width: 82, height: 38 });
        // At 150 % the DIP placement is scaled back.
        let r = placement(ChipMode::Selection, (300, 600), (82.0, 38.0), &mon(1.5));
        assert_eq!((r.width, r.height), (123, 57));
        assert_eq!(r.x, 330);
        // No room above → below.
        let r = placement(ChipMode::Selection, (300, 10), (82.0, 38.0), &mon(1.0));
        assert_eq!(r.y, 30);
    }

    #[test]
    fn context_chip_never_covers_the_predicted_menu() {
        let r = placement(ChipMode::Context, (500, 400), (153.0, 38.0), &mon(1.0));
        // The menu opens to the lower right of the cursor: the chip goes left of it.
        assert_eq!(r.x + r.width, 500 - 18);
    }
}
