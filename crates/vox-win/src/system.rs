//! System events (shell.md §3.6, §5.5, §5.6, pitfalls §12.1): a hidden **top-level** window (not `HWND_MESSAGE`, so it
//! receives broadcasts) on its own thread.
//!
//! - `WM_QUERYENDSESSION` / `WM_ENDSESSION`: calls `on_session_end` **synchronously** inside the window procedure
//!   (vox-app blocks until the core has persisted playback and flushed settings + history), never vetoes.
//! - `WM_DISPLAYCHANGE`, `WM_DPICHANGED`, `WM_SETTINGCHANGE(SPI_SETWORKAREA)` → [`SystemEvent::DisplaysChanged`].
//!   (`WM_DPICHANGED` reaches this hidden window only for its own monitor; a scale change on another monitor while the
//!   widget or chip is visible there is caught by vox-app from winit's `ScaleFactorChanged` on that window.)
//! - `WM_SETTINGCHANGE("ImmersiveColorSet")` → [`SystemEvent::ThemeChanged`].
//! - `WM_SETTINGCHANGE("intl")` → [`SystemEvent::LocaleChanged`] (vox-app re-reads [`user_locale`]).
//! - `WM_SETTINGCHANGE(SPI_SETCLIENTAREAANIMATION)` → [`SystemEvent::MotionChanged`] (vox-app re-reads
//!   `window::reduce_motion()` and publishes `UiUpdate::Motion`).
//! - `WM_TIMECHANGE` → [`SystemEvent::ClockChanged`] (day re-check, ui-main.md §1.4).
//! - `WM_POWERBROADCAST(PBT_APMRESUMEAUTOMATIC)` → [`SystemEvent::Resumed`] (timers re-check midnight / credit resets).
//! - `WM_CLOSE` from outside (taskkill without /f, installers) → [`SystemEvent::CloseRequested`] (save, then quit).
//!
//! Also `WM_SETTINGCHANGE(SPI_SETLOGICALDPIOVERRIDE)` (the display scale slider) → `DisplaysChanged`. Handlers run
//! under `catch_unwind` on the watcher thread; a panic is logged and never crosses the window procedure.

use std::cell::RefCell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::thread::{JoinHandle, ThreadId};

use vox_core::locale::{DateOrder, Locale};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Globalization::{
    GetLocaleInfoEx, LOCALE_SDECIMAL, LOCALE_SGROUPING, LOCALE_SSHORTDATE, LOCALE_STHOUSAND, LOCALE_STIMEFORMAT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, DestroyWindow, PostMessageW, PostQuitMessage, PBT_APMRESUMEAUTOMATIC, SPI_SETCLIENTAREAANIMATION,
    SPI_SETLOGICALDPIOVERRIDE, SPI_SETWORKAREA, WM_APP, WM_CLOSE, WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED,
    WM_ENDSESSION, WM_POWERBROADCAST, WM_QUERYENDSESSION, WM_SETTINGCHANGE, WM_TIMECHANGE,
};

use crate::window::{create_hidden_window, hwnd_from, hwnd_raw, pump_messages, register_class, Hwnd};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemEvent {
    DisplaysChanged,
    ThemeChanged,
    LocaleChanged,
    ClockChanged,
    Resumed,
    CloseRequested,
    /// The OS "show animations" setting changed (reduce motion).
    MotionChanged,
}

pub type SystemHandler = Arc<dyn Fn(SystemEvent) + Send + Sync>;
/// Must return only after the synchronous save. vox-app's handler sends `Command::SessionEnd(reply)` and waits ≤ 5 s;
/// if no reply arrived within 2.5 s it calls `settings.flush_sync()` and `history.flush_sync()` itself (both are
/// thread-safe and idempotent; only `persistPlayback` is skipped in that fallback), so a busy core cannot lose the
/// debounced writes at log-off (persistence.md §10.3; ARCHITECTURE.md §4.17).
pub type SessionEndHandler = Arc<dyn Fn() + Send + Sync>;

/// Posted by [`SystemWatcher::shutdown`]: destroy the window and end the thread.
const WM_APP_SHUTDOWN: u32 = WM_APP + 0x51;

pub struct SystemWatcher {
    hwnd: Hwnd,
    thread: parking_lot::Mutex<Option<JoinHandle<()>>>,
    thread_id: ThreadId,
}

