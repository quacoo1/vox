//! The Linux implementation of vox-win's public API (same module names and signatures as the Windows modules).
//!
//! Vox runs as an X11 client (on GNOME/Wayland through XWayland: `app::run` drops `WAYLAND_DISPLAY` before the winit
//! backend starts), because Wayland lets no client place its own windows or keep them above others, which the floating
//! player needs. Window ids are X11 window ids; coordinates are X11 root-window pixels.
//!
//! | Module | Linux mechanism |
//! |---|---|
//! | [`window`] | x11rb: geometry, EWMH state (above, skip taskbar), RandR monitors, pointer |
//! | [`clipboard`] | arboard: CLIPBOARD and PRIMARY (Mutter mirrors both between Wayland and XWayland) |
//! | [`selection`] | "Read selection" reads the PRIMARY selection; no global mouse hook, so no chip / popup |
//! | [`hotkeys`] | GNOME custom keyboard shortcuts that run `vox --vox-hotkey <action>` (forwarded to the running copy) |
//! | [`single_instance`] | `flock` lock file + Unix socket in `$XDG_RUNTIME_DIR` |
//! | [`system`] | SIGTERM/SIGINT/SIGHUP → close request; `gsettings monitor` → theme changes; libc locale |
//! | [`registry`] | autostart `.desktop` file, Nautilus script, GNOME colour scheme, proxy from the environment |
//! | [`shell`] | xdg-open, FileManager1 over gdbus, notify-send, zenity |
//! | [`media`], [`foreground`] | not available (no-ops) |

pub mod clipboard;
pub mod foreground;
pub mod hotkeys;
pub mod media;
pub mod registry;
pub mod selection;
pub mod shell;
pub mod single_instance;
pub mod system;
pub mod window;

/// Runs a program and returns its trimmed stdout when it exits successfully.
pub(crate) fn run_output(program: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(program).args(args).stderr(std::process::Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Starts a program without waiting for it (reaped on a helper thread).
pub(crate) fn spawn_detached(program: &str, args: &[&str]) -> Result<(), crate::WinError> {
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| crate::WinError(format!("cannot run {program}: {e}")))?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// A GVariant string literal (`'…'` with `\` and `'` escaped) for gsettings / gdbus.
pub(crate) fn gvariant_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\\' || c == '\'' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('\'');
    out
}

/// The tray's bitmap type (vox-app draws the icons; on Linux they go to a StatusNotifierItem, see vox-app `tray`).
pub mod notify_icon {
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct TrayIconImage {
        pub size: u32,
        pub rgba: Vec<u8>,
    }
}
