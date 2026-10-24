//! DIP ↔ physical conversion per monitor (BRIEF.md Product rulings "DPI"; ARCHITECTURE.md §4.13, D-DPI-1).
//!
//! The process is Per-Monitor-V2 aware: every native coordinate is physical. The placement geometry
//! (`vox_core::geometry`) works in the DIP of **one** monitor (its work area divided by its scale), and the results
//! are converted back with that monitor's scale.
//!
//! `widget.customX/customY` in `settings.json` are **Electron DIP screen coordinates**. Electron (Chromium
//! `ScreenWin`) lays monitors out in a DIP space where each monitor's DIP origin is its physical origin divided by
//! its own scale (for the common layouts; Chromium additionally nudges non-primary monitors so touching edges keep
//! touching). The port uses exactly that model, [`monitor_dip_rect`]:
//! - **Load** ([`electron_dip_to_phys`]): find the monitor whose DIP rect contains the saved point (first match in
//!   `vox_win::window::monitors()` order, primary first) and map `phys = origin_phys + (dip − origin_dip) × scale`.
//!   No monitor contains it (unplugged monitor, Chromium's nudge) → `None`; the caller then takes
//!   [`nearest_monitor_dip`] and lets the `remember` rule clamp the saved rect into that monitor's work area
//!   ("if that fails, re-clamp to the nearest work area").
//! - **Save** ([`phys_to_electron_dip`], after a user drag): through the monitor containing the window's top-left
//!   (else the nearest), the inverse mapping, then `js_round`. On single-DPI setups this equals Electron's numbers
//!   exactly, so files stay interchangeable; on mixed-DPI setups both builds keep the widget on screen thanks to the
//!   `remember` clamp.

use vox_core::geometry::{Point, Rect};
use vox_win::window::{MonitorInfo, PhysRect};

/// Physical → DIP with `scale`.
pub fn to_dip(r: PhysRect, scale: f64) -> Rect {
    Rect::new(r.x as f64 / scale, r.y as f64 / scale, r.width as f64 / scale, r.height as f64 / scale)
}

/// DIP → physical (rounded half up like `Math.round`).
pub fn to_phys(r: Rect, scale: f64) -> PhysRect {
    let j = |v: f64| vox_core::js::js_round(v * scale) as i32;
    PhysRect { x: j(r.x), y: j(r.y), width: j(r.width), height: j(r.height) }
}

/// A monitor's work area in its own DIP.
pub fn work_area_dip(m: &MonitorInfo) -> Rect {
    to_dip(m.work_area, m.scale)
}

/// The monitor's bounds in the Electron/Chromium DIP screen space (origin = physical origin / own scale).
pub fn monitor_dip_rect(m: &MonitorInfo) -> Rect {
    to_dip(m.bounds, m.scale)
}

/// Saved Electron DIP point → physical point via the first monitor whose DIP rect contains it; `None` if none does.
pub fn electron_dip_to_phys(p: Point, monitors: &[MonitorInfo]) -> Option<(i32, i32, MonitorInfo)> {
    let m = monitors.iter().find(|m| monitor_dip_rect(m).contains(p))?;
    let o = monitor_dip_rect(m);
    let x = m.bounds.x as f64 + (p.x - o.x) * m.scale;
    let y = m.bounds.y as f64 + (p.y - o.y) * m.scale;
    Some((vox_core::js::js_round(x) as i32, vox_core::js::js_round(y) as i32, *m))
}

/// Physical point → Electron DIP point via the monitor containing it (else the nearest one, D-DPI-1), rounded with
/// `js_round` (the value written to `widget.customX/customY`).
pub fn phys_to_electron_dip(x: i32, y: i32, monitors: &[MonitorInfo]) -> Option<Point> {
    let m = monitors.iter().find(|m| m.bounds.contains(x, y)).or_else(|| nearest_monitor_phys(x, y, monitors))?;
    let o = monitor_dip_rect(m);
    let dx = o.x + (x - m.bounds.x) as f64 / m.scale;
    let dy = o.y + (y - m.bounds.y) as f64 / m.scale;
    Some(Point { x: vox_core::js::js_round(dx), y: vox_core::js::js_round(dy) })
}

