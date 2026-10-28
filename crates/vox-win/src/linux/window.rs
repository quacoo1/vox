//! X11 window helpers (the Linux side of shell.md §6.1). Window ids are X11 client windows; rectangles are root
//! pixels. Outer rectangles include the window manager's frame (`_NET_FRAME_EXTENTS`), so a rectangle read with
//! [`window_rect`] can be written back with [`set_bounds`] without drifting by the title bar.

use std::sync::OnceLock;

use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _, EventMask, KeyButMask, MapState, PropMode,
    StackMode, Window,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

pub type Hwnd = isize;

/// A rectangle in physical (root-window) pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PhysRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

impl PhysRect {
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    fn intersect(&self, o: &PhysRect) -> Option<PhysRect> {
        let x0 = self.x.max(o.x);
        let y0 = self.y.max(o.y);
        let x1 = (self.x + self.width).min(o.x + o.width);
        let y1 = (self.y + self.height).min(o.y + o.height);
        (x1 > x0 && y1 > y0).then_some(PhysRect { x: x0, y: y0, width: x1 - x0, height: y1 - y0 })
    }
}

/// One monitor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MonitorInfo {
    pub bounds: PhysRect,
    /// Bounds minus panels (`_NET_WORKAREA` clipped to the monitor).
    pub work_area: PhysRect,
    /// `Xft.dpi / 96` (what winit uses for X11 windows), else 1.
    pub scale: f64,
    pub primary: bool,
    /// The RandR monitor name atom.
    pub id: isize,
}

/// Called when an overlay is asked to close by someone else (unused on X11: overlays have no close button).
pub type OverlayCloseHandler = std::sync::Arc<dyn Fn() + Send + Sync>;

struct X {
    conn: RustConnection,
    root: Window,
    atoms: Atoms,
}

struct Atoms {
    net_wm_state: Atom,
    above: Atom,
    skip_taskbar: Atom,
    skip_pager: Atom,
    max_vert: Atom,
    max_horz: Atom,
    active_window: Atom,
    frame_extents: Atom,
    workarea: Atom,
    gtk_theme_variant: Atom,
    utf8_string: Atom,
    resource_manager: Atom,
}

fn x() -> Option<&'static X> {
    static X11: OnceLock<Option<X>> = OnceLock::new();
    X11.get_or_init(|| {
        let (conn, screen) = match x11rb::connect(None) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(target: "window", "no X11 display: {e}");
                return None;
            }
        };
        let root = conn.setup().roots.get(screen)?.root;
        let atom = |name: &str| -> Option<Atom> { Some(conn.intern_atom(false, name.as_bytes()).ok()?.reply().ok()?.atom) };
        let atoms = Atoms {
            net_wm_state: atom("_NET_WM_STATE")?,
            above: atom("_NET_WM_STATE_ABOVE")?,
            skip_taskbar: atom("_NET_WM_STATE_SKIP_TASKBAR")?,
            skip_pager: atom("_NET_WM_STATE_SKIP_PAGER")?,
            max_vert: atom("_NET_WM_STATE_MAXIMIZED_VERT")?,
            max_horz: atom("_NET_WM_STATE_MAXIMIZED_HORZ")?,
            active_window: atom("_NET_ACTIVE_WINDOW")?,
            frame_extents: atom("_NET_FRAME_EXTENTS")?,
            workarea: atom("_NET_WORKAREA")?,
            gtk_theme_variant: atom("_GTK_THEME_VARIANT")?,
            utf8_string: atom("UTF8_STRING")?,
            resource_manager: atom("RESOURCE_MANAGER")?,
        };
        Some(X { conn, root, atoms })
    })
    .as_ref()
}

fn win(hwnd: Hwnd) -> Option<Window> {
    u32::try_from(hwnd).ok().filter(|w| *w != 0)
}

fn cardinals(x: &X, window: Window, property: Atom, kind: AtomEnum) -> Vec<u32> {
    x.conn
        .get_property(false, window, property, kind, 0, 4096)
        .ok()
        .and_then(|c| c.reply().ok())
        .and_then(|r| r.value32().map(|v| v.collect()))
        .unwrap_or_default()
}

fn wm_state(x: &X, window: Window) -> Vec<Atom> {
    cardinals(x, window, x.atoms.net_wm_state, AtomEnum::ATOM)
}

