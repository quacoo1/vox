//! The widget window (shell.md §6.3). Exactly the card; no-activate topmost tool window (styles enforced by the
//! overlay subclass, `windowing` module docs); shown through Slint (`window.show()`, never activating thanks to the
//! attributes hook's `with_active(false)`), then `assert_topmost(hwnd, false)`; never closable by the user (an outside
//! `WM_CLOSE` reaches the subclass → `WindowEvent::CloseRequested(Widget)` → the core logs, persists and quits). Always
//! dark (D-USW-20).
//!
//! - `show(reposition)`: when hidden (or `reposition`), place with `geometry::place_widget` on the monitor under the
//!   cursor (`remember` with coordinates: the monitor matching the saved rect; a DPI hop first moves the hidden window
//!   onto that monitor, `windowing` docs); record the anchor and "moved by us" time; show — a hidden widget is shown
//!   parked off-screen and moved into place once its first frame at the new data and size was presented
//!   (`windowing::reveal`), so the previous show's stale surface never reaches the screen; start the hover poll.
//! - `resize(size)` (from `size-changed`): sanitize (40..1200 × 24..900); hidden → resize keeping the top-left;
//!   visible → `resize_anchored` against the work area of the monitor matching the old bounds; position **and** size
//!   in **one** `vox_win::window::set_bounds` (the `.slint` window does not bind its own size, so Slint never resizes
//!   it top-left first — no frame grows downward past the work area); mark "moved by us".
//! - Drag: `drag-start` → `winit Window::drag_window()`; a 50 ms timer waits for the left button release; if the
//!   window moved (and > 500 ms since our own move) → `WindowEvent::WidgetDragged{x, y}` (DIP; the settings write
//!   then re-places it through the clamped `remember` rule).
//! - Hover: poll the cursor every 100 ms while visible (`geometry::point_on_card`) → the widget controller's hover
//!   hold (no core round trip).
//! - Shape: the renderer's swap chain is opaque (`windowing` docs "Per-pixel alpha"), so the window is clipped to the
//!   card's rounded rectangle (`SetWindowRgn`, radius 20 DIP, a pill when compact) after every size change; the
//!   corners then show the desktop (aliased) instead of black.
//!
//! Coordinates: placement runs in the target monitor's own DIP (`physical / scale`, `dpi` module). A saved
//! `remember` point is an Electron DIP point; for the monitor that contains it, that space equals the monitor's own
//! DIP, and the nearest-monitor fallback relies on the `remember` clamp (D-DPI-1). The timers themselves live in
//! `ui::AppUi` (they need the controllers); this module is the native side they call ([`poll_hover`],
//! [`poll_drag`]).

use vox_core::geometry::{place_widget, resize_anchored, sanitize_size, Anchor, Point, Size, WidgetPlacementInput};
use vox_core::settings::WidgetPosition;
use vox_win::window::{MonitorInfo, PhysRect};

use super::dpi;
use crate::ui::WidgetWindow;

/// Native-side widget state.
#[derive(Debug, Default)]
pub struct WidgetWindowState {
    pub anchor: Anchor,
    pub size: Option<Size>,
    pub moved_by_us_ms: f64,
    pub visible: bool,
    pub hovered: bool,
    /// The window rect when a drag started (`None` = no drag in progress).
    pub drag_start: Option<PhysRect>,
}

/// What a drag poll found.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DragPoll {
    /// The button is still down: poll again.
    Pending,
    /// No drag in progress, or it ended without a user move.
    Done,
    /// The user moved the window: its top-left in Electron DIP (the `widget.customX/Y` values).
    Moved { x: i32, y: i32 },
}

/// The card's corner radius in DIP (`widget-window.slint`: `Theme.radius-widget`, a pill when compact).
const CARD_RADIUS_DIP: f64 = 20.0;

/// The card's corner radius in physical px for a window `height` px high (pure; tested).
pub fn card_radius(compact: bool, height: i32, scale: f64) -> f64 {
    if compact {
        f64::from(height) / 2.0
    } else {
        CARD_RADIUS_DIP * if scale > 0.0 { scale } else { 1.0 }
    }
}

