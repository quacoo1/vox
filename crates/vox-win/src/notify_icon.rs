//! The notification-area (tray) icon (shell.md §7.3–§7.4; BRIEF.md Product rulings "Tray"; ARCHITECTURE.md §8
//! D-SH-18). Replaces `tray-icon`, which never calls `NIM_SETVERSION` (tray-icon 0.26.1 only maps the legacy
//! `WM_*BUTTON*` lParams), so keyboard selection of the icon (Win+B, arrows, Enter/Space) arrives as
//! `WM_RBUTTONDOWN/UP` and opens the menu — the ruling "left click **and keyboard select** open the main window"
//! cannot be met with it. vox-app keeps `muda` for the menu itself (`muda::ContextMenu::show_context_menu_for_hwnd`
//! with [`NotifyIcon::hwnd`]).
//!
//! - Created on the UI thread: a hidden **top-level** window (message-only windows do not receive `TaskbarCreated`)
//!   whose procedure runs on that thread through the Slint/winit message pump, so events are delivered on the UI
//!   thread. `Shell_NotifyIconW(NIM_ADD)` with `NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP`, then
//!   `NIM_SETVERSION(NOTIFYICON_VERSION_4)`.
//! - Version-4 notifications: `NIN_SELECT`, `NIN_KEYSELECT`, `WM_LBUTTONUP` and `WM_LBUTTONDBLCLK` →
//!   [`NotifyIconEvent::Activate`] (a double click therefore activates at most twice; vox-app's show is idempotent);
//!   `WM_CONTEXTMENU` (right click, Shift+F10, the Menu key) → [`NotifyIconEvent::ContextMenu`] with the anchor point
//!   from `wParam` (physical px).
//! - `TaskbarCreated` (Explorer restarted; allowed through UIPI with `ChangeWindowMessageFilterEx`) → re-add the icon
//!   and re-apply the version, tooltip and current image.
//! - Icon size: the image whose size equals `GetSystemMetricsForDpi(SM_CXSMICON, dpi of the primary monitor)` (the
//!   nearest larger one, else the 32-px one); re-picked by [`NotifyIcon::refresh_dpi`] (vox-app calls it on
//!   `UiUpdate::DisplaysChanged`) and on every [`NotifyIcon::set_images`].
//! - Tooltip ≤ 127 UTF-16 units (`vox_core::format::tray_tooltip` already cuts).
//! - Never panics across the window procedure (`catch_unwind`, logged).
//!
//! Implementation notes: one mouse click produces `WM_LBUTTONUP` **and** `NIN_SELECT`; activations closer than
//! 300 ms are merged so a click activates once and a double click at most twice. The hidden window also re-picks the
//! icon on `WM_DISPLAYCHANGE` / `WM_DPICHANGED` by itself. An `NIM_ADD` that fails at start-up (Explorer not ready
//! yet) is logged and retried on `TaskbarCreated`. An outside `WM_CLOSE` (taskkill without `/f`) is ignored: the system
//! watcher turns it into the app's quit, which destroys the icon.

use std::cell::RefCell;
use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;
use std::time::{Duration, Instant};

use windows::core::{w, BOOL, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, MonitorFromPoint, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS, HBITMAP, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, GetSystemMetricsForDpi, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION,
    NINF_KEY, NIN_SELECT, NOTIFYICONDATAW, NOTIFYICON_VERSION_4, NOTIFY_ICON_DATA_FLAGS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    ChangeWindowMessageFilterEx, CreateIconIndirect, DefWindowProcW, DestroyIcon, DestroyWindow, GetWindowLongPtrW,
    RegisterWindowMessageW, SetWindowLongPtrW, GWLP_USERDATA, HICON, ICONINFO, MSGFLT_ALLOW, SM_CXSMICON, WM_APP,
    WM_CLOSE, WM_CONTEXTMENU, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NCDESTROY,
};

use crate::window::{create_hidden_window, hwnd_from, hwnd_raw, register_class, Hwnd};

/// One rendered icon bitmap (straight RGBA8, `size × size`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayIconImage {
    pub size: u32,
    pub rgba: Vec<u8>,
}