/// `_NET_FRAME_EXTENTS` = (left, right, top, bottom); zeros when undecorated or unknown.
fn frame_extents(x: &X, window: Window) -> (i32, i32, i32, i32) {
    match cardinals(x, window, x.atoms.frame_extents, AtomEnum::CARDINAL).as_slice() {
        [l, r, t, b, ..] => (*l as i32, *r as i32, *t as i32, *b as i32),
        _ => (0, 0, 0, 0),
    }
}

/// Sends an EWMH client message to the root window.
fn send_root_message(x: &X, window: Window, kind: Atom, data: [u32; 5]) {
    let event = ClientMessageEvent::new(32, window, kind, data);
    let _ = x.conn.send_event(false, x.root, EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY, event);
}

/// Marks an overlay (player, chip) as a keep-above window that stays out of the taskbar and the overview. The window
/// is hidden (withdrawn) when this runs, so the state is written directly and the window manager reads it at the
/// next map; while mapped, the EWMH client messages are sent as well.
pub fn install_overlay_subclass(hwnd: Hwnd, _on_close: OverlayCloseHandler) -> Result<(), crate::WinError> {
    let x = x().ok_or_else(|| crate::WinError("no X11 display".into()))?;
    let w = win(hwnd).ok_or_else(|| crate::WinError("not an X11 window".into()))?;
    let mut state = wm_state(x, w);
    for a in [x.atoms.above, x.atoms.skip_taskbar, x.atoms.skip_pager] {
        if !state.contains(&a) {
            state.push(a);
        }
    }
    x.conn
        .change_property32(PropMode::REPLACE, w, x.atoms.net_wm_state, AtomEnum::ATOM, &state)
        .map_err(|e| crate::WinError(e.to_string()))?;
    if is_visible(hwnd) {
        send_root_message(x, w, x.atoms.net_wm_state, [1, x.atoms.skip_taskbar, x.atoms.skip_pager, 1, 0]);
    }
    let _ = x.conn.flush();
    Ok(())
}

/// Re-asserts keep-above (and raises the window when `raise`).
pub fn assert_topmost(hwnd: Hwnd, raise: bool) {
    let (Some(x), Some(w)) = (x(), win(hwnd)) else { return };
    send_root_message(x, w, x.atoms.net_wm_state, [1, x.atoms.above, 0, 1, 0]);
    send_root_message(x, w, x.atoms.net_wm_state, [1, x.atoms.skip_taskbar, x.atoms.skip_pager, 1, 0]);
    if raise {
        let _ = x.conn.configure_window(w, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE));
    }
    let _ = x.conn.flush();
}

pub fn is_visible(hwnd: Hwnd) -> bool {
    let (Some(x), Some(w)) = (x(), win(hwnd)) else { return false };
    x.conn
        .get_window_attributes(w)
        .ok()
        .and_then(|c| c.reply().ok())
        .is_some_and(|a| a.map_state == MapState::VIEWABLE)
}

/// Moves and sizes the window so that its outer (framed) rectangle is `r`.
pub fn set_bounds(hwnd: Hwnd, r: PhysRect) {
    let (Some(x), Some(w)) = (x(), win(hwnd)) else { return };
    let (l, rt, t, b) = frame_extents(x, w);
    let width = (r.width - l - rt).max(1) as u32;
    let height = (r.height - t - b).max(1) as u32;
    let _ = x.conn.configure_window(w, &ConfigureWindowAux::new().x(r.x).y(r.y).width(width).height(height));
    let _ = x.conn.flush();
}

/// The outer (framed) rectangle in root pixels.
pub fn window_rect(hwnd: Hwnd) -> Option<PhysRect> {
    let x = x()?;
    let w = win(hwnd)?;
    let geometry = x.conn.get_geometry(w).ok()?.reply().ok()?;
    let origin = x.conn.translate_coordinates(w, x.root, 0, 0).ok()?.reply().ok()?;
    let (l, r, t, b) = frame_extents(x, w);
    Some(PhysRect {
        x: origin.dst_x as i32 - l,
        y: origin.dst_y as i32 - t,
        width: geometry.width as i32 + l + r,
        height: geometry.height as i32 + t + b,
    })
}

