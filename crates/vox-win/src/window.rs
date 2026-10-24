//! HWND helpers for the Slint/winit windows (shell.md §6.1). Callable from any thread (they only use thread-agnostic
//! Win32 calls); vox-app calls them on the UI thread. All geometry is physical pixels.
//!
//! Overlay windows (widget, chip; ARCHITECTURE.md §3.4, D-SH-2/D-SH-3): **visibility always goes through Slint**
//! (`window.show()` / `window.hide()`) so Slint's own state and rendering stay in step; this module never shows or
//! hides them. winit rewrites `GWL_STYLE`/`GWL_EXSTYLE` from its own flags on every flag change (visibility, topmost,
//! cursor hit-test — `WindowFlags::apply_diff` in winit 0.30), so overlay styles are not "applied once": they are
//! **enforced** by [`install_overlay_subclass`]. Click-through during the chip's exit uses winit
//! `Window::set_cursor_hittest(false/true)` (tracked in winit's `IGNORE_CURSOR_EVENT` flag, so it survives diffs);
//! the subclass makes the resulting `WS_EX_LAYERED` render (see below). After a Slint show, [`assert_topmost`]
//! re-asserts the z-order.
//!
//! The crate-private helpers at the end (wide strings, hidden message windows, the COM apartment guard) are shared by
//! the other service modules (hotkeys, media, tray, system watcher, shell).

use std::ffi::{c_void, OsString};
use std::os::windows::ffi::OsStringExt;
use std::panic::{catch_unwind, AssertUnwindSafe};

use windows::core::{s, w, BOOL, HRESULT, PCSTR, PCWSTR};
use windows::Win32::Foundation::{COLORREF, ERROR_CLASS_ALREADY_EXISTS, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_CAPTION_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWINDOWATTRIBUTE,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, MonitorFromPoint, MonitorFromRect, MonitorFromWindow, HDC, HMONITOR,
    MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT};
use windows::Win32::System::LibraryLoader::{
    GetModuleHandleW, GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, CreateWindowExW, DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics,
    GetWindowLongPtrW, GetWindowPlacement, GetWindowRect, IsIconic, IsWindow, IsWindowVisible, RegisterClassExW,
    SetForegroundWindow, SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPlacement, SetWindowPos, ShowWindow,
    SystemParametersInfoW, TranslateMessage, GWL_EXSTYLE, GWL_STYLE, HWND_TOP, HWND_TOPMOST, LWA_ALPHA, MA_NOACTIVATE,
    MSG, SHOW_WINDOW_CMD, SM_SWAPBUTTON, SPI_GETCLIENTAREAANIMATION, STYLESTRUCT, SWP_FRAMECHANGED, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE, SW_RESTORE, SW_SHOW, SW_SHOWMAXIMIZED, SW_SHOWMINIMIZED,
    SW_SHOWNORMAL, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WINDOWPLACEMENT, WINDOWPLACEMENT_FLAGS, WM_CLOSE,
    WM_CONTEXTMENU, WM_MOUSEACTIVATE, WM_NCDESTROY, WM_NCRBUTTONDOWN, WM_NCRBUTTONUP, WM_STYLECHANGED,
    WM_STYLECHANGING, WNDCLASSEXW, WNDPROC, WPF_RESTORETOMAXIMIZED, WS_EX_APPWINDOW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_SYSMENU,
};

/// A raw window handle (`HWND` as `isize`, as winit's `raw-window-handle` exposes it).
pub type Hwnd = isize;

/// A rectangle in physical pixels.
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
}

/// One monitor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MonitorInfo {
    pub bounds: PhysRect,
    /// `rcWork`: minus taskbar and app bars.
    pub work_area: PhysRect,
    /// DPI / 96 (`GetDpiForMonitor(MDT_EFFECTIVE_DPI)`).
    pub scale: f64,
    pub primary: bool,
    /// The `HMONITOR` (stable while the display configuration is unchanged; the shots harness reports it as the
    /// display `id`).
    pub id: isize,
}

/// Called when an overlay receives `WM_CLOSE` (Alt+F4 is impossible without focus; this is taskkill without `/f`, an
/// installer, …). vox-app sends `Command::Window(WindowEvent::CloseRequested(kind))`.
pub type OverlayCloseHandler = std::sync::Arc<dyn Fn() + Send + Sync>;

/// The `uIdSubclass` of the overlay subclass.
const OVERLAY_SUBCLASS_ID: usize = 0x564F_5853; // "VOXS"
/// `MONITORINFO::dwFlags` bit of the primary monitor.
const MONITORINFOF_PRIMARY: u32 = 1;

/// What the overlay subclass keeps per window (owned through the subclass reference data).
struct OverlayData {
    on_close: OverlayCloseHandler,
}