/// What the icon reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyIconEvent {
    /// Left click, double click, or keyboard select (Enter/Space): open the main window.
    Activate,
    /// Right click / Shift+F10 / Menu key: show the menu at this physical point.
    ContextMenu { x: i32, y: i32 },
}

/// Event callback, called on the UI thread (the thread that created the icon).
pub type NotifyIconHandler = Box<dyn Fn(NotifyIconEvent)>;

/// The `uCallbackMessage` of our icon.
const TRAY_CALLBACK: u32 = WM_APP + 0x54;
/// The icon id within our window.
const ICON_ID: u32 = 1;
/// Activations closer than this are one (a click sends `WM_LBUTTONUP` and `NIN_SELECT`).
const ACTIVATE_MERGE: Duration = Duration::from_millis(300);
/// `NIN_KEYSELECT` = `NIN_SELECT | NINF_KEY`.
const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;

/// The icon and its hidden window. Not `Send`: lives on the UI thread.
pub struct NotifyIcon {
    hwnd: Hwnd,
    shared: Option<Rc<IconShared>>,
}

/// Shared between the `NotifyIcon` handle and the window procedure (through `GWLP_USERDATA`).
struct IconShared {
    on_event: NotifyIconHandler,
    state: RefCell<IconState>,
}

struct IconState {
    hwnd: HWND,
    tooltip: String,
    images: Vec<TrayIconImage>,
    /// Index into `images` of the bitmap behind `hicon`.
    shown: Option<usize>,
    hicon: Option<HICON>,
    /// `NIM_ADD` succeeded (and no `TaskbarCreated` since).
    added: bool,
    taskbar_created: u32,
    gate: ActivateGate,
}

impl Drop for IconState {
    fn drop(&mut self) {
        if let Some(icon) = self.hicon.take() {
            // SAFETY: an icon we created and no longer hand to the shell.
            let _ = unsafe { DestroyIcon(icon) };
        }
    }
}

impl NotifyIcon {
    /// Creates the hidden window and adds the icon (version 4). `images` holds the 16/20/24/32-px renderings.
    pub fn new(
        tooltip: &str,
        images: Vec<TrayIconImage>,
        on_event: NotifyIconHandler,
    ) -> Result<NotifyIcon, crate::WinError> {
        let class = w!("VoxTrayIconWindow");
        register_class(class, Some(tray_proc))?;
        let hwnd = create_hidden_window(class, "Vox Tray", None)?;
        let message = crate::window::wide("TaskbarCreated");
        // SAFETY: the string outlives the call.
        let taskbar_created = unsafe { RegisterWindowMessageW(PCWSTR(message.as_ptr())) };
        if taskbar_created != 0 {
            // SAFETY: our own window; lets Explorer's broadcast through UIPI when Vox runs elevated.
            if let Err(e) = unsafe { ChangeWindowMessageFilterEx(hwnd, taskbar_created, MSGFLT_ALLOW, None) } {
                tracing::debug!(target: "tray", "TaskbarCreated filter not changed: {e}");
            }
        }
        let shared = Rc::new(IconShared {
            on_event,
            state: RefCell::new(IconState {
                hwnd,
                tooltip: tooltip.to_string(),
                images,
                shown: None,
                hicon: None,
                added: false,
                taskbar_created,
                gate: ActivateGate::default(),
            }),
        });
        // The window holds one strong reference, released on WM_NCDESTROY.
        let raw = Rc::into_raw(shared.clone());
        // SAFETY: stores the pointer in our own window's user data.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize) };
        {
            let mut state = shared.state.borrow_mut();
            update_icon(&mut state, true);
            add_icon(&mut state);
        }
        Ok(NotifyIcon { hwnd: hwnd_raw(hwnd), shared: Some(shared) })
    }

    /// Replaces the bitmaps (normal / active state) and re-picks the size for the current DPI.
    pub fn set_images(&self, images: Vec<TrayIconImage>) {
        self.with_state(|state| {
            state.images = images;
            update_icon(state, true);
        });
    }

    /// Re-picks the bitmap size after a display / DPI change.
    pub fn refresh_dpi(&self) {
        self.with_state(|state| update_icon(state, false));
    }