struct Handlers {
    on_event: SystemHandler,
    on_session_end: SessionEndHandler,
}

thread_local! {
    /// The watcher thread's handlers (its window procedure runs on that thread).
    static HANDLERS: RefCell<Option<Handlers>> = const { RefCell::new(None) };
}

impl SystemWatcher {
    pub fn spawn(on_event: SystemHandler, on_session_end: SessionEndHandler) -> Result<SystemWatcher, crate::WinError> {
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<Hwnd, crate::WinError>>(1);
        let thread = std::thread::Builder::new()
            .name("vox-system-events".into())
            .spawn(move || watcher_thread(Handlers { on_event, on_session_end }, ready_tx))
            .map_err(|e| crate::WinError(format!("cannot start the system watcher thread: {e}")))?;
        let thread_id = thread.thread().id();
        match ready_rx.recv() {
            Ok(Ok(hwnd)) => Ok(SystemWatcher { hwnd, thread: parking_lot::Mutex::new(Some(thread)), thread_id }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(crate::WinError("the system watcher thread ended before creating its window".into()))
            }
        }
    }

    /// Destroys the hidden window and joins the thread (idempotent; from the watcher thread itself it only posts).
    pub fn shutdown(&self) {
        let Some(thread) = self.thread.lock().take() else { return };
        // SAFETY: posting to our own window; it fails only if the window is already gone.
        if let Err(e) = unsafe { PostMessageW(Some(hwnd_from(self.hwnd)), WM_APP_SHUTDOWN, WPARAM(0), LPARAM(0)) } {
            tracing::warn!(target: "system", "system watcher did not get the stop request: {e}");
            return;
        }
        if std::thread::current().id() == self.thread_id {
            return;
        }
        if thread.join().is_err() {
            tracing::warn!(target: "system", "system watcher thread panicked while stopping");
        }
    }
}

