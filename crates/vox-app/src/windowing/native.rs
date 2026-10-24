//! The few Win32 calls the native shell needs that `vox_win` does not wrap (yet): `WM_NULL` after the tray menu,
//! finding winit's thread event target window, and the overlays' rounded window regions. Declared with `raw-dylib`, so
//! no import library is needed at link time. Everything here is best effort: a failing call is logged at debug level
//! and the caller carries on.

#[link(name = "user32", kind = "raw-dylib")]
extern "system" {
    fn PostMessageW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> i32;
    fn EnumThreadWindows(thread: u32, callback: unsafe extern "system" fn(isize, isize) -> i32, lparam: isize) -> i32;
    fn GetClassNameW(hwnd: isize, name: *mut u16, max: i32) -> i32;
    fn SetWindowRgn(hwnd: isize, region: isize, redraw: i32) -> i32;
}

#[link(name = "gdi32", kind = "raw-dylib")]
extern "system" {
    fn CreateRoundRectRgn(left: i32, top: i32, right: i32, bottom: i32, width: i32, height: i32) -> isize;
    fn DeleteObject(object: isize) -> i32;
}

#[link(name = "kernel32", kind = "raw-dylib")]
extern "system" {
    fn GetCurrentThreadId() -> u32;
}

/// `WM_NULL`.
const WM_NULL: u32 = 0;
/// The class of winit's per-thread event target window (winit 0.30 `platform_impl::windows::event_loop`).
pub const WINIT_EVENT_TARGET_CLASS: &str = "Winit Thread Event Target";

/// Posts `WM_NULL` to a window (shell.md §7.4: after `TrackPopupMenu`, so a dismissed tray menu behaves the next time).
pub fn post_null(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    // SAFETY: PostMessageW accepts any handle and fails for an invalid one.
    if unsafe { PostMessageW(hwnd, WM_NULL, 0, 0) } == 0 {
        tracing::debug!(target: "tray", "WM_NULL could not be posted");
    }
}

/// The corner diameter of a rounded window region (pure; tested): twice the radius, at least 0, at most the shorter
/// side (a pill).
pub fn region_diameter(width: i32, height: i32, radius: f64) -> i32 {
    let max = width.min(height).max(0);
    if !radius.is_finite() || radius <= 0.0 {
        return 0;
    }
    ((radius * 2.0).round() as i32).clamp(0, max)
}

/// Clips a window to a rounded rectangle of `width`×`height` physical px with corner `radius` (`SetWindowRgn`; the
/// window's own coordinates). The system owns the region afterwards.
pub fn set_round_region(hwnd: isize, width: i32, height: i32, radius: f64) {
    if hwnd == 0 || width <= 0 || height <= 0 {
        return;
    }
    let d = region_diameter(width, height, radius);
    // SAFETY: plain GDI region creation; `+ 1` because the right and bottom edges are exclusive.
    let region = unsafe { CreateRoundRectRgn(0, 0, width + 1, height + 1, d, d) };
    if region == 0 {
        tracing::debug!(target: "window", "rounded region not created");
        return;
    }
    // SAFETY: on success the system owns `region`; on failure it is still ours and deleted.
    unsafe {
        if SetWindowRgn(hwnd, region, 1) == 0 {
            DeleteObject(region);
            tracing::debug!(target: "window", "rounded region not applied");
        }
    }
}

/// The top-level windows of the calling thread whose class name is `class` (e.g. winit's event target window).
pub fn thread_windows_of_class(class: &str) -> Vec<isize> {
    struct Search {
        class: Vec<u16>,
        found: Vec<isize>,
    }
    unsafe extern "system" fn visit(hwnd: isize, lparam: isize) -> i32 {
        // SAFETY: `lparam` is the `&mut Search` passed below, alive for the whole enumeration.
        let search = unsafe { &mut *(lparam as *mut Search) };
        let mut buf = [0u16; 128];
        // SAFETY: the buffer and its length match.
        let len = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
        if len > 0 && buf[..len as usize] == search.class[..] {
            search.found.push(hwnd);
        }
        1
    }
    let mut search = Search { class: class.encode_utf16().collect(), found: Vec::new() };
    // SAFETY: the callback only touches `search`, which outlives the synchronous enumeration.
    unsafe {
        EnumThreadWindows(GetCurrentThreadId(), visit, (&mut search as *mut Search) as isize);
    }
    search.found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harmless_on_bad_handles() {
        post_null(0);
        set_round_region(0, 10, 10, 2.0);
        assert_eq!(region_diameter(475, 248, 30.0), 60);
        assert_eq!(region_diameter(510, 72, 36.0), 72, "a pill");
        assert_eq!(region_diameter(510, 72, 90.0), 72, "never more than the shorter side");
        assert_eq!(region_diameter(10, 10, f64::NAN), 0);
        assert_eq!(region_diameter(-1, 10, 4.0), 0);
        // A test thread owns no windows.
        assert!(thread_windows_of_class(WINIT_EVENT_TARGET_CLASS).is_empty());
    }
}