    pub fn set_tooltip(&self, tooltip: &str) {
        self.with_state(|state| {
            if state.tooltip == tooltip {
                return;
            }
            state.tooltip = tooltip.to_string();
            if state.added {
                let mut data = base_data(state.hwnd);
                data.uFlags = NIF_TIP | NIF_SHOWTIP;
                data.szTip = tooltip_units(&state.tooltip);
                // SAFETY: a fully initialised NOTIFYICONDATAW for our icon.
                if !unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) }.as_bool() {
                    tracing::debug!(target: "tray", "tray tooltip not updated");
                }
            }
        });
    }

    /// The hidden window (owner for the muda context menu; `SetForegroundWindow` on it before `TrackPopupMenu`).
    pub fn hwnd(&self) -> Hwnd {
        self.hwnd
    }

    /// Removes the icon (`NIM_DELETE`) and destroys the window. Idempotent.
    pub fn destroy(&mut self) {
        let Some(shared) = self.shared.take() else { return };
        let hwnd = hwnd_from(self.hwnd);
        if let Ok(mut state) = shared.state.try_borrow_mut() {
            if state.added {
                let data = base_data(state.hwnd);
                // SAFETY: removes our own icon.
                let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
                state.added = false;
            }
        }
        // SAFETY: our own window on its own (this, non-Send) thread; WM_NCDESTROY releases the window's reference.
        if let Err(e) = unsafe { DestroyWindow(hwnd) } {
            tracing::debug!(target: "tray", "tray window not destroyed: {e}");
        }
        self.hwnd = 0;
        drop(shared);
    }

    fn with_state(&self, f: impl FnOnce(&mut IconState)) {
        if let Some(shared) = &self.shared {
            match shared.state.try_borrow_mut() {
                Ok(mut state) => f(&mut state),
                Err(_) => tracing::debug!(target: "tray", "tray state busy; update skipped"),
            }
        }
    }
}

impl Drop for NotifyIcon {
    fn drop(&mut self) {
        self.destroy();
    }
}

fn base_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: ICON_ID,
        ..Default::default()
    }
}

/// `NIM_ADD` + `NIM_SETVERSION(4)` with the current tooltip and image.
fn add_icon(state: &mut IconState) {
    let mut data = base_data(state.hwnd);
    data.uFlags =
        NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP | if state.hicon.is_some() { NIF_ICON } else { NOTIFY_ICON_DATA_FLAGS(0) };
    data.uCallbackMessage = TRAY_CALLBACK;
    data.hIcon = state.hicon.unwrap_or_default();
    data.szTip = tooltip_units(&state.tooltip);
    // SAFETY: a fully initialised NOTIFYICONDATAW for our icon.
    let mut ok = unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool();
    if !ok {
        // Still registered from before (e.g. a spurious TaskbarCreated): update it in place.
        // SAFETY: as above.
        ok = unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) }.as_bool();
    }
    if !ok {
        tracing::warn!(target: "tray", "tray icon could not be added; retrying when the taskbar is created");
        state.added = false;
        return;
    }
    data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    // SAFETY: as above.
    if !unsafe { Shell_NotifyIconW(NIM_SETVERSION, &data) }.as_bool() {
        tracing::warn!(target: "tray", "tray icon version 4 not accepted");
    }
    state.added = true;
}

/// Re-picks the bitmap for the current small-icon size; `force` rebuilds it even when the same image is picked (new
/// bitmaps). Updates the shell when the icon is added.
fn update_icon(state: &mut IconState, force: bool) {
    let pick = pick_image(&state.images, small_icon_size());
    if !force && pick == state.shown {
        return;
    }
    let icon = pick.and_then(|i| state.images.get(i)).and_then(create_icon);
    if pick.is_some() && icon.is_none() {
        tracing::warn!(target: "tray", "tray icon image could not be created");
    }
    let old = std::mem::replace(&mut state.hicon, icon);
    state.shown = pick;
    if state.added {
        let mut data = base_data(state.hwnd);
        data.uFlags = NIF_ICON;
        data.hIcon = state.hicon.unwrap_or_default();
        // SAFETY: a fully initialised NOTIFYICONDATAW for our icon.
        if !unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) }.as_bool() {
            tracing::debug!(target: "tray", "tray icon image not updated");
        }
    }
    if let Some(old) = old {
        // SAFETY: the shell copied the new icon; the old one is ours to free.
        let _ = unsafe { DestroyIcon(old) };
    }
}