/// Installs the overlay subclass (`SetWindowSubclass`, `Win32_UI_Shell`) on a widget/chip HWND, once, right after the
/// window was first created (vox-app pre-creates both overlays off-screen at startup, before any can be seen), and
/// applies the styles at once. The subclass:
/// - `WM_STYLECHANGING`: ORs `WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_TOPMOST` into a new `GWL_EXSTYLE` and strips
///   `WS_SYSMENU` (and `WS_EX_APPWINDOW`) from the new styles — so winit's style rewrites can never undo them;
/// - `WM_STYLECHANGED`: when `WS_EX_LAYERED` was just added (winit's click-through), calls
///   `SetLayeredWindowAttributes(hwnd, 0, 255, LWA_ALPHA)` so the window keeps rendering during the chip's exit;
/// - `WM_MOUSEACTIVATE` → `MA_NOACTIVATE`; `WM_NCRBUTTONDOWN/UP` and `WM_CONTEXTMENU` are swallowed (no system menu);
/// - `WM_CLOSE` → `on_close()` and not passed on (no default hide).
///
/// No DWM shadow. Manual checklist: the chip stays visible during its exit while click-through; if the Skia surface does
/// not render while layered, vox-app hides the chip at once when the exit starts (bounds already `None`).
///
/// Must be called on the thread that owns the window (`SetWindowSubclass` cannot subclass across threads); calling it
/// again replaces the close handler. The subclass removes itself on `WM_NCDESTROY`.
pub fn install_overlay_subclass(hwnd: Hwnd, on_close: OverlayCloseHandler) -> Result<(), crate::WinError> {
    let h = hwnd_from(hwnd);
    // SAFETY: IsWindow accepts any value.
    if hwnd == 0 || !unsafe { IsWindow(Some(h)) }.as_bool() {
        return Err(crate::WinError("not a window".into()));
    }
    let api = subclass_api().ok_or_else(|| crate::WinError("window subclassing (comctl32) unavailable".into()))?;
    let mut previous: usize = 0;
    // SAFETY: `previous` outlives the call; the procedure and id identify our subclass.
    let existed = unsafe { (api.get)(h, Some(overlay_proc), OVERLAY_SUBCLASS_ID, &mut previous) }.as_bool();
    let data = Box::into_raw(Box::new(OverlayData { on_close })) as usize;
    // SAFETY: `data` is a leaked Box the subclass owns from now on (freed on WM_NCDESTROY or on replacement below).
    if !unsafe { (api.set)(h, Some(overlay_proc), OVERLAY_SUBCLASS_ID, data) }.as_bool() {
        // SAFETY: the subclass did not take the box.
        drop(unsafe { Box::from_raw(data as *mut OverlayData) });
        return Err(crate::WinError("SetWindowSubclass failed (wrong thread?)".into()));
    }
    if existed && previous != 0 && previous != data {
        // SAFETY: the previous reference data was a Box<OverlayData> we installed; it is no longer referenced.
        drop(unsafe { Box::from_raw(previous as *mut OverlayData) });
    }
    apply_overlay_styles(h);
    Ok(())
}

/// The window styles an overlay must have: no system menu.
fn overlay_style(style: u32) -> u32 {
    style & !WS_SYSMENU.0
}

/// The extended styles an overlay must have: never activated, no taskbar button / Alt+Tab entry, topmost.
fn overlay_ex_style(ex: u32) -> u32 {
    (ex | WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0 | WS_EX_TOPMOST.0) & !WS_EX_APPWINDOW.0
}

/// `WS_EX_LAYERED` was just added (winit's click-through): the window needs layered attributes to keep rendering.
fn layered_added(old_ex: u32, new_ex: u32) -> bool {
    old_ex & WS_EX_LAYERED.0 == 0 && new_ex & WS_EX_LAYERED.0 != 0
}

fn apply_overlay_styles(h: HWND) {
    // SAFETY: plain window-long reads and writes on a live window owned by this thread; the subclass sees the writes.
    unsafe {
        let style = GetWindowLongPtrW(h, GWL_STYLE) as u32;
        let wanted = overlay_style(style);
        if wanted != style {
            SetWindowLongPtrW(h, GWL_STYLE, wanted as i32 as isize);
        }
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE) as u32;
        let wanted_ex = overlay_ex_style(ex);
        if wanted_ex != ex {
            SetWindowLongPtrW(h, GWL_EXSTYLE, wanted_ex as i32 as isize);
        }
        if let Err(e) =
            SetWindowPos(h, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED)
        {
            tracing::debug!(target: "window", "overlay topmost not applied: {e}");
        }
    }
}

/// The comctl32 subclass functions, resolved at run time. Linking them statically would make every binary that
/// contains this module fail to **load** without a Common-Controls-v6 manifest (comctl32 5.8 exports them by ordinal
/// only), test executables included; looked up by name first (v6) and by their fixed ordinals 410–413 otherwise.
struct SubclassApi {
    set: SetSubclassFn,
    get: GetSubclassFn,
    remove: RemoveSubclassFn,
    def: DefSubclassFn,
}

type SubclassProc = Option<unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM, usize, usize) -> LRESULT>;
type SetSubclassFn = unsafe extern "system" fn(HWND, SubclassProc, usize, usize) -> BOOL;
type GetSubclassFn = unsafe extern "system" fn(HWND, SubclassProc, usize, *mut usize) -> BOOL;
type RemoveSubclassFn = unsafe extern "system" fn(HWND, SubclassProc, usize) -> BOOL;
type DefSubclassFn = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;
type ExportFn = unsafe extern "system" fn() -> isize;