/// Clips the window to the card's rounded shape (module docs "Shape") for a window of `size` physical px.
fn shape(window: &WidgetWindow, h: isize, width: i32, height: i32) {
    let compact = window.get_data().view == crate::ui::generated::WidgetView::Compact;
    super::native::set_round_region(h, width, height, card_radius(compact, height, window_scale(window)));
}

fn hwnd(window: &WidgetWindow) -> Option<isize> {
    use slint::ComponentHandle;
    super::hwnd_of(window.window())
}

/// The window's current scale (winit's, which follows `WM_DPICHANGED`).
fn window_scale(window: &WidgetWindow) -> f64 {
    use slint::ComponentHandle;
    super::with_winit(window.window(), |w| w.scale_factor()).filter(|s| *s > 0.0).unwrap_or(1.0)
}

/// The card size in DIP (the reported one, else the default).
fn card_size(state: &WidgetWindowState) -> Size {
    state.size.unwrap_or(Size { width: vox_core::constants::DEFAULT_WIDGET_SIZE.0, height: vox_core::constants::DEFAULT_WIDGET_SIZE.1 })
}

/// The placement target: the monitor and the saved point in that monitor's DIP (pure apart from the inputs).
pub fn placement_target(
    position: WidgetPosition,
    custom: Option<(i32, i32)>,
    cursor: (i32, i32),
    monitors: &[MonitorInfo],
    at_cursor: Option<MonitorInfo>,
) -> Option<(MonitorInfo, Option<Point>)> {
    if position == WidgetPosition::Remember {
        if let Some((cx, cy)) = custom {
            let p = Point { x: cx as f64, y: cy as f64 };
            if let Some((px, py, m)) = dpi::electron_dip_to_phys(p, monitors) {
                return Some((m, Some(Point { x: px as f64 / m.scale, y: py as f64 / m.scale })));
            }
            if let Some(m) = dpi::nearest_monitor_dip(p, monitors) {
                return Some((m, Some(p)));
            }
        }
    }
    let m = at_cursor.or_else(|| monitors.iter().copied().find(|m| m.bounds.contains(cursor.0, cursor.1))).or_else(|| monitors.first().copied())?;
    Some((m, None))
}

/// The bounds for a show: `(physical rect, anchor, monitor)` (pure; tested).
pub fn placement(
    settings: &vox_core::settings::WidgetSettings,
    size: Size,
    cursor: (i32, i32),
    monitors: &[MonitorInfo],
    at_cursor: Option<MonitorInfo>,
) -> Option<(PhysRect, Anchor, MonitorInfo)> {
    let custom = match (settings.custom_x, settings.custom_y) {
        (Some(x), Some(y)) => Some((x, y)),
        _ => None,
    };
    let (m, custom) = placement_target(settings.position, custom, cursor, monitors, at_cursor)?;
    let scale = if m.scale > 0.0 { m.scale } else { 1.0 };
    let (rect, anchor) = place_widget(WidgetPlacementInput {
        position: settings.position,
        size,
        area: dpi::work_area_dip(&m),
        cursor: Point { x: cursor.0 as f64 / scale, y: cursor.1 as f64 / scale },
        custom,
    });
    Some((dpi::to_phys(rect, scale), anchor, m))
}

/// Moves the hidden window onto `m` first when its scale differs from the window's (the DPI hop, `windowing` docs).
fn dpi_hop(hwnd: isize, current_scale: f64, m: &MonitorInfo) {
    if (current_scale - m.scale).abs() < 1e-6 {
        return;
    }
    let size = vox_win::window::window_rect(hwnd).map(|r| (r.width, r.height)).unwrap_or((1, 1));
    vox_win::window::set_bounds(hwnd, PhysRect { x: m.work_area.x, y: m.work_area.y, width: size.0.max(1), height: size.1.max(1) });
}