/// `SM_CXSMICON` at the primary monitor's DPI (16 at 100 %).
fn small_icon_size() -> u32 {
    // SAFETY: plain queries with out-parameters that outlive the calls.
    unsafe {
        let monitor = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let (mut dpi, mut dpi_y) = (96u32, 96u32);
        if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi, &mut dpi_y).is_err() || dpi == 0 {
            dpi = 96;
        }
        let size = GetSystemMetricsForDpi(SM_CXSMICON, dpi);
        if size > 0 {
            size as u32
        } else {
            16
        }
    }
}

/// The image to show for a wanted size: the exact size, else the nearest larger, else the 32-px one, else the largest.
fn pick_image(images: &[TrayIconImage], wanted: u32) -> Option<usize> {
    let valid = |i: &usize| images[*i].size > 0;
    let all = || (0..images.len()).filter(valid);
    all()
        .find(|&i| images[i].size == wanted)
        .or_else(|| all().filter(|&i| images[i].size > wanted).min_by_key(|&i| images[i].size))
        .or_else(|| all().find(|&i| images[i].size == 32))
        .or_else(|| all().max_by_key(|&i| images[i].size))
}

/// An alpha icon from straight RGBA (`None` when the buffer does not match the size).
fn create_icon(image: &TrayIconImage) -> Option<HICON> {
    let size = image.size as usize;
    if size == 0 || size > 1024 || image.rgba.len() != size * size * 4 {
        return None;
    }
    let bgra = rgba_to_bgra(&image.rgba);
    let side = size as i32;
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: side,
            biHeight: -side, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    // Monochrome AND mask, all zero (the alpha channel decides); rows are WORD-aligned.
    let mask_bits = vec![0u8; size.div_ceil(16) * 2 * size];
    // SAFETY: `bits` is the DIB section's buffer of exactly size×size×4 bytes; every GDI object is deleted below.
    unsafe {
        let mut bits: *mut c_void = std::ptr::null_mut();
        let color = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        if bits.is_null() {
            let _ = DeleteObject(color.into());
            return None;
        }
        std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits as *mut u8, bgra.len());
        let mask = CreateBitmap(side, side, 1, 1, Some(mask_bits.as_ptr() as *const c_void));
        let icon = if mask.is_invalid() {
            None
        } else {
            let icon_info = ICONINFO { fIcon: BOOL(1), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
            CreateIconIndirect(&icon_info).ok()
        };
        delete_bitmap(mask);
        delete_bitmap(color);
        icon
    }
}

fn delete_bitmap(b: HBITMAP) {
    if !b.is_invalid() {
        // SAFETY: a bitmap we created and no longer use (CreateIconIndirect copied it).
        let _ = unsafe { DeleteObject(b.into()) };
    }
}

/// RGBA → BGRA (straight alpha, as icons store it).
fn rgba_to_bgra(rgba: &[u8]) -> Vec<u8> {
    rgba.as_chunks::<4>().0.iter().flat_map(|p| [p[2], p[1], p[0], p[3]]).collect()
}

/// The tooltip as `szTip`: at most 127 UTF-16 units (never ending inside a surrogate pair), NUL-terminated.
fn tooltip_units(tooltip: &str) -> [u16; 128] {
    let mut out = [0u16; 128];
    let units: Vec<u16> = tooltip.encode_utf16().collect();
    let mut n = units.len().min(127);
    if n < units.len() && n > 0 && (0xD800..0xDC00).contains(&units[n - 1]) {
        n -= 1;
    }
    out[..n].copy_from_slice(&units[..n]);
    out
}

/// Version-4 callback (`LOWORD(lParam)` = event, `wParam` = anchor x/y) → event.
fn map_tray_message(event: u32, wparam: usize) -> Option<NotifyIconEvent> {
    match event {
        NIN_SELECT | NIN_KEYSELECT | WM_LBUTTONUP | WM_LBUTTONDBLCLK => Some(NotifyIconEvent::Activate),
        WM_CONTEXTMENU => Some(NotifyIconEvent::ContextMenu {
            x: (wparam & 0xFFFF) as u16 as i16 as i32,
            y: ((wparam >> 16) & 0xFFFF) as u16 as i16 as i32,
        }),
        _ => None,
    }
}

