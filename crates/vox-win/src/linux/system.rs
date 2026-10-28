//! System events on Linux: SIGTERM / SIGINT / SIGHUP (logout, `kill`, Ctrl+C) → `CloseRequested` (save, then quit);
//! GNOME colour-scheme changes (`gsettings monitor`) → `ThemeChanged`; GNOME animation changes → `MotionChanged`.
//! The user locale comes from the C library (`LC_NUMERIC`, `LC_TIME`).

use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread::JoinHandle;

use parking_lot::Mutex;
use vox_core::locale::{DateOrder, Locale};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemEvent {
    DisplaysChanged,
    ThemeChanged,
    LocaleChanged,
    ClockChanged,
    Resumed,
    CloseRequested,
    MotionChanged,
}

pub type SystemHandler = Arc<dyn Fn(SystemEvent) + Send + Sync>;
pub type SessionEndHandler = Arc<dyn Fn() + Send + Sync>;

pub struct SystemWatcher {
    signals: signal_hook::iterator::Handle,
    monitor: Mutex<Option<Child>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

impl SystemWatcher {
    pub fn spawn(on_event: SystemHandler, _on_session_end: SessionEndHandler) -> Result<SystemWatcher, crate::WinError> {
        use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
        let mut signals = signal_hook::iterator::Signals::new([SIGTERM, SIGINT, SIGHUP])?;
        let handle = signals.handle();
        let mut threads = Vec::new();
        {
            let on_event = on_event.clone();
            threads.push(
                std::thread::Builder::new()
                    .name("vox-signals".into())
                    .spawn(move || {
                        for signal in signals.forever() {
                            tracing::info!(target: "system", "signal {signal}: closing");
                            on_event(SystemEvent::CloseRequested);
                        }
                    })
                    .map_err(|e| crate::WinError(format!("cannot start the signal thread: {e}")))?,
            );
        }
        let monitor = Command::new("gsettings")
            .args(["monitor", "org.gnome.desktop.interface"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok();
        let monitor = monitor.map(|mut child| {
            if let Some(out) = child.stdout.take() {
                let spawned = std::thread::Builder::new().name("vox-gsettings".into()).spawn(move || {
                    use std::io::BufRead;
                    for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                        if line.starts_with("color-scheme:") || line.starts_with("gtk-theme:") {
                            on_event(SystemEvent::ThemeChanged);
                        } else if line.starts_with("enable-animations:") {
                            on_event(SystemEvent::MotionChanged);
                        }
                    }
                });
                if let Ok(t) = spawned {
                    threads.push(t);
                }
            }
            child
        });
        Ok(SystemWatcher { signals: handle, monitor: Mutex::new(monitor), threads: Mutex::new(threads) })
    }

    pub fn shutdown(&self) {
        self.signals.close();
        if let Some(mut child) = self.monitor.lock().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        for t in self.threads.lock().drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for SystemWatcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The user's number/time/date conventions from the C library; any failure → en-US.
pub fn user_locale() -> Locale {
    // SAFETY: newlocale/nl_langinfo_l/freelocale on a locale object we own; the returned strings are copied before
    // the object is freed.
    unsafe {
        let loc = libc::newlocale(libc::LC_ALL_MASK, c"".as_ptr(), std::ptr::null_mut());
        if loc.is_null() {
            return Locale::EN_US;
        }
        let read = |item: libc::nl_item| -> String {
            let p = libc::nl_langinfo_l(item, loc);
            if p.is_null() {
                String::new()
            } else {
                std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        };
        let decimal = read(libc::RADIXCHAR);
        let group = read(libc::THOUSEP);
        let t_fmt = read(libc::T_FMT);
        let d_fmt = read(libc::D_FMT);
        libc::freelocale(loc);
        if decimal.is_empty() || t_fmt.is_empty() || d_fmt.is_empty() || (decimal == "." && t_fmt.contains("%r")) {
            // "C"/POSIX or en-US-like.
            if d_fmt == "%m/%d/%y" || d_fmt.is_empty() {
                return Locale::EN_US;
            }
        }
        let hour12 = t_fmt.contains("%I") || t_fmt.contains("%l") || t_fmt.contains("%r") || t_fmt.contains("%p");
        let day = d_fmt.find("%d").or_else(|| d_fmt.find("%e"));
        let month = d_fmt.find("%m").or_else(|| d_fmt.find("%b"));
        let date_order = match (day, month) {
            _ if d_fmt.starts_with("%Y") || d_fmt.starts_with("%y") => DateOrder::DayFirst,
            (Some(d), Some(m)) if d < m => DateOrder::DayFirst,
            _ => DateOrder::MonthFirst,
        };
        Locale { decimal, group, hour12, date_order }
    }
}