fn subclass_api() -> Option<&'static SubclassApi> {
    static API: std::sync::OnceLock<Option<SubclassApi>> = std::sync::OnceLock::new();
    API.get_or_init(|| {
        // SAFETY: loads comctl32 (honouring the process's activation context) and reads exports whose signatures are
        // the documented ones; the module is never unloaded.
        unsafe {
            let module = match GetModuleHandleW(w!("comctl32.dll")) {
                Ok(m) => m,
                Err(_) => LoadLibraryExW(w!("comctl32.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32).ok()?,
            };
            let find = |name: PCSTR, ordinal: usize| {
                GetProcAddress(module, name).or_else(|| GetProcAddress(module, PCSTR(ordinal as *const u8)))
            };
            let set = find(s!("SetWindowSubclass"), 410)?;
            let get = find(s!("GetWindowSubclass"), 411)?;
            let remove = find(s!("RemoveWindowSubclass"), 412)?;
            let def = find(s!("DefSubclassProc"), 413)?;
            Some(SubclassApi {
                set: std::mem::transmute::<ExportFn, SetSubclassFn>(set),
                get: std::mem::transmute::<ExportFn, GetSubclassFn>(get),
                remove: std::mem::transmute::<ExportFn, RemoveSubclassFn>(remove),
                def: std::mem::transmute::<ExportFn, DefSubclassFn>(def),
            })
        }
    })
    .as_ref()
}

/// `DefSubclassProc` (the subclass exists only when the API was found, so the fallback is never used in practice).
///
/// # Safety
/// Must be called from a subclass procedure with the message it received.
unsafe fn def_subclass(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match subclass_api() {
        // SAFETY: forwarded unchanged down the subclass chain.
        Some(api) => unsafe { (api.def)(hwnd, msg, wparam, lparam) },
        // SAFETY: default handling.
        None => unsafe { windows::Win32::UI::WindowsAndMessaging::DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

unsafe extern "system" fn overlay_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    data: usize,
) -> LRESULT {
    match msg {
        WM_STYLECHANGING if lparam.0 != 0 => {
            // SAFETY: for WM_STYLECHANGING, lparam points at the STYLESTRUCT the system will apply.
            let styles = unsafe { &mut *(lparam.0 as *mut STYLESTRUCT) };
            let which = wparam.0 as i32;
            if which == GWL_STYLE.0 {
                styles.styleNew = overlay_style(styles.styleNew);
            } else if which == GWL_EXSTYLE.0 {
                styles.styleNew = overlay_ex_style(styles.styleNew);
            }
            // SAFETY: forwards the (modified) message down the subclass chain.
            unsafe { def_subclass(hwnd, msg, wparam, lparam) }
        }
        WM_STYLECHANGED if lparam.0 != 0 => {
            // SAFETY: as above, for the applied styles.
            let styles = unsafe { *(lparam.0 as *const STYLESTRUCT) };
            // SAFETY: forwards the message down the subclass chain.
            let result = unsafe { def_subclass(hwnd, msg, wparam, lparam) };
            if wparam.0 as i32 == GWL_EXSTYLE.0 && layered_added(styles.styleOld, styles.styleNew) {
                // SAFETY: a live layered window.
                if let Err(e) = unsafe { SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA) } {
                    tracing::debug!(target: "window", "layered attributes not set: {e}");
                }
            }
            result
        }
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_NCRBUTTONDOWN | WM_NCRBUTTONUP | WM_CONTEXTMENU => LRESULT(0),
        WM_CLOSE => {
            if data != 0 {
                // SAFETY: the reference data is our Box<OverlayData>, alive until WM_NCDESTROY.
                let overlay = unsafe { &*(data as *const OverlayData) };
                if catch_unwind(AssertUnwindSafe(|| (overlay.on_close)())).is_err() {
                    tracing::warn!(target: "window", "overlay close handler panicked");
                }
            }
            LRESULT(0)
        }
        WM_NCDESTROY => {
            // SAFETY: removes our own subclass from the window being destroyed (on its thread).
            unsafe {
                if let Some(api) = subclass_api() {
                    let _ = (api.remove)(hwnd, Some(overlay_proc), id);
                }
            }
            if data != 0 {
                // SAFETY: the Box installed by `install_overlay_subclass`; nothing references it after removal.
                drop(unsafe { Box::from_raw(data as *mut OverlayData) });
            }
            // SAFETY: forwards the message down the chain.
            unsafe { def_subclass(hwnd, msg, wparam, lparam) }
        }
        // SAFETY: forwards everything else unchanged.
        _ => unsafe { def_subclass(hwnd, msg, wparam, lparam) },
    }
}

/// Re-asserts the z-order of a window Slint just showed: `SetWindowPos(HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE |
/// SWP_NOSIZE | SWP_NOACTIVATE)`, then with `raise` (the chip) `HWND_TOP` within the topmost band so it sits above an
/// open context menu. Never shows, hides or activates.
pub fn assert_topmost(hwnd: Hwnd, raise: bool) {
    if hwnd == 0 {
        return;
    }
    let h = hwnd_from(hwnd);
    let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE;
    // SAFETY: plain z-order changes; an invalid handle only makes the call fail.
    unsafe {
        if let Err(e) = SetWindowPos(h, Some(HWND_TOPMOST), 0, 0, 0, 0, flags) {
            tracing::debug!(target: "window", "SetWindowPos(HWND_TOPMOST) failed: {e}");
        }
        if raise {
            if let Err(e) = SetWindowPos(h, Some(HWND_TOP), 0, 0, 0, 0, flags) {
                tracing::debug!(target: "window", "SetWindowPos(HWND_TOP) failed: {e}");
            }
        }
    }
}

pub fn is_visible(hwnd: Hwnd) -> bool {
    // SAFETY: IsWindowVisible accepts any handle.
    hwnd != 0 && unsafe { IsWindowVisible(hwnd_from(hwnd)) }.as_bool()
}

/// Moves and resizes in **one** `SetWindowPos(.., SWP_NOACTIVATE | SWP_NOZORDER)` (the widget's anchored resize and
/// placement apply position and size together; Slint follows `WM_SIZE`).
pub fn set_bounds(hwnd: Hwnd, r: PhysRect) {
    if hwnd == 0 {
        return;
    }
    // SAFETY: plain move/resize; an invalid handle only makes the call fail.
    if let Err(e) =
        unsafe { SetWindowPos(hwnd_from(hwnd), None, r.x, r.y, r.width, r.height, SWP_NOACTIVATE | SWP_NOZORDER) }
    {
        tracing::debug!(target: "window", "set_bounds failed: {e}");
    }
}

pub fn window_rect(hwnd: Hwnd) -> Option<PhysRect> {
    if hwnd == 0 {
        return None;
    }
    let mut rc = RECT::default();
    // SAFETY: `rc` outlives the call.
    unsafe { GetWindowRect(hwnd_from(hwnd), &mut rc) }.ok()?;
    Some(phys_rect(&rc))
}

/// Dark title bar + caption colour (`DWMWA_USE_IMMERSIVE_DARK_MODE`, `DWMWA_CAPTION_COLOR` as 0x00BBGGRR).
/// `caption_rgb` is `0xRRGGBB` (the backdrop token); the caption colour needs Windows 11 and is skipped silently
/// elsewhere. Windows 10 before 20H1 uses the undocumented attribute 19 for dark mode.
pub fn set_caption_theme(hwnd: Hwnd, dark: bool, caption_rgb: u32) {
    if hwnd == 0 {
        return;
    }
    let h = hwnd_from(hwnd);
    let dark_value = BOOL::from(dark);
    let size = std::mem::size_of::<BOOL>() as u32;
    // SAFETY: the attribute values outlive the calls and match the documented sizes.
    unsafe {
        let ptr = &dark_value as *const BOOL as *const c_void;
        if DwmSetWindowAttribute(h, DWMWA_USE_IMMERSIVE_DARK_MODE, ptr, size).is_err() {
            let _ = DwmSetWindowAttribute(h, DWMWINDOWATTRIBUTE(19), ptr, size);
        }
        let colour = COLORREF(colorref_from_rgb(caption_rgb));
        if let Err(e) = DwmSetWindowAttribute(
            h,
            DWMWA_CAPTION_COLOR,
            &colour as *const COLORREF as *const c_void,
            std::mem::size_of::<COLORREF>() as u32,
        ) {
            tracing::debug!(target: "window", "caption colour not applied: {e}");
        }
    }
}

/// `0xRRGGBB` → `COLORREF` (`0x00BBGGRR`).
fn colorref_from_rgb(rgb: u32) -> u32 {
    let r = (rgb >> 16) & 0xFF;
    let g = (rgb >> 8) & 0xFF;
    let b = rgb & 0xFF;
    r | (g << 8) | (b << 16)
}

/// Brings a normal window forward (restore if minimized, show, `SetForegroundWindow`).
pub fn bring_to_front(hwnd: Hwnd) {
    if hwnd == 0 {
        return;
    }
    let h = hwnd_from(hwnd);
    // SAFETY: plain window-state calls; an invalid handle only makes them fail.
    unsafe {
        if IsIconic(h).as_bool() {
            let _ = ShowWindow(h, SW_RESTORE);
        } else {
            let _ = ShowWindow(h, SW_SHOW);
        }
        let _ = BringWindowToTop(h);
        if !SetForegroundWindow(h).as_bool() {
            tracing::debug!(target: "window", "SetForegroundWindow was refused (no foreground permission)");
        }
    }
}

pub fn cursor_pos() -> Option<(i32, i32)> {
    let mut p = POINT::default();
    // SAFETY: `p` outlives the call.
    unsafe { GetCursorPos(&mut p) }.ok()?;
    Some((p.x, p.y))
}

/// The left button is physically down (`GetAsyncKeyState(VK_LBUTTON)`), used to detect the end of a widget drag.
/// With swapped mouse buttons (`SM_SWAPBUTTON`) the primary button is the physical right one, so `VK_RBUTTON` is read
/// instead (a drag is always made with the primary button).
pub fn left_button_down() -> bool {
    // SAFETY: no pointers involved.
    unsafe {
        let vk = if GetSystemMetrics(SM_SWAPBUTTON) != 0 { VK_RBUTTON } else { VK_LBUTTON };
        (GetAsyncKeyState(vk.0 as i32) as u16) & 0x8000 != 0
    }
}

/// Monitor nearest a point.
pub fn monitor_from_point(x: i32, y: i32) -> Option<MonitorInfo> {
    // SAFETY: no pointers involved.
    let m = unsafe { MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST) };
    monitor_info(m)
}

/// Monitor best matching a rect.
pub fn monitor_from_rect(r: PhysRect) -> Option<MonitorInfo> {
    let rc = RECT { left: r.x, top: r.y, right: r.x.saturating_add(r.width), bottom: r.y.saturating_add(r.height) };
    // SAFETY: `rc` outlives the call.
    let m = unsafe { MonitorFromRect(&rc, MONITOR_DEFAULTTONEAREST) };
    monitor_info(m)
}

/// The primary monitor.
pub fn primary_monitor() -> Option<MonitorInfo> {
    // SAFETY: no pointers involved.
    let m = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
    monitor_info(m)
}

/// Every monitor (`EnumDisplayMonitors`), primary first. Used for the Electron-DIP conversion of
/// `widget.customX/Y` (vox-app `windowing::dpi`) and to validate `window-state.json`.
pub fn monitors() -> Vec<MonitorInfo> {
    let mut handles: Vec<HMONITOR> = Vec::new();
    // SAFETY: the callback only pushes into `handles`, which outlives the synchronous enumeration.
    let ok = unsafe {
        EnumDisplayMonitors(None, None, Some(collect_monitor), LPARAM(&mut handles as *mut Vec<HMONITOR> as isize))
    };
    if !ok.as_bool() {
        tracing::debug!(target: "window", "EnumDisplayMonitors failed");
    }
    let mut out: Vec<MonitorInfo> = handles.into_iter().filter_map(monitor_info).collect();
    // Stable: keeps the enumeration order among the non-primary monitors.
    out.sort_by_key(|m| !m.primary);
    out
}

unsafe extern "system" fn collect_monitor(m: HMONITOR, _dc: HDC, _rc: *mut RECT, data: LPARAM) -> BOOL {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if data.0 != 0 {
            // SAFETY: `data` is the `&mut Vec<HMONITOR>` passed by `monitors()` for this synchronous call.
            let list = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
            list.push(m);
        }
    }));
    BOOL::from(result.is_ok())
}