/// Merges activations that arrive within [`ACTIVATE_MERGE`] of the last one passed.
#[derive(Debug, Default)]
struct ActivateGate {
    last: Option<Instant>,
}

impl ActivateGate {
    fn allow(&mut self, now: Instant) -> bool {
        if self.last.is_some_and(|t| now.saturating_duration_since(t) < ACTIVATE_MERGE) {
            return false;
        }
        self.last = Some(now);
        true
    }
}

unsafe extern "system" fn tray_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: our own window's user data: 0 or the pointer stored by `NotifyIcon::new`.
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const IconShared;
    if ptr.is_null() {
        // SAFETY: default handling.
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    if msg == WM_NCDESTROY {
        // SAFETY: clears the pointer, then releases the window's strong reference taken in `new`.
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Rc::from_raw(ptr));
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
    }
    // SAFETY: the window holds a strong reference until WM_NCDESTROY, so the pointer is live; this frame takes its own.
    let shared = unsafe {
        Rc::increment_strong_count(ptr);
        Rc::from_raw(ptr)
    };
    match catch_unwind(AssertUnwindSafe(|| handle_message(&shared, msg, wparam, lparam))) {
        Ok(Some(result)) => result,
        // SAFETY: default handling.
        Ok(None) => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        Err(_) => {
            tracing::warn!(target: "tray", "tray window procedure panicked");
            LRESULT(0)
        }
    }
}