impl Drop for SystemWatcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn watcher_thread(handlers: Handlers, ready: crossbeam_channel::Sender<Result<Hwnd, crate::WinError>>) {
    let _ = HANDLERS.try_with(|h| *h.borrow_mut() = Some(handlers));
    let class = w!("VoxSystemEventsWindow");
    let hwnd = match register_class(class, Some(system_proc)).and_then(|_| create_hidden_window(class, "Vox", None)) {
        Ok(h) => h,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let _ = ready.send(Ok(hwnd_raw(hwnd)));
    pump_messages();
    let _ = HANDLERS.try_with(|h| h.borrow_mut().take());
}

fn emit(event: SystemEvent) {
    let _ = HANDLERS.try_with(|h| {
        let handler = h.try_borrow().ok().and_then(|g| g.as_ref().map(|x| x.on_event.clone()));
        if let Some(handler) = handler {
            if catch_unwind(AssertUnwindSafe(|| handler(event))).is_err() {
                tracing::warn!(target: "system", "system event handler panicked ({event:?})");
            }
        }
    });
}

fn session_end() {
    let _ = HANDLERS.try_with(|h| {
        let handler = h.try_borrow().ok().and_then(|g| g.as_ref().map(|x| x.on_session_end.clone()));
        if let Some(handler) = handler {
            if catch_unwind(AssertUnwindSafe(|| handler())).is_err() {
                tracing::warn!(target: "system", "session-end save failed (handler panicked)");
            }
        }
    });
}

unsafe extern "system" fn system_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_QUERYENDSESSION => {
            session_end();
            // Never veto the session end.
            LRESULT(1)
        }
        WM_ENDSESSION => {
            if wparam.0 != 0 {
                session_end();
            }
            LRESULT(0)
        }
        WM_DISPLAYCHANGE | WM_DPICHANGED => {
            emit(SystemEvent::DisplaysChanged);
            LRESULT(0)
        }
        WM_SETTINGCHANGE => {
            // SAFETY: for WM_SETTINGCHANGE a non-zero lparam points at a NUL-terminated area name.
            let area = unsafe { setting_area(lparam) };
            if let Some(event) = classify_setting_change(wparam.0 as u32, area.as_deref()) {
                emit(event);
            }
            LRESULT(0)
        }
        WM_TIMECHANGE => {
            emit(SystemEvent::ClockChanged);
            LRESULT(0)
        }
        WM_POWERBROADCAST => {
            if wparam.0 as u32 == PBT_APMRESUMEAUTOMATIC {
                emit(SystemEvent::Resumed);
            }
            LRESULT(1)
        }
        WM_CLOSE => {
            // Never destroyed by an outside close: the app saves and quits instead.
            emit(SystemEvent::CloseRequested);
            LRESULT(0)
        }
        WM_APP_SHUTDOWN => {
            // SAFETY: destroys our own window on its thread.
            if let Err(e) = unsafe { DestroyWindow(hwnd) } {
                tracing::warn!(target: "system", "DestroyWindow failed: {e}");
                // SAFETY: ends this thread's message loop anyway.
                unsafe { PostQuitMessage(0) };
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: default handling for everything else.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// The `WM_SETTINGCHANGE` area string (at most 256 units), if any.
///
/// # Safety
/// `lparam` must be 0 or point at a NUL-terminated UTF-16 string (the `WM_SETTINGCHANGE` contract).
unsafe fn setting_area(lparam: LPARAM) -> Option<String> {
    if lparam.0 == 0 {
        return None;
    }
    let p = lparam.0 as *const u16;
    let mut len = 0usize;
    // SAFETY: the caller guarantees a NUL-terminated string; the scan stops at the NUL or after 256 units.
    while len < 256 && unsafe { *p.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` units were just read from the same string.
    Some(String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) }))
}

/// `WM_SETTINGCHANGE` → event (pure). The area string wins (it names the change); otherwise the `SPI_SET*` action.
fn classify_setting_change(action: u32, area: Option<&str>) -> Option<SystemEvent> {
    match area {
        Some(a) if a.eq_ignore_ascii_case("ImmersiveColorSet") => return Some(SystemEvent::ThemeChanged),
        Some(a) if a.eq_ignore_ascii_case("intl") => return Some(SystemEvent::LocaleChanged),
        _ => {}
    }
    if action == SPI_SETWORKAREA.0 || action == SPI_SETLOGICALDPIOVERRIDE.0 {
        Some(SystemEvent::DisplaysChanged)
    } else if action == SPI_SETCLIENTAREAANIMATION.0 {
        Some(SystemEvent::MotionChanged)
    } else {
        None
    }
}

/// The user's number/date conventions (`GetLocaleInfoEx(LOCALE_NAME_USER_DEFAULT, …)`): `LOCALE_SDECIMAL`,
/// `LOCALE_STHOUSAND` (empty when `LOCALE_SGROUPING` is "0"), 12-hour when `LOCALE_STIMEFORMAT` contains `h` but no
/// `H`, day-first when `LOCALE_SSHORTDATE` has `d` before `M` or starts with `y`. Any failure → en-US (D-FMT-1).
pub fn user_locale() -> Locale {
    let read = |lctype: u32| -> Option<String> {
        let mut buf = [0u16; 128];
        // SAFETY: `buf` outlives the call; a null locale name = LOCALE_NAME_USER_DEFAULT.
        let n = unsafe { GetLocaleInfoEx(PCWSTR::null(), lctype, Some(&mut buf)) };
        (n > 0).then(|| crate::window::string_from_wide(&buf[..(n as usize).min(buf.len())]))
    };
    let parts = (|| {
        Some((
            read(LOCALE_SDECIMAL)?,
            read(LOCALE_STHOUSAND)?,
            read(LOCALE_SGROUPING)?,
            read(LOCALE_STIMEFORMAT)?,
            read(LOCALE_SSHORTDATE)?,
        ))
    })();
    match parts {
        Some((decimal, thousand, grouping, time, date)) => {
            locale_from_parts(&decimal, &thousand, &grouping, &time, &date)
        }
        None => {
            tracing::warn!(target: "system", "user locale unreadable; using en-US conventions");
            Locale::EN_US
        }
    }
}

/// Builds the conventions from the raw `GetLocaleInfoEx` strings (pure).
fn locale_from_parts(decimal: &str, thousand: &str, grouping: &str, time_format: &str, short_date: &str) -> Locale {
    if decimal.is_empty() {
        return Locale::EN_US;
    }
    let no_grouping = grouping.split(';').next().map(str::trim) == Some("0");
    let group = if no_grouping { "" } else { thousand };
    let time = format_letters(time_format);
    let hour12 = time.contains('h') && !time.contains('H');
    let date = format_letters(short_date);
    let day_first = match (date.find('d'), date.find('M')) {
        _ if date.starts_with('y') => true,
        (Some(d), Some(m)) => d < m,
        (Some(_), None) => true,
        _ => false,
    };
    Locale::new(decimal, group, hour12, if day_first { DateOrder::DayFirst } else { DateOrder::MonthFirst })
}

/// The pattern letters of a Windows date/time format, without quoted literals (`'h'`) and separators.
fn format_letters(format: &str) -> String {
    let mut out = String::new();
    let mut quoted = false;
    for c in format.chars() {
        if c == '\'' {
            quoted = !quoted;
        } else if !quoted && c.is_ascii_alphabetic() {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setting_changes_map_to_events() {
        assert_eq!(classify_setting_change(0, Some("ImmersiveColorSet")), Some(SystemEvent::ThemeChanged));
        assert_eq!(classify_setting_change(0, Some("intl")), Some(SystemEvent::LocaleChanged));
        assert_eq!(classify_setting_change(SPI_SETWORKAREA.0, None), Some(SystemEvent::DisplaysChanged));
        assert_eq!(classify_setting_change(SPI_SETWORKAREA.0, Some("")), Some(SystemEvent::DisplaysChanged));
        assert_eq!(classify_setting_change(SPI_SETLOGICALDPIOVERRIDE.0, None), Some(SystemEvent::DisplaysChanged));
        assert_eq!(classify_setting_change(SPI_SETCLIENTAREAANIMATION.0, None), Some(SystemEvent::MotionChanged));
        assert_eq!(classify_setting_change(0, Some("Policy")), None);
        assert_eq!(classify_setting_change(0x2B, None), None);
    }

    #[test]
    fn locales_follow_the_windows_formats() {
        let us = locale_from_parts(".", ",", "3;0", "h:mm:ss tt", "M/d/yyyy");
        assert_eq!(us, Locale::new(".", ",", true, DateOrder::MonthFirst));
        let de = locale_from_parts(",", ".", "3;0", "HH:mm:ss", "dd.MM.yyyy");
        assert_eq!(de, Locale::new(",", ".", false, DateOrder::DayFirst));
        assert_eq!(de.group_digits("1234567"), "1.234.567");
        let iso = locale_from_parts(".", " ", "3;0", "HH:mm:ss", "yyyy-MM-dd");
        assert_eq!(iso.date_order, DateOrder::DayFirst);
        // "0" grouping: no thousands separator.
        let none = locale_from_parts(".", ",", "0", "H:mm", "d/M/yy");
        assert_eq!(none.group_sep(), "");
        assert!(!none.hour12);
        // Quoted literals never count as pattern letters.
        let bg = locale_from_parts(",", "\u{A0}", "3;0", "H:mm 'h'", "d.M.yyyy 'г.'");
        assert!(!bg.hour12);
        assert_eq!(bg.date_order, DateOrder::DayFirst);
        // h and H both present → 24 h; no decimal → en-US.
        assert!(!locale_from_parts(".", ",", "3;0", "h H", "M/d").hour12);
        assert_eq!(locale_from_parts("", ",", "3;0", "h", "M/d"), Locale::EN_US);
    }

    #[test]
    fn setting_area_reads_up_to_the_nul() {
        let s: Vec<u16> = "intl\0junk".encode_utf16().collect();
        // SAFETY: a NUL-terminated buffer that outlives the call.
        assert_eq!(unsafe { setting_area(LPARAM(s.as_ptr() as isize)) }.as_deref(), Some("intl"));
        // SAFETY: zero is allowed.
        assert_eq!(unsafe { setting_area(LPARAM(0)) }, None);
    }

    /// `cargo test -p vox-win watcher_starts_and_stops -- --ignored --nocapture` (creates a hidden top-level window;
    /// change the theme or the display scale during the 5 s to see events).
    #[test]
    #[ignore = "creates a hidden top-level window on the interactive desktop"]
    fn watcher_starts_and_stops() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let watcher = SystemWatcher::spawn(
            Arc::new(move |e| {
                let _ = tx.send(e);
            }),
            Arc::new(|| println!("session end")),
        )
        .expect("spawn");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while let Ok(e) = rx.recv_deadline(deadline) {
            println!("{e:?}");
        }
        watcher.shutdown();
        watcher.shutdown();
        println!("user locale: {:?}", user_locale());
    }
}