pub fn show(window: &WidgetWindow, state: &mut WidgetWindowState, settings: &vox_core::settings::Settings, reposition: bool) {
    use slint::ComponentHandle;
    let handle = hwnd(window);
    // A hidden window's target: `(rect, can park)`.
    let mut target = None;
    if let Some(h) = handle {
        if !state.visible || reposition {
            let cursor = vox_win::window::cursor_pos().unwrap_or((0, 0));
            let monitors = vox_win::window::monitors();
            let at_cursor = vox_win::window::monitor_from_point(cursor.0, cursor.1);
            if let Some((rect, anchor, m)) = placement(&settings.widget, card_size(state), cursor, &monitors, at_cursor) {
                if !state.visible {
                    dpi_hop(h, window_scale(window), &m);
                    target = Some((rect, super::reveal::can_park(&m, &monitors)));
                } else if !super::reveal::retarget(h, rect) {
                    vox_win::window::set_bounds(h, rect);
                }
                shape(window, h, rect.width, rect.height);
                state.anchor = anchor;
                state.moved_by_us_ms = super::mono_ms();
            }
        }
    }
    if !state.visible {
        let parked = match (handle, target) {
            (Some(h), Some((rect, true))) => super::reveal::begin(window.window(), h, rect),
            _ => false,
        };
        if let (false, Some(h), Some((rect, _))) = (parked, handle, target) {
            vox_win::window::set_bounds(h, rect);
        }
        if let Err(e) = window.show() {
            tracing::warn!(target: "window", "could not show the widget: {e}");
            if let Some(h) = handle {
                super::reveal::cancel(h);
            }
            return;
        }
        state.visible = true;
    }
    if let Some(h) = handle.or_else(|| hwnd(window)) {
        vox_win::window::assert_topmost(h, false);
    }
}

pub fn hide(window: &WidgetWindow, state: &mut WidgetWindowState) {
    use slint::ComponentHandle;
    state.visible = false;
    state.hovered = false;
    state.drag_start = None;
    if let Err(e) = window.hide() {
        tracing::warn!(target: "window", "could not hide the widget: {e}");
    }
    if let Some(h) = hwnd(window) {
        super::reveal::cancel(h);
    }
}

/// The new bounds for a size report (pure; tested): hidden → keep the top-left; visible → anchored against `area`
/// (all in the monitor's DIP), converted with `scale`.
pub fn resized_bounds(old: PhysRect, size: Size, visible: bool, anchor: Anchor, work_area: PhysRect, scale: f64) -> PhysRect {
    let scale = if scale > 0.0 { scale } else { 1.0 };
    if !visible {
        let w = vox_core::js::js_round(size.width * scale) as i32;
        let h = vox_core::js::js_round(size.height * scale) as i32;
        return PhysRect { x: old.x, y: old.y, width: w.max(1), height: h.max(1) };
    }
    let r = resize_anchored(dpi::to_dip(old, scale), size, anchor, dpi::to_dip(work_area, scale));
    dpi::to_phys(r, scale)
}

pub fn resize(window: &WidgetWindow, state: &mut WidgetWindowState, width: f64, height: f64) {
    let Some(size) = sanitize_size(width, height, vox_core::constants::WIDGET_SIZE_MIN, vox_core::constants::WIDGET_SIZE_MAX) else {
        return;
    };
    if state.size == Some(size) {
        return;
    }
    state.size = Some(size);
    let Some(h) = hwnd(window) else { return };
    if let Some(target) = super::reveal::target(h) {
        // Parked (first frame pending): the resize applies to where the window is going.
        let (work_area, scale) = vox_win::window::monitor_from_rect(target).map(|m| (m.work_area, m.scale)).unwrap_or((target, window_scale(window)));
        let next = resized_bounds(target, size, true, state.anchor, work_area, scale);
        super::reveal::retarget(h, next);
        shape(window, h, next.width, next.height);
        state.moved_by_us_ms = super::mono_ms();
        return;
    }
    let Some(old) = vox_win::window::window_rect(h) else { return };
    let monitor = vox_win::window::monitor_from_rect(old);
    let (work_area, scale) = match (&monitor, state.visible) {
        (Some(m), true) => (m.work_area, m.scale),
        _ => (old, window_scale(window)),
    };
    let next = resized_bounds(old, size, state.visible, state.anchor, work_area, scale);
    vox_win::window::set_bounds(h, next);
    shape(window, h, next.width, next.height);
    if state.visible {
        state.moved_by_us_ms = super::mono_ms();
    }
}