fn handle_message(shared: &IconShared, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    if msg == TRAY_CALLBACK {
        if let Some(event) = map_tray_message((lparam.0 as u32) & 0xFFFF, wparam.0) {
            let pass = match event {
                NotifyIconEvent::Activate => {
                    shared.state.try_borrow_mut().map(|mut s| s.gate.allow(Instant::now())).unwrap_or(true)
                }
                NotifyIconEvent::ContextMenu { .. } => true,
            };
            if pass {
                // No borrow is held here: the handler may call back into the icon (tooltip, images, destroy).
                (shared.on_event)(event);
            }
        }
        return Some(LRESULT(0));
    }
    let taskbar_created = shared.state.try_borrow().map(|s| s.taskbar_created).unwrap_or(0);
    if taskbar_created != 0 && msg == taskbar_created {
        if let Ok(mut state) = shared.state.try_borrow_mut() {
            state.added = false;
            tracing::info!(target: "tray", "taskbar re-created; adding the tray icon again");
            add_icon(&mut state);
        }
        return Some(LRESULT(0));
    }
    if msg == WM_CLOSE {
        // An outside close request (taskkill without /f) is handled by the system watcher, which quits the app; the
        // icon's window stays until `destroy`.
        return Some(LRESULT(0));
    }
    if msg == WM_DISPLAYCHANGE || msg == WM_DPICHANGED {
        if let Ok(mut state) = shared.state.try_borrow_mut() {
            update_icon(&mut state, false);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(size: u32) -> TrayIconImage {
        TrayIconImage { size, rgba: vec![255; (size * size * 4) as usize] }
    }

    #[test]
    fn image_pick_prefers_exact_then_larger_then_32() {
        let set = vec![img(16), img(20), img(24), img(32)];
        assert_eq!(pick_image(&set, 16), Some(0));
        assert_eq!(pick_image(&set, 20), Some(1));
        assert_eq!(pick_image(&set, 18), Some(1));
        assert_eq!(pick_image(&set, 28), Some(3));
        assert_eq!(pick_image(&set, 40), Some(3));
        let no32 = vec![img(16), img(24)];
        assert_eq!(pick_image(&no32, 40), Some(1));
        let unordered = vec![img(32), img(24), img(16)];
        assert_eq!(pick_image(&unordered, 20), Some(1));
        assert_eq!(pick_image(&[], 16), None);
        assert_eq!(pick_image(&[img(0)], 16), None);
    }

    #[test]
    fn tray_messages_map_to_events() {
        assert_eq!(map_tray_message(NIN_SELECT, 0), Some(NotifyIconEvent::Activate));
        assert_eq!(map_tray_message(0x401, 0), Some(NotifyIconEvent::Activate));
        assert_eq!(map_tray_message(WM_LBUTTONUP, 0), Some(NotifyIconEvent::Activate));
        assert_eq!(map_tray_message(WM_LBUTTONDBLCLK, 0), Some(NotifyIconEvent::Activate));
        // Anchor coordinates are signed 16-bit (a monitor left of the primary has negative x).
        let wparam = ((1040u32 << 16) | (0xFFFF - 99)) as usize; // x = -100, y = 1040
        assert_eq!(map_tray_message(WM_CONTEXTMENU, wparam), Some(NotifyIconEvent::ContextMenu { x: -100, y: 1040 }));
        // Right button up/down and mouse moves are not events (the context menu comes as WM_CONTEXTMENU).
        assert_eq!(map_tray_message(0x0205, 0), None);
        assert_eq!(map_tray_message(0x0200, 0), None);
    }

    #[test]
    fn a_click_activates_once_and_a_double_click_at_most_twice() {
        let mut gate = ActivateGate::default();
        let t0 = Instant::now();
        // Click: WM_LBUTTONUP then NIN_SELECT.
        assert!(gate.allow(t0));
        assert!(!gate.allow(t0 + Duration::from_millis(2)));
        // Second click of a double click 350 ms later: WM_LBUTTONDBLCLK, WM_LBUTTONUP, NIN_SELECT.
        assert!(gate.allow(t0 + Duration::from_millis(350)));
        assert!(!gate.allow(t0 + Duration::from_millis(352)));
        assert!(!gate.allow(t0 + Duration::from_millis(355)));
        // Much later: a new activation.
        assert!(gate.allow(t0 + Duration::from_secs(2)));
    }

    #[test]
    fn tooltips_are_cut_to_127_units_without_splitting_a_pair() {
        let t = tooltip_units("Vox · Reading “x”");
        assert_eq!(crate::window::string_from_wide(&t), "Vox · Reading “x”");
        let long = "a".repeat(200);
        let t = tooltip_units(&long);
        assert_eq!(t.iter().position(|&u| u == 0), Some(127));
        // A surrogate pair straddling the cut is dropped whole.
        let s = format!("{}😀b", "a".repeat(126));
        let t = tooltip_units(&s);
        assert_eq!(t.iter().position(|&u| u == 0), Some(126));
        assert_eq!(t[127], 0);
    }

    #[test]
    fn pixels_are_swizzled_to_bgra() {
        assert_eq!(rgba_to_bgra(&[1, 2, 3, 4, 5, 6, 7, 8]), vec![3, 2, 1, 4, 7, 6, 5, 8]);
    }

    #[test]
    fn icons_are_built_from_rgba() {
        let icon = create_icon(&img(16)).expect("icon");
        // SAFETY: the icon was just created.
        unsafe { DestroyIcon(icon) }.expect("destroy");
        assert!(create_icon(&TrayIconImage { size: 16, rgba: vec![0; 10] }).is_none());
        assert!(create_icon(&TrayIconImage { size: 0, rgba: vec![] }).is_none());
        assert!(small_icon_size() >= 16);
    }

    /// Desktop checklist (D-SH-18): `cargo test -p vox-win tray_icon_keyboard_select -- --ignored --nocapture`, then
    /// within 20 s: click the icon, press Win+B, arrow to it and press Enter (→ `Activate`, not the menu), right-click
    /// it and press Shift+F10 (→ `ContextMenu`).
    #[test]
    #[ignore = "adds an icon to the notification area and pumps messages for 20 s"]
    fn tray_icon_keyboard_select() {
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
        };
        let mut icon =
            NotifyIcon::new("Vox test", vec![img(16), img(20), img(24), img(32)], Box::new(|e| println!("{e:?}")))
                .expect("tray icon");
        icon.set_tooltip("Vox · Paused “test”");
        icon.refresh_dpi();
        let end = Instant::now() + Duration::from_secs(20);
        let mut msg = MSG::default();
        while Instant::now() < end {
            // SAFETY: a plain message pump on this thread.
            unsafe {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        icon.destroy();
        icon.destroy();
        assert_eq!(icon.hwnd(), 0);
    }
}