/// Asks the window manager for a dark or light title bar (`_GTK_THEME_VARIANT`, honoured by Mutter).
pub fn set_caption_theme(hwnd: Hwnd, dark: bool, _caption_rgb: u32) {
    let (Some(x), Some(w)) = (x(), win(hwnd)) else { return };
    let value: &[u8] = if dark { b"dark" } else { b"light" };
    let _ = x.conn.change_property8(PropMode::REPLACE, w, x.atoms.gtk_theme_variant, x.atoms.utf8_string, value);
    let _ = x.conn.flush();
}

/// Activates the window (`_NET_ACTIVE_WINDOW` with a pager source, which focus-stealing prevention lets through).
pub fn bring_to_front(hwnd: Hwnd) {
    let (Some(x), Some(w)) = (x(), win(hwnd)) else { return };
    // A minimized window (`_NET_WM_STATE_HIDDEN`) is restored by the activation request itself.
    send_root_message(x, w, x.atoms.active_window, [2, 0, 0, 0, 0]);
    let _ = x.conn.configure_window(w, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE));
    let _ = x.conn.flush();
}

fn pointer() -> Option<(i32, i32, KeyButMask)> {
    let x = x()?;
    let p = x.conn.query_pointer(x.root).ok()?.reply().ok()?;
    Some((p.root_x as i32, p.root_y as i32, p.mask))
}

/// The pointer in root pixels. Under XWayland it is only up to date while it is over an X11 window or was last there.
pub fn cursor_pos() -> Option<(i32, i32)> {
    pointer().map(|(x, y, _)| (x, y))
}

/// The primary (left) button is down.
pub fn left_button_down() -> bool {
    pointer().is_some_and(|(_, _, mask)| mask.contains(KeyButMask::BUTTON1))
}

pub fn monitor_from_point(x: i32, y: i32) -> Option<MonitorInfo> {
    let all = monitors();
    all.iter().find(|m| m.bounds.contains(x, y)).copied().or_else(|| nearest(&all, x, y))
}

pub fn monitor_from_rect(r: PhysRect) -> Option<MonitorInfo> {
    let all = monitors();
    let best = all
        .iter()
        .filter_map(|m| m.bounds.intersect(&r).map(|i| (i64::from(i.width) * i64::from(i.height), m)))
        .max_by_key(|(area, _)| *area)
        .map(|(_, m)| *m);
    best.or_else(|| nearest(&all, r.x + r.width / 2, r.y + r.height / 2))
}

pub fn primary_monitor() -> Option<MonitorInfo> {
    let all = monitors();
    all.iter().find(|m| m.primary).copied().or_else(|| all.first().copied())
}

fn nearest(all: &[MonitorInfo], x: i32, y: i32) -> Option<MonitorInfo> {
    all.iter()
        .min_by_key(|m| {
            let b = m.bounds;
            let dx = (b.x - x).max(0).max(x - (b.x + b.width - 1));
            let dy = (b.y - y).max(0).max(y - (b.y + b.height - 1));
            i64::from(dx) * i64::from(dx) + i64::from(dy) * i64::from(dy)
        })
        .copied()
}

/// `Xft.dpi` from the root window's `RESOURCE_MANAGER` / 96.
fn xft_scale(x: &X) -> f64 {
    let Some(reply) = x.conn.get_property(false, x.root, x.atoms.resource_manager, AtomEnum::STRING, 0, 1 << 20).ok().and_then(|c| c.reply().ok())
    else {
        return 1.0;
    };
    let text = String::from_utf8_lossy(&reply.value);
    text.lines()
        .find_map(|l| l.strip_prefix("Xft.dpi:").and_then(|v| v.trim().parse::<f64>().ok()))
        .filter(|dpi| *dpi > 0.0)
        .map_or(1.0, |dpi| dpi / 96.0)
}

