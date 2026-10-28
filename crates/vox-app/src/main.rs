//! `vox.exe` — the only binary. See docs/ARCHITECTURE.md §2.9 for the module map and §3 for threads.
//!
//! | Module | Owner area | Runs on |
//! |---|---|---|
//! | [`app`] | bootstrap order, wiring, quit / session-end sequences | main thread (then spawns) |
//! | [`runtime`] | runtime flags, profile, exe / extension paths, version strings | — |
//! | [`command`] | `Command` (everything sent to the core), `RequestId`, `CoreHandle` | — |
//! | [`update`] | `UiUpdate` (everything the core publishes) and the `UiSink` publisher | — |
//! | [`core`] | the core thread: read controller, services, glue to every subsystem | core thread |
//! | [`ui`] | Slint windows, page controllers (view-model builders), art cache, recorder | main thread |
//! | [`windowing`] | native treatment of the three windows: styles, placement, drag, hover, DPI | main thread |
//! | [`tray`] | tray icon (`vox_win::notify_icon`) + muda menu | main thread |
//! | [`shots`] | `VOX_SHOTS_DIR` screenshot harness | main thread |
//!
//! The executable embeds its icon, VERSIONINFO and application manifest (`build.rs`, D-SH-20).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod command;
mod core;
mod runtime;
mod shots;
mod tray;
mod ui;
mod update;
mod windowing;

fn main() -> std::process::ExitCode {
    prefer_x11();
    // In shots mode: where this run's log lines start, before anything logs (`shots` docs "Console entries").
    shots::note_log_start_from_env();
    match app::run() {
        Ok(code) => code,
        Err(e) => {
            // Logging may not be up yet; stderr is the last resort.
            eprintln!("vox: fatal: {e:#}");
            std::process::ExitCode::from(1)
        }
    }
}

/// Linux: run as an X11 client (XWayland on a Wayland desktop), before any thread or window exists. Wayland lets no
/// client position its own windows or keep them above others, which the floating player and the window placement
/// need (`vox_win` Linux module docs). `VOX_WAYLAND=1` keeps a native Wayland window for testing.
#[cfg(not(windows))]
fn prefer_x11() {
    if std::env::var_os("VOX_WAYLAND").is_none() && std::env::var_os("DISPLAY").is_some() {
        std::env::remove_var("WAYLAND_DISPLAY");
        // Match the size of Wayland apps on fractional-scaling GNOME (winit and vox_win::window both read this).
        if std::env::var_os("WINIT_X11_SCALE_FACTOR").is_none() {
            if let Some(scale) = vox_win::window::gnome_native_x11_scale() {
                std::env::set_var("WINIT_X11_SCALE_FACTOR", scale.to_string());
            }
        }
    }
}

#[cfg(windows)]
fn prefer_x11() {}

#[cfg(all(test, windows, target_env = "msvc"))]
mod resource_tests {
    //! The `.res` file `build.rs` writes and the linker embeds (D-SH-20).

    fn u16_at(b: &[u8], o: usize) -> u16 {
        u16::from_le_bytes([b[o], b[o + 1]])
    }
    fn u32_at(b: &[u8], o: usize) -> u32 {
        u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
    }

    /// `(type, name, data)` of every entry (all ordinals).
    fn entries(res: &[u8]) -> Vec<(u16, u16, Vec<u8>)> {
        let mut out = Vec::new();
        let mut o = 0;
        while o + 32 <= res.len() {
            let data = u32_at(res, o) as usize;
            let header = u32_at(res, o + 4) as usize;
            assert_eq!(header, 32, "ordinal type and name");
            assert_eq!(u16_at(res, o + 8), 0xFFFF);
            assert_eq!(u16_at(res, o + 12), 0xFFFF);
            let (ty, name) = (u16_at(res, o + 10), u16_at(res, o + 14));
            out.push((ty, name, res[o + header..o + header + data].to_vec()));
            o += (header + data).div_ceil(4) * 4;
        }
        assert_eq!(o, res.len(), "entries are DWORD-aligned and fill the file");
        out
    }

    fn utf16(s: &str) -> Vec<u8> {
        s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
    }

    #[test]
    fn the_resource_file_holds_icon_version_and_manifest() {
        let res = std::fs::read(concat!(env!("OUT_DIR"), "/vox.res")).expect("build.rs writes vox.res");
        let e = entries(&res);
        assert_eq!((e[0].0, e[0].1, e[0].2.len()), (0, 0, 0), "the empty leading entry");
        let version = e.iter().find(|x| x.0 == 16).expect("RT_VERSION");
        let v = &version.2;
        assert_eq!(u16_at(v, 0) as usize, v.len(), "VS_VERSIONINFO.wLength");
        assert_eq!(u16_at(v, 2), 52, "VS_FIXEDFILEINFO size");
        let fixed = 6 + utf16("VS_VERSION_INFO\0").len();
        let fixed = fixed.div_ceil(4) * 4;
        assert_eq!(u32_at(v, fixed), 0xFEEF_04BD);
        for needle in ["ProductName", "Vox", "vox.exe", env!("CARGO_PKG_VERSION"), "040904B0", "Translation"] {
            let n = utf16(needle);
            assert!(v.windows(n.len()).any(|w| w == n.as_slice()), "{needle}");
        }
        let manifest = e.iter().find(|x| x.0 == 24).expect("RT_MANIFEST");
        let text = String::from_utf8(manifest.2.clone()).unwrap();
        assert!(text.contains("PerMonitorV2") && text.contains("Microsoft.Windows.Common-Controls"));
        // The icon group once the checked-in assets exist.
        let assets = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets").join("vox.ico");
        if assets.is_file() {
            let group = e.iter().find(|x| x.0 == 14).expect("RT_GROUP_ICON");
            let count = u16_at(&group.2, 4) as usize;
            assert_eq!(count, crate::tray::APP_ICON_SIZES.len());
            assert_eq!(e.iter().filter(|x| x.0 == 3).count(), count, "one RT_ICON per size");
            assert_eq!(group.2.len(), 6 + 14 * count);
        }
    }
}