pub fn start_drag(window: &WidgetWindow, state: &mut WidgetWindowState) {
    use slint::ComponentHandle;
    if !state.visible {
        return;
    }
    state.drag_start = hwnd(window).and_then(vox_win::window::window_rect);
    let started = super::with_winit(window.window(), |w| w.drag_window());
    match started {
        Some(Ok(())) => {}
        Some(Err(e)) => {
            tracing::debug!(target: "window", "widget drag did not start: {e}");
            state.drag_start = None;
        }
        None => state.drag_start = None,
    }
}

/// The 50 ms drag-end poll (shell.md §6.3.5, D-SH-8). `now_ms` is [`super::mono_ms`] (the clock of `moved_by_us_ms`).
pub fn poll_drag(window: &WidgetWindow, state: &mut WidgetWindowState, now_ms: f64) -> DragPoll {
    let Some(start) = state.drag_start else { return DragPoll::Done };
    if vox_win::window::left_button_down() {
        return DragPoll::Pending;
    }
    state.drag_start = None;
    let Some(rect) = hwnd(window).and_then(vox_win::window::window_rect) else { return DragPoll::Done };
    if !state.visible || rect == start || now_ms - state.moved_by_us_ms <= vox_core::constants::OWN_MOVE_GRACE_MS as f64 {
        return DragPoll::Done;
    }
    if let Some(m) = vox_win::window::monitor_from_rect(rect) {
        state.anchor = vox_core::geometry::anchor_for(dpi::to_dip(rect, m.scale), dpi::work_area_dip(&m));
    }
    match dpi::phys_to_electron_dip(rect.x, rect.y, &vox_win::window::monitors()) {
        Some(p) => DragPoll::Moved { x: p.x as i32, y: p.y as i32 },
        None => DragPoll::Done,
    }
}

