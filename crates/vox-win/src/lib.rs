//! # vox-win
//!
//! Everything that talks to Windows directly, except audio (vox-audio), DPAPI (vox-store) and the Slint/winit windows
//! themselves (vox-app). All entry points are thread-agnostic unless stated; long-lived services own their threads.
//!
//! | Module | Spec |
//! |---|---|
//! | [`gestures`] | selection.md §5 — the pure gesture state machine (unit-tested with the §5.8 scenarios) |
//! | [`hook`] | selection.md §4, §14 — `WH_MOUSE_LL`/`WH_KEYBOARD_LL` thread (hook procs only enqueue; `catch_unwind`) |
//! | [`selection`] | selection.md §2, §6, §8 — the selection service (hint/chip decisions, capture pipeline, restores) |
//! | [`clipboard`] | selection.md §9 — snapshot/restore (+ `CF_HDROP`, `CF_DIB`, "do not record" markers), sequence numbers, privacy markers, plain text read/write |
//! | [`keys`] | selection.md §8.4, §10 — copy method per app, `SendInput` chords, held modifiers |
//! | [`foreground`] | selection.md §7, §11 — window/process info, friendly names, password managers, text cursor, fullscreen, UIPI probe |
//! | [`hotkeys`] | shell.md §8 — `RegisterHotKey` thread (message-only window), AltGr lookup, recorder VK helpers |
//! | [`media`] | reader-playback.md §9 — System Media Transport Controls (media keys, Windows media flyout) |
//! | [`notify_icon`] | shell.md §7.3–§7.4 — the tray icon (version-4 notifications: keyboard select activates; D-SH-18) |
//! | [`registry`] | integrations.md §3–§4, shell.md §5.6 — Explorer verb, launch at login, app theme, system proxy (D-NET-1) |
//! | [`single_instance`] | shell.md §3.3 — named mutex + named pipe forwarding argv and cwd; Electron-instance detection (BRIEF.md) |
//! | [`system`] | shell.md §3.6, §5.5, §5.6 — hidden top-level window: session end, display/work-area/theme/locale/clock changes, resume; user locale (D-FMT-1) |
//! | [`window`] | shell.md §6.1 — HWND helpers (overlay subclass enforcing no-activate tool-window styles, topmost, bounds, monitors, DWM) |
//! | [`shell`] | contracts.md §5.3 — open URL, reveal folder, OS notification (+ AUMID registration), save dialog, known folders |
//!
//! Coordinates: the process is Per-Monitor-V2 DPI aware, so every point/rect in this crate is in **physical pixels**
//! ([`window::PhysRect`]); vox-app converts to DIP for the placement geometry with the monitor's scale.
//!
//! Callbacks: every Win32/WinRT entry point of this crate (hook procedures, window and subclass procedures, SMTC
//! handlers) catches panics and logs them; handlers given to the services run on the services' own threads (the tray
//! icon's and the overlay subclass's on the UI thread) and must only enqueue.
//! Owner: the vox-win engineer.

pub mod clipboard;
pub mod error;
pub mod foreground;
pub mod gestures;
pub mod hook;
pub mod hotkeys;
pub mod keys;
pub mod media;
pub mod notify_icon;
pub mod registry;
pub mod selection;
pub mod shell;
pub mod single_instance;
pub mod system;
pub mod window;

pub use error::WinError;