/// The monitor nearest a physical point: minimal distance from the point to each monitor's bounds (first wins a tie).
pub fn nearest_monitor_phys(x: i32, y: i32, monitors: &[MonitorInfo]) -> Option<&MonitorInfo> {
    let dist = |m: &MonitorInfo| {
        let r = m.bounds;
        let (x, y) = (i128::from(x), i128::from(y));
        let (left, top) = (i128::from(r.x), i128::from(r.y));
        let (right, bottom) = (left + i128::from(r.width), top + i128::from(r.height));
        let dx = (left - x).max(0).max(x - right);
        let dy = (top - y).max(0).max(y - bottom);
        dx * dx + dy * dy
    };
    monitors.iter().min_by_key(|m| dist(m))
}

/// The monitor nearest a DIP point (fallback when no monitor contains a saved point): minimal distance from the
/// point to each monitor's DIP rect.
pub fn nearest_monitor_dip(p: Point, monitors: &[MonitorInfo]) -> Option<MonitorInfo> {
    let dist = |m: &MonitorInfo| {
        let r = monitor_dip_rect(m);
        let dx = (r.x - p.x).max(0.0).max(p.x - r.right());
        let dy = (r.y - p.y).max(0.0).max(p.y - r.bottom());
        dx * dx + dy * dy
    };
    monitors.iter().copied().min_by(|a, b| dist(a).total_cmp(&dist(b)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(x: i32, y: i32, w: i32, h: i32, scale: f64, primary: bool) -> MonitorInfo {
        let r = PhysRect { x, y, width: w, height: h };
        MonitorInfo { bounds: r, work_area: PhysRect { height: h - 48, ..r }, scale, primary, id: x as isize }
    }

    #[test]
    fn round_trips_single_and_mixed_dpi() {
        let ms = [mon(0, 0, 2880, 1800, 1.5, true), mon(2880, 0, 1920, 1080, 1.0, false)];
        // Primary at 150 %: DIP (1500, 900) → physical (2250, 1350) → back.
        let (x, y, m) = electron_dip_to_phys(Point { x: 1500.0, y: 900.0 }, &ms).unwrap();
        assert_eq!((x, y, m.primary), (2250, 1350, true));
        assert_eq!(phys_to_electron_dip(x, y, &ms), Some(Point { x: 1500.0, y: 900.0 }));
        // Second monitor at 100 %: its DIP origin is 2880 / 1.0.
        let (x, y, _) = electron_dip_to_phys(Point { x: 3000.0, y: 100.0 }, &ms).unwrap();
        assert_eq!((x, y), (3000, 100));
        // A point in the DIP gap between the monitors (1920..2880) belongs to none.
        assert!(electron_dip_to_phys(Point { x: 2500.0, y: 100.0 }, &ms).is_none());
        assert!(nearest_monitor_dip(Point { x: 2500.0, y: 100.0 }, &ms).is_some());
    }

    #[test]
    fn a_point_on_no_monitor_saves_through_the_nearest_one() {
        // A 100 % monitor left of a 150 % primary; the widget's top-left was dropped 10 px left of the left monitor.
        let ms = [mon(0, 0, 2880, 1800, 1.5, true), mon(-1920, 0, 1920, 1080, 1.0, false)];
        let p = phys_to_electron_dip(-1930, 100, &ms).unwrap();
        assert_eq!(p, Point { x: -1930.0, y: 100.0 }, "mapped through the left monitor, not the primary (-1287)");
        // Loading it back: no DIP rect contains it, so the nearest (left) monitor takes it and the remember clamp
        // keeps the widget where it was dropped.
        assert!(electron_dip_to_phys(p, &ms).is_none());
        assert_eq!(nearest_monitor_dip(p, &ms).unwrap().bounds.x, -1920);
        // Below the primary: the primary is nearest.
        assert!(phys_to_electron_dip(100, 1900, &ms).is_some());
        assert_eq!(nearest_monitor_phys(100, 1900, &ms).unwrap().bounds.x, 0);
        assert_eq!(nearest_monitor_phys(i32::MIN, i32::MAX, &ms).unwrap().bounds.x, -1920, "extreme values never overflow");
        assert!(phys_to_electron_dip(0, 0, &[]).is_none());
    }
}