/// The 100 ms hover poll: `Some(hovered)` when the hover state changed.
pub fn poll_hover(window: &WidgetWindow, state: &mut WidgetWindowState) -> Option<bool> {
    let next = if state.visible {
        match (hwnd(window).and_then(vox_win::window::window_rect), vox_win::window::cursor_pos()) {
            (Some(r), Some((x, y))) => {
                let win = vox_core::geometry::Rect::new(r.x as f64, r.y as f64, r.width as f64, r.height as f64);
                vox_core::geometry::point_on_card(Point { x: x as f64, y: y as f64 }, win, None)
            }
            _ => state.hovered,
        }
    } else {
        false
    };
    if next != state.hovered {
        state.hovered = next;
        Some(next)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vox_core::geometry::{HAnchor, VAnchor};
    use vox_core::settings::WidgetSettings;

    fn mon(x: i32, y: i32, w: i32, h: i32, scale: f64, primary: bool) -> MonitorInfo {
        let r = PhysRect { x, y, width: w, height: h };
        MonitorInfo { bounds: r, work_area: PhysRect { height: h - 48, ..r }, scale, primary, id: x as isize }
    }

    fn widget(position: WidgetPosition, custom: Option<(i32, i32)>) -> WidgetSettings {
        WidgetSettings { position, custom_x: custom.map(|c| c.0), custom_y: custom.map(|c| c.1), ..WidgetSettings::default() }
    }

    const CARD: Size = Size { width: 380.0, height: 198.0 };

    #[test]
    fn bottom_right_follows_the_monitor_under_the_cursor() {
        let ms = [mon(0, 0, 2880, 1800, 1.5, true), mon(2880, 0, 1920, 1080, 1.0, false)];
        let (r, a, m) = placement(&widget(WidgetPosition::BottomRight, None), CARD, (3000, 500), &ms, Some(ms[1])).unwrap();
        assert_eq!(m.id, 2880);
        assert_eq!(r, PhysRect { x: 2880 + 1920 - 380 - 24, y: 1032 - 198 - 24, width: 380, height: 198 });
        assert_eq!(a, Anchor { h: HAnchor::Right, v: VAnchor::Bottom });
        // At 150 % everything scales.
        let (r, _, _) = placement(&widget(WidgetPosition::BottomRight, None), CARD, (100, 100), &ms, Some(ms[0])).unwrap();
        assert_eq!(r, PhysRect { x: 2880 - 570 - 36, y: 1752 - 297 - 36, width: 570, height: 297 });
    }

    #[test]
    fn remember_maps_the_saved_point_through_its_monitor() {
        let ms = [mon(0, 0, 2880, 1800, 1.5, true), mon(2880, 0, 1920, 1080, 1.0, false)];
        // DIP (1000, 500) on the 150 % primary → physical (1500, 750).
        let (r, _, m) = placement(&widget(WidgetPosition::Remember, Some((1000, 500))), CARD, (3000, 10), &ms, Some(ms[1])).unwrap();
        assert!(m.primary);
        assert_eq!((r.x, r.y, r.width, r.height), (1500, 750, 570, 297));
        // A point in no monitor (the DIP gap 1920..2880) → nearest monitor, clamped into its work area.
        let (r, _, m) = placement(&widget(WidgetPosition::Remember, Some((2400, 100))), CARD, (0, 0), &ms, Some(ms[0])).unwrap();
        assert!(m.primary);
        assert_eq!(r.x + r.width, 2880, "clamped to the right edge of the primary work area");
        // Remember without coordinates = bottom-right at the cursor.
        let (r, _, _) = placement(&widget(WidgetPosition::Remember, None), CARD, (3000, 10), &ms, Some(ms[1])).unwrap();
        assert_eq!(r.x, 2880 + 1920 - 380 - 24);
    }

    #[test]
    fn the_card_shape_follows_the_view() {
        assert_eq!(card_radius(false, 248, 1.0), 20.0);
        assert_eq!(card_radius(false, 372, 1.5), 30.0);
        assert_eq!(card_radius(true, 72, 1.5), 36.0, "compact is a pill");
        assert_eq!(card_radius(false, 100, 0.0), 20.0);
    }

    #[test]
    fn resize_keeps_the_anchored_edges() {
        let wa = PhysRect { x: 0, y: 0, width: 1920, height: 1032 };
        let old = PhysRect { x: 1516, y: 810, width: 380, height: 198 };
        let anchor = Anchor { h: HAnchor::Right, v: VAnchor::Bottom };
        let grown = resized_bounds(old, Size { width: 380.0, height: 228.0 }, true, anchor, wa, 1.0);
        assert_eq!(grown, PhysRect { x: 1516, y: 780, width: 380, height: 228 });
        let compact = resized_bounds(old, Size { width: 340.0, height: 48.0 }, true, anchor, wa, 1.0);
        assert_eq!(compact, PhysRect { x: 1556, y: 960, width: 340, height: 48 });
        // Hidden: keep the top-left, scale the size.
        let hidden = resized_bounds(old, Size { width: 340.0, height: 48.0 }, false, anchor, wa, 1.5);
        assert_eq!(hidden, PhysRect { x: 1516, y: 810, width: 510, height: 72 });
        // Never past the work area.
        let top = Anchor { h: HAnchor::Left, v: VAnchor::Top };
        let low = PhysRect { x: 10, y: 900, width: 380, height: 100 };
        let r = resized_bounds(low, Size { width: 380.0, height: 198.0 }, true, top, wa, 1.0);
        assert_eq!(r.y + r.height, 1032);
    }
}