fn monitor_info(m: HMONITOR) -> Option<MonitorInfo> {
    if m.is_invalid() {
        return None;
    }
    let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
    // SAFETY: `mi` is a properly sized MONITORINFO.
    if !unsafe { GetMonitorInfoW(m, &mut mi) }.as_bool() {
        return None;
    }
    let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
    // SAFETY: the out-parameters outlive the call.
    if unsafe { GetDpiForMonitor(m, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }.is_err() || dpi_x == 0 {
        dpi_x = 96;
    }
    Some(MonitorInfo {
        bounds: phys_rect(&mi.rcMonitor),
        work_area: phys_rect(&mi.rcWork),
        scale: dpi_x as f64 / 96.0,
        primary: mi.dwFlags & MONITORINFOF_PRIMARY != 0,
        id: m.0 as isize,
    })
}

/// The restored (normal) rectangle of a top-level window in screen pixels and whether it is maximized
/// (`GetWindowPlacement`, `rcNormalPosition` converted from workspace to screen coordinates).
pub fn normal_placement(hwnd: Hwnd) -> Option<(PhysRect, bool)> {
    if hwnd == 0 {
        return None;
    }
    let h = hwnd_from(hwnd);
    let mut wp = WINDOWPLACEMENT { length: std::mem::size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
    // SAFETY: `wp` is a properly sized WINDOWPLACEMENT.
    unsafe { GetWindowPlacement(h, &mut wp) }.ok()?;
    let normal = phys_rect(&wp.rcNormalPosition);
    let screen = match workspace_offset(h) {
        Some(offset) => workspace_to_screen(normal, offset),
        None => normal,
    };
    let maximized = wp.showCmd == SW_SHOWMAXIMIZED.0 as u32
        || (wp.showCmd == SW_SHOWMINIMIZED.0 as u32 && wp.flags.0 & WPF_RESTORETOMAXIMIZED.0 != 0);
    Some((screen, maximized))
}

/// Sets the restored (normal) rectangle (`SetWindowPlacement`) and, with `show = true`, shows the window maximized
/// (`SW_SHOWMAXIMIZED`) or normal (`SW_SHOWNORMAL`).
///
/// `show = false` keeps the window hidden (`SW_HIDE`) and sets **only** the restored rectangle: `maximized` cannot be
/// recorded on a hidden window. Windows honours `WPF_RESTORETOMAXIMIZED` only with `SW_SHOWMINIMIZED` and drops it
/// with `SW_HIDE`, so a later `ShowWindow`/Slint `show()` would open the window at its normal size anyway. A caller
/// that restores a saved maximized state on a hidden window therefore maximizes it right after its first show
/// (winit `set_maximized(true)`, as vox-app's main window does, or this function with `show = true`); never with
/// `SW_SHOWNOACTIVATE`, which restores a maximized window to its normal size.
pub fn set_normal_placement(hwnd: Hwnd, normal: PhysRect, maximized: bool, show: bool) {
    if hwnd == 0 {
        return;
    }
    let h = hwnd_from(hwnd);
    let rect = match workspace_offset(h) {
        Some(offset) => screen_to_workspace(normal, offset),
        None => normal,
    };
    let (show_cmd, flags) = placement_command(maximized, show);
    let wp = WINDOWPLACEMENT {
        length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
        flags,
        showCmd: show_cmd.0 as u32,
        rcNormalPosition: RECT {
            left: rect.x,
            top: rect.y,
            right: rect.x.saturating_add(rect.width),
            bottom: rect.y.saturating_add(rect.height),
        },
        ..Default::default()
    };
    // SAFETY: `wp` is a properly sized WINDOWPLACEMENT.
    if let Err(e) = unsafe { SetWindowPlacement(h, &wp) } {
        tracing::debug!(target: "window", "SetWindowPlacement failed: {e}");
    }
}

/// `showCmd` and flags for [`set_normal_placement`]. `WPF_RESTORETOMAXIMIZED` is never set: it only means something
/// with `SW_SHOWMINIMIZED` (which this function never uses), and with `SW_HIDE` Windows silently drops it.
fn placement_command(maximized: bool, show: bool) -> (SHOW_WINDOW_CMD, WINDOWPLACEMENT_FLAGS) {
    let show_cmd = match (show, maximized) {
        (false, _) => SW_HIDE,
        (true, true) => SW_SHOWMAXIMIZED,
        (true, false) => SW_SHOWNORMAL,
    };
    (show_cmd, WINDOWPLACEMENT_FLAGS(0))
}

/// Workspace → screen offset (`rcWork − rcMonitor` of the window's monitor) for top-level windows without
/// `WS_EX_TOOLWINDOW` (whose placement rectangles are in workspace coordinates); `None` for tool windows.
fn workspace_offset(h: HWND) -> Option<(i32, i32)> {
    // SAFETY: plain reads on a window handle.
    let ex = unsafe { GetWindowLongPtrW(h, GWL_EXSTYLE) } as u32;
    if ex & WS_EX_TOOLWINDOW.0 != 0 {
        return None;
    }
    // SAFETY: no pointers involved.
    let m = monitor_info(unsafe { MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST) })?;
    Some((m.work_area.x - m.bounds.x, m.work_area.y - m.bounds.y))
}

fn workspace_to_screen(r: PhysRect, (dx, dy): (i32, i32)) -> PhysRect {
    PhysRect { x: r.x + dx, y: r.y + dy, ..r }
}

fn screen_to_workspace(r: PhysRect, (dx, dy): (i32, i32)) -> PhysRect {
    PhysRect { x: r.x - dx, y: r.y - dy, ..r }
}

/// OS "reduce motion" (`SPI_GETCLIENTAREAANIMATION` off). Read by vox-app at startup (`UiInit`) and again on
/// `SystemEvent::MotionChanged` (published as `UiUpdate::Motion`).
pub fn reduce_motion() -> bool {
    let mut animate = BOOL(1);
    // SAFETY: `animate` is the BOOL this action writes.
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some(&mut animate as *mut BOOL as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    ok.is_ok() && !animate.as_bool()
}

fn phys_rect(rc: &RECT) -> PhysRect {
    PhysRect { x: rc.left, y: rc.top, width: rc.right - rc.left, height: rc.bottom - rc.top }
}

// ---------------------------------------------------------------------------------------------------------------
// Crate-private helpers shared by the service modules.
// ---------------------------------------------------------------------------------------------------------------

/// `Hwnd` → `HWND`.
pub(crate) fn hwnd_from(raw: Hwnd) -> HWND {
    HWND(raw as *mut c_void)
}

/// `HWND` → `Hwnd`.
pub(crate) fn hwnd_raw(h: HWND) -> Hwnd {
    h.0 as isize
}

/// A NUL-terminated UTF-16 copy of `s`.
pub(crate) fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// UTF-16 up to the first NUL (or the end) → `String` (lossy).
pub(crate) fn string_from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&u| u == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// UTF-16 up to the first NUL (or the end) → `OsString` (exact).
pub(crate) fn os_string_from_wide(buf: &[u16]) -> OsString {
    let end = buf.iter().position(|&u| u == 0).unwrap_or(buf.len());
    OsString::from_wide(&buf[..end])
}

/// Registers a window class for this module's hidden windows (idempotent: an existing class is fine).
pub(crate) fn register_class(name: PCWSTR, proc_: WNDPROC) -> Result<(), crate::WinError> {
    // SAFETY: the module handle of the executable (never freed).
    let instance = unsafe { GetModuleHandleW(None) }.map_err(crate::WinError::from)?;
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: proc_,
        hInstance: instance.into(),
        lpszClassName: name,
        ..Default::default()
    };
    // SAFETY: `class` and the static class name outlive the call.
    if unsafe { RegisterClassExW(&class) } == 0 {
        let e = windows::core::Error::from_thread();
        if e.code() != HRESULT::from_win32(ERROR_CLASS_ALREADY_EXISTS.0) {
            return Err(crate::WinError(format!("RegisterClassExW failed: {}", e.message())));
        }
    }
    Ok(())
}

/// Creates a hidden window of a registered class: a **top-level** popup (receives broadcasts such as
/// `WM_SETTINGCHANGE`, `WM_QUERYENDSESSION`, `TaskbarCreated`) or, with `parent = Some(HWND_MESSAGE)`, a
/// message-only window. Never shown, never in the taskbar.
pub(crate) fn create_hidden_window(class: PCWSTR, title: &str, parent: Option<HWND>) -> Result<HWND, crate::WinError> {
    // SAFETY: the module handle of the executable (never freed).
    let instance = unsafe { GetModuleHandleW(None) }.map_err(crate::WinError::from)?;
    let title = wide(title);
    // SAFETY: the class is registered and the strings outlive the call.
    unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class,
            PCWSTR(title.as_ptr()),
            WS_POPUP,
            0,
            0,
            0,
            0,
            parent,
            None,
            Some(instance.into()),
            None,
        )
    }
    .map_err(|e| crate::WinError(format!("CreateWindowExW failed: {}", e.message())))
}