pub fn monitors() -> Vec<MonitorInfo> {
    let Some(x) = x() else { return Vec::new() };
    let scale = std::env::var("WINIT_X11_SCALE_FACTOR").ok().and_then(|v| v.parse::<f64>().ok()).filter(|s| *s > 0.0).unwrap_or_else(|| xft_scale(x));
    let work = match cardinals(x, x.root, x.atoms.workarea, AtomEnum::CARDINAL).as_slice() {
        [wx, wy, ww, wh, ..] => Some(PhysRect { x: *wx as i32, y: *wy as i32, width: *ww as i32, height: *wh as i32 }),
        _ => None,
    };
    let mut out: Vec<MonitorInfo> = x
        .conn
        .randr_get_monitors(x.root, true)
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.monitors)
        .unwrap_or_default()
        .into_iter()
        .map(|m| {
            let bounds = PhysRect { x: m.x as i32, y: m.y as i32, width: m.width as i32, height: m.height as i32 };
            let work_area = work.and_then(|w| w.intersect(&bounds)).unwrap_or(bounds);
            MonitorInfo { bounds, work_area, scale, primary: m.primary, id: m.name as isize }
        })
        .collect();
    if out.is_empty() {
        let screen = &x.conn.setup().roots[0];
        let bounds = PhysRect { x: 0, y: 0, width: screen.width_in_pixels as i32, height: screen.height_in_pixels as i32 };
        out.push(MonitorInfo { bounds, work_area: work.and_then(|w| w.intersect(&bounds)).unwrap_or(bounds), scale, primary: true, id: 1 });
    }
    if !out.iter().any(|m| m.primary) {
        out[0].primary = true;
    }
    out
}

/// The outer rectangle and the maximized flag.
pub fn normal_placement(hwnd: Hwnd) -> Option<(PhysRect, bool)> {
    let x = x()?;
    let w = win(hwnd)?;
    let state = wm_state(x, w);
    let maximized = state.contains(&x.atoms.max_vert) && state.contains(&x.atoms.max_horz);
    Some((window_rect(hwnd)?, maximized))
}

/// Applies an outer rectangle and the maximized state (visibility stays with Slint).
pub fn set_normal_placement(hwnd: Hwnd, normal: PhysRect, maximized: bool, _show: bool) {
    let (Some(x), Some(w)) = (x(), win(hwnd)) else { return };
    if normal.width > 0 && normal.height > 0 {
        set_bounds(hwnd, normal);
    }
    let action = u32::from(maximized);
    send_root_message(x, w, x.atoms.net_wm_state, [action, x.atoms.max_vert, x.atoms.max_horz, 1, 0]);
    let _ = x.conn.flush();
}

/// GNOME's "Reduce animation" (`org.gnome.desktop.interface enable-animations` false).
pub fn reduce_motion() -> bool {
    super::run_output("gsettings", &["get", "org.gnome.desktop.interface", "enable-animations"]).is_some_and(|v| v == "false")
}

/// The scale GNOME uses for the primary monitor when Mutter renders X11 clients at native resolution
/// (`xwayland-native-scaling`): Mutter then advertises the next whole number as `Xft.dpi` (2 for 125 %), which would
/// make Vox larger than every Wayland app. `None` when not on such a GNOME session.
pub fn gnome_native_x11_scale() -> Option<f64> {
    let features = super::run_output("gsettings", &["get", "org.gnome.mutter", "experimental-features"])?;
    if !features.contains("xwayland-native-scaling") {
        return None;
    }
    let state = super::run_output(
        "gdbus",
        &[
            "call",
            "--session",
            "--dest",
            "org.gnome.Mutter.DisplayConfig",
            "--object-path",
            "/org/gnome/Mutter/DisplayConfig",
            "--method",
            "org.gnome.Mutter.DisplayConfig.GetCurrentState",
        ],
    )?;
    primary_logical_scale(&state)
}

/// Finds `(x, y, <scale>, uint32 <transform>, true, …)` (the primary logical monitor) in GetCurrentState's output.
fn primary_logical_scale(state: &str) -> Option<f64> {
    let re = regex::Regex::new(r"\(-?\d+, -?\d+, ([0-9]+(?:\.[0-9]+)?), uint32 \d+, true").ok()?;
    re.captures(state)?.get(1)?.as_str().parse::<f64>().ok().filter(|s| *s > 0.0 && *s <= 8.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_scale_from_display_state() {
        let s = "(uint32 3, [], [(0, 0, 1.25, uint32 0, true, [('eDP-1', 'BOE', '0x0a1f', '0x00000000')], {})], {})";
        assert_eq!(primary_logical_scale(s), Some(1.25));
        assert_eq!(primary_logical_scale("(0, 0, 2.0, uint32 0, false, [])"), None);
    }
}