/// Runs the thread's message loop until `WM_QUIT`.
pub(crate) fn pump_messages() {
    let mut msg = MSG::default();
    loop {
        // SAFETY: `msg` outlives the calls; window procedures of this thread run inside DispatchMessageW.
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if r.0 == 0 || r.0 == -1 {
            break;
        }
        // SAFETY: as above.
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Keeps the process's multithreaded apartment alive for good (`CoIncrementMTAUsage`, once): WinRT calls from threads
/// that never initialised COM then run in the implicit MTA, and no thread's `CoUninitialize` can tear COM down under
/// the activation factories the `windows` crate caches process-wide.
pub(crate) fn ensure_mta() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: plain call; the cookie is intentionally never released.
        if let Err(e) = unsafe { windows::Win32::System::Com::CoIncrementMTAUsage() } {
            tracing::debug!(target: "window", "CoIncrementMTAUsage failed: {e}");
        }
    });
}

/// Keeps COM initialised on this thread for the guard's lifetime (`CoInitializeEx`), and balances it with
/// `CoUninitialize` only when this call initialised it. A thread already in another apartment
/// (`RPC_E_CHANGED_MODE`) keeps working in it.
pub(crate) struct ComGuard {
    uninit: bool,
}

impl ComGuard {
    pub(crate) fn new(mode: COINIT) -> ComGuard {
        // SAFETY: plain apartment initialisation of the calling thread.
        let hr = unsafe { CoInitializeEx(None, mode) };
        ComGuard { uninit: hr.is_ok() }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.uninit {
            // SAFETY: balances the successful CoInitializeEx of this guard on the same thread.
            unsafe { CoUninitialize() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlay_styles_add_no_activate_tool_window_topmost_and_drop_the_system_menu() {
        let winit_style = 0x16CF_0000u32 | WS_SYSMENU.0; // WS_OVERLAPPEDWINDOW-ish with the system menu
        assert_eq!(overlay_style(winit_style) & WS_SYSMENU.0, 0);
        assert_eq!(overlay_style(winit_style) | WS_SYSMENU.0, winit_style);
        let ex = WS_EX_APPWINDOW.0 | WS_EX_LAYERED.0;
        let out = overlay_ex_style(ex);
        assert_ne!(out & WS_EX_NOACTIVATE.0, 0);
        assert_ne!(out & WS_EX_TOOLWINDOW.0, 0);
        assert_ne!(out & WS_EX_TOPMOST.0, 0);
        assert_eq!(out & WS_EX_APPWINDOW.0, 0);
        // Other bits (winit's click-through) survive.
        assert_ne!(out & WS_EX_LAYERED.0, 0);
        // Idempotent.
        assert_eq!(overlay_ex_style(out), out);
        assert_eq!(overlay_style(overlay_style(winit_style)), overlay_style(winit_style));
    }

    #[test]
    fn layered_attributes_only_when_layered_was_just_added() {
        assert!(layered_added(0, WS_EX_LAYERED.0));
        assert!(!layered_added(WS_EX_LAYERED.0, WS_EX_LAYERED.0));
        assert!(!layered_added(WS_EX_LAYERED.0, 0));
        assert!(!layered_added(0, WS_EX_TOPMOST.0));
    }

    #[test]
    fn caption_colour_is_bgr() {
        assert_eq!(colorref_from_rgb(0x05070B), 0x000B_0705);
        assert_eq!(colorref_from_rgb(0xE9ECF1), 0x00F1_ECE9);
        assert_eq!(colorref_from_rgb(0xFF0000), 0x0000_00FF);
        assert_eq!(colorref_from_rgb(0xFF12_3456), 0x0056_3412);
    }

    #[test]
    fn workspace_coordinates_shift_by_the_work_area_origin() {
        // Taskbar on the left (60 px): workspace (0, 0) is screen (60, 0).
        let r = PhysRect { x: 10, y: 20, width: 300, height: 200 };
        let s = workspace_to_screen(r, (60, 0));
        assert_eq!(s, PhysRect { x: 70, y: 20, width: 300, height: 200 });
        assert_eq!(screen_to_workspace(s, (60, 0)), r);
    }

    #[test]
    fn phys_rect_contains_is_half_open() {
        let r = PhysRect { x: 10, y: 10, width: 5, height: 5 };
        assert!(r.contains(10, 10));
        assert!(r.contains(14, 14));
        assert!(!r.contains(15, 14));
        assert!(!r.contains(14, 15));
        assert_eq!(
            phys_rect(&RECT { left: -5, top: 2, right: 5, bottom: 12 }),
            PhysRect { x: -5, y: 2, width: 10, height: 10 }
        );
    }

    #[test]
    fn wide_strings_round_trip() {
        use std::os::windows::ffi::OsStrExt;
        let w = wide("Vox é");
        assert_eq!(*w.last().unwrap_or(&1), 0);
        assert_eq!(string_from_wide(&w), "Vox é");
        assert_eq!(string_from_wide(&[0x56, 0x6F, 0, 0x78]), "Vo");
        assert_eq!(os_string_from_wide(&[0xD800, 0x41]).encode_wide().collect::<Vec<_>>(), vec![0xD800, 0x41]);
    }

    /// `cargo test -p vox-win overlay_subclass_enforces_styles -- --ignored`: a hidden window (never shown) gets the
    /// subclass; style rewrites like winit's are undone, activation and the system menu are refused, `WM_CLOSE` calls
    /// the handler without destroying the window, and destroying it releases the handler.
    #[test]
    #[ignore = "creates a (hidden) window on the interactive desktop"]
    fn overlay_subclass_enforces_styles() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use windows::core::w;
        use windows::Win32::UI::WindowsAndMessaging::{
            DefWindowProcW, DestroyWindow, SendMessageW, WS_OVERLAPPEDWINDOW,
        };

        unsafe extern "system" fn plain_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
            // SAFETY: default handling.
            unsafe { DefWindowProcW(h, m, w, l) }
        }
        let class = w!("VoxOverlaySubclassTest");
        register_class(class, Some(plain_proc)).expect("class");
        let title = wide("overlay test");
        // SAFETY: a hidden test window of this thread.
        let h = unsafe {
            CreateWindowExW(
                WS_EX_APPWINDOW,
                class,
                PCWSTR(title.as_ptr()),
                WS_OVERLAPPEDWINDOW,
                -32000,
                -32000,
                100,
                40,
                None,
                None,
                None,
                None,
            )
        }
        .expect("window");
        let raw = hwnd_raw(h);
        let closes = Arc::new(AtomicUsize::new(0));
        let counter = closes.clone();
        let handler: OverlayCloseHandler = Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        install_overlay_subclass(raw, handler.clone()).expect("subclass");
        // Installing twice replaces the handler (no leak, no double subclass).
        install_overlay_subclass(raw, handler).expect("subclass again");
        // SAFETY: plain reads/writes and messages on our own window, on its thread.
        unsafe {
            let ex = GetWindowLongPtrW(h, GWL_EXSTYLE) as u32;
            assert_ne!(ex & WS_EX_NOACTIVATE.0, 0);
            assert_ne!(ex & WS_EX_TOOLWINDOW.0, 0);
            assert_eq!(ex & WS_EX_APPWINDOW.0, 0);
            assert_eq!(GetWindowLongPtrW(h, GWL_STYLE) as u32 & WS_SYSMENU.0, 0);
            // A winit-style rewrite: plain extended style, system menu back on.
            SetWindowLongPtrW(h, GWL_EXSTYLE, WS_EX_APPWINDOW.0 as isize);
            SetWindowLongPtrW(h, GWL_STYLE, (WS_OVERLAPPEDWINDOW.0 | WS_SYSMENU.0) as isize);
            let ex = GetWindowLongPtrW(h, GWL_EXSTYLE) as u32;
            assert_ne!(ex & WS_EX_NOACTIVATE.0, 0);
            assert_ne!(ex & WS_EX_TOOLWINDOW.0, 0);
            assert_eq!(GetWindowLongPtrW(h, GWL_STYLE) as u32 & WS_SYSMENU.0, 0);
            // Click-through: layered added → still a valid window (attributes set).
            SetWindowLongPtrW(h, GWL_EXSTYLE, (ex | WS_EX_LAYERED.0) as isize);
            assert_eq!(SendMessageW(h, WM_MOUSEACTIVATE, None, None).0, MA_NOACTIVATE as isize);
            assert_eq!(SendMessageW(h, WM_CLOSE, None, None).0, 0);
            assert_eq!(closes.load(Ordering::SeqCst), 1);
            assert!(IsWindow(Some(h)).as_bool(), "WM_CLOSE must not destroy an overlay");
            assert_topmost(raw, true);
            assert!(!is_visible(raw));
            set_bounds(raw, PhysRect { x: -32000, y: -32000, width: 120, height: 50 });
            assert_eq!(window_rect(raw).map(|r| (r.width, r.height)), Some((120, 50)));
            DestroyWindow(h).expect("destroy");
        }
        assert_eq!(Arc::strong_count(&closes), 1, "the subclass released its handler");
    }

    /// Review finding: `WPF_RESTORETOMAXIMIZED` is dropped with `SW_HIDE`, so a hidden placement never records it
    /// (the caller maximizes after the first show); shown placements use the plain show commands.
    #[test]
    fn placement_commands_never_rely_on_restore_to_maximized() {
        assert_eq!(placement_command(true, false), (SW_HIDE, WINDOWPLACEMENT_FLAGS(0)));
        assert_eq!(placement_command(false, false), (SW_HIDE, WINDOWPLACEMENT_FLAGS(0)));
        assert_eq!(placement_command(true, true), (SW_SHOWMAXIMIZED, WINDOWPLACEMENT_FLAGS(0)));
        assert_eq!(placement_command(false, true), (SW_SHOWNORMAL, WINDOWPLACEMENT_FLAGS(0)));
    }

    /// `cargo test -p vox-win hidden_placement_round_trips -- --ignored`: a hidden window (never shown) gets its
    /// restored rectangle, stays hidden and reads back as not maximized.
    #[test]
    #[ignore = "creates a (hidden) window on the interactive desktop"]
    fn hidden_placement_round_trips() {
        use windows::core::w;
        use windows::Win32::UI::WindowsAndMessaging::{DefWindowProcW, DestroyWindow, WS_OVERLAPPEDWINDOW};

        unsafe extern "system" fn plain_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
            // SAFETY: default handling.
            unsafe { DefWindowProcW(h, m, w, l) }
        }
        let class = w!("VoxPlacementTest");
        register_class(class, Some(plain_proc)).expect("class");
        let title = wide("placement test");
        // SAFETY: a hidden test window of this thread.
        let h = unsafe {
            CreateWindowExW(
                Default::default(),
                class,
                PCWSTR(title.as_ptr()),
                WS_OVERLAPPEDWINDOW,
                100,
                100,
                300,
                200,
                None,
                None,
                None,
                None,
            )
        }
        .expect("window");
        let raw = hwnd_raw(h);
        let work = primary_monitor().expect("primary monitor").work_area;
        let rect = PhysRect { x: work.x + 40, y: work.y + 30, width: 640, height: 480 };
        set_normal_placement(raw, rect, true, false);
        assert!(!is_visible(raw), "show = false keeps the window hidden");
        assert_eq!(normal_placement(raw), Some((rect, false)), "a hidden window cannot record 'maximized'");
        // SAFETY: our own window on this thread.
        unsafe { DestroyWindow(h) }.expect("destroy");
    }

    /// Desktop: `cargo test -p vox-win monitors_are_listed_primary_first -- --ignored --nocapture`.
    #[test]
    #[ignore = "reads the interactive desktop's monitor layout"]
    fn monitors_are_listed_primary_first() {
        let all = monitors();
        println!("{all:#?}");
        assert!(!all.is_empty());
        assert!(all[0].primary);
        assert_eq!(primary_monitor().map(|m| m.id), Some(all[0].id));
        let p = all[0].bounds;
        assert_eq!(monitor_from_point(p.x + 1, p.y + 1).map(|m| m.id), Some(all[0].id));
        assert_eq!(monitor_from_rect(p).map(|m| m.id), Some(all[0].id));
        assert!(cursor_pos().is_some());
        println!("reduce motion: {}, left button down: {}", reduce_motion(), left_button_down());
    }
}
