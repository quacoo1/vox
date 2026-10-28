//! Global hotkeys on Linux. An X11 client under Wayland cannot grab keys globally, so each hotkey becomes a GNOME
//! custom keyboard shortcut (`org.gnome.settings-daemon.plugins.media-keys`, paths `…/custom-keybindings/vox-<id>/`)
//! whose command is `<vox> --vox-hotkey <id>`. That launch hands its argv to the running copy (`single_instance`),
//! which calls [`dispatch_from_argv`] → the hotkey handler. Only Vox's own entries are ever added or removed; other
//! custom shortcuts are left alone. Registration runs on a worker thread (gsettings is slow); GNOME does not report
//! conflicts, so no registration is reported as refused.

use std::sync::Arc;

use crossbeam_channel::Sender;
use parking_lot::Mutex;
use vox_core::hotkey::{sort_failures, HotkeyAction, HotkeyFailure, ParsedAccelerator, RegistrationPlan};

pub type HotkeyHandler = Arc<dyn Fn(HotkeyAction) + Send + Sync>;

/// The argument the GNOME shortcuts pass.
pub const HOTKEY_ARG: &str = "--vox-hotkey";

const SCHEMA: &str = "org.gnome.settings-daemon.plugins.media-keys";
const LIST_KEY: &str = "custom-keybindings";
const BASE_PATH: &str = "/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/";
const OUR_PREFIX: &str = "vox-";

static HANDLER: Mutex<Option<HotkeyHandler>> = Mutex::new(None);

/// Handles a forwarded `--vox-hotkey <id>` argv. `true` = it was a hotkey launch (handled or not).
pub fn dispatch_from_argv(argv: &[String]) -> bool {
    let Some(i) = argv.iter().position(|a| a == HOTKEY_ARG) else { return false };
    if let Some(action) = argv.get(i + 1).and_then(|id| HotkeyAction::parse(id)) {
        if let Some(handler) = HANDLER.lock().clone() {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(action))).is_err() {
                tracing::warn!(target: "hotkeys", "hotkey {} handler failed", action.id());
            }
        }
    }
    true
}

enum Job {
    Apply(Vec<(HotkeyAction, ParsedAccelerator)>),
    Stop(Sender<()>),
}

pub struct HotkeyService {
    jobs: Sender<Job>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl HotkeyService {
    pub fn spawn(on_hotkey: HotkeyHandler) -> Result<HotkeyService, crate::WinError> {
        if super::run_output("gsettings", &["get", SCHEMA, LIST_KEY]).is_none() {
            return Err(crate::WinError("GNOME keyboard shortcuts are not available (gsettings)".into()));
        }
        *HANDLER.lock() = Some(on_hotkey.clone());
        let (jobs, rx) = crossbeam_channel::unbounded::<Job>();
        let thread = std::thread::Builder::new()
            .name("vox-hotkeys".into())
            .spawn(move || {
                for job in rx {
                    match job {
                        Job::Apply(hotkeys) => apply(&hotkeys),
                        Job::Stop(reply) => {
                            apply(&[]);
                            let _ = reply.send(());
                            break;
                        }
                    }
                }
            })
            .map_err(|e| crate::WinError(format!("cannot start the hotkey thread: {e}")))?;
        // The very first launch may itself come from a shortcut (Vox was not running): handle it once started.
        let own: Vec<String> = std::env::args().collect();
        if own.iter().any(|a| a == HOTKEY_ARG) {
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(1500));
                dispatch_from_argv(&own);
            });
        }
        Ok(HotkeyService { jobs, thread: Mutex::new(Some(thread)) })
    }

    pub fn register(&self, plan: &RegistrationPlan) -> Vec<HotkeyFailure> {
        let wanted: Vec<(HotkeyAction, ParsedAccelerator)> = plan.register.iter().map(|(a, _, p)| (*a, *p)).collect();
        let _ = self.jobs.send(Job::Apply(wanted));
        let mut failures = plan.failures.clone();
        sort_failures(&mut failures);
        failures
    }

    pub fn unregister_all(&self) {
        let _ = self.jobs.send(Job::Apply(Vec::new()));
    }

    /// Removes Vox's shortcuts and stops the worker (waits ≤ 5 s). Idempotent.
    pub fn shutdown(&self) {
        let Some(thread) = self.thread.lock().take() else { return };
        let (reply, done) = crossbeam_channel::bounded(1);
        if self.jobs.send(Job::Stop(reply)).is_ok() && done.recv_timeout(std::time::Duration::from_secs(5)).is_ok() {
            let _ = thread.join();
        }
        *HANDLER.lock() = None;
    }
}

impl Drop for HotkeyService {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Parses gsettings' `@as []` / `['a', 'b']` output.
fn parse_list(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\'' {
            let mut item = String::new();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => {
                        if let Some(n) = chars.next() {
                            item.push(n);
                        }
                    }
                    '\'' => break,
                    _ => item.push(c),
                }
            }
            out.push(item);
        }
    }
    out
}

fn format_list(items: &[String]) -> String {
    format!("[{}]", items.iter().map(|s| super::gvariant_str(s)).collect::<Vec<_>>().join(", "))
}

fn gsettings_set(schema: &str, key: &str, value: &str) -> bool {
    std::process::Command::new("gsettings").args(["set", schema, key, value]).status().is_ok_and(|s| s.success())
}

/// Replaces Vox's custom shortcuts with `hotkeys`.
fn apply(hotkeys: &[(HotkeyAction, ParsedAccelerator)]) {
    let Some(current) = super::run_output("gsettings", &["get", SCHEMA, LIST_KEY]) else { return };
    let mut list: Vec<String> = parse_list(&current).into_iter().filter(|p| !p.starts_with(&format!("{BASE_PATH}{OUR_PREFIX}"))).collect();
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "vox".into());
    for (action, accel) in hotkeys {
        let Some(binding) = gnome_binding(accel) else {
            tracing::warn!(target: "hotkeys", "no GNOME key name for {}", action.id());
            continue;
        };
        let path = format!("{BASE_PATH}{OUR_PREFIX}{}/", action.id());
        let schema = format!("{SCHEMA}.custom-keybinding:{path}");
        let command = format!("{} {HOTKEY_ARG} {}", shell_quote(&exe), action.id());
        let ok = gsettings_set(&schema, "name", &super::gvariant_str(&format!("Vox: {}", action.label())))
            && gsettings_set(&schema, "command", &super::gvariant_str(&command))
            && gsettings_set(&schema, "binding", &super::gvariant_str(&binding));
        if ok {
            list.push(path);
        } else {
            tracing::warn!(target: "hotkeys", "could not set the GNOME shortcut for {}", action.id());
        }
    }
    if !gsettings_set(SCHEMA, LIST_KEY, &format_list(&list)) {
        tracing::warn!(target: "hotkeys", "could not update the GNOME custom shortcut list");
    } else {
        tracing::info!(target: "hotkeys", "{} GNOME shortcut(s) registered", hotkeys.len());
    }
}

fn shell_quote(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// `Ctrl+Alt+R` (as parsed) → `<Control><Alt>r`.
fn gnome_binding(accel: &ParsedAccelerator) -> Option<String> {
    let key = keysym_name(accel.vk)?;
    let m = accel.modifiers;
    let mut s = String::new();
    if m.ctrl {
        s.push_str("<Control>");
    }
    if m.alt {
        s.push_str("<Alt>");
    }
    if m.shift {
        s.push_str("<Shift>");
    }
    if m.win {
        s.push_str("<Super>");
    }
    s.push_str(&key);
    Some(s)
}

/// Windows virtual-key code → X keysym name.
fn keysym_name(vk: u32) -> Option<String> {
    Some(match vk {
        0x41..=0x5A => char::from_u32(vk + 0x20)?.to_string(),
        0x30..=0x39 => char::from_u32(vk)?.to_string(),
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        0x60..=0x69 => format!("KP_{}", vk - 0x60),
        _ => match vk {
            0x08 => "BackSpace",
            0x09 => "Tab",
            0x0D => "Return",
            0x13 => "Pause",
            0x1B => "Escape",
            0x20 => "space",
            0x21 => "Page_Up",
            0x22 => "Page_Down",
            0x23 => "End",
            0x24 => "Home",
            0x25 => "Left",
            0x26 => "Up",
            0x27 => "Right",
            0x28 => "Down",
            0x2C => "Print",
            0x2D => "Insert",
            0x2E => "Delete",
            0x6A => "KP_Multiply",
            0x6B => "KP_Add",
            0x6D => "KP_Subtract",
            0x6E => "KP_Decimal",
            0x6F => "KP_Divide",
            0x91 => "Scroll_Lock",
            0xAD => "XF86AudioMute",
            0xAE => "XF86AudioLowerVolume",
            0xAF => "XF86AudioRaiseVolume",
            0xB0 => "XF86AudioNext",
            0xB1 => "XF86AudioPrev",
            0xB2 => "XF86AudioStop",
            0xB3 => "XF86AudioPlay",
            0xBA => "semicolon",
            0xBB => "equal",
            0xBC => "comma",
            0xBD => "minus",
            0xBE => "period",
            0xBF => "slash",
            0xC0 => "grave",
            0xDB => "bracketleft",
            0xDC => "backslash",
            0xDD => "bracketright",
            0xDE => "apostrophe",
            _ => return None,
        }
        .to_string(),
    })
}

/// What Ctrl+Alt(+Shift)+vk types: AltGr layouts are not probed on Linux (GNOME shortcuts use keysyms).
pub fn alt_gr_character(_vk: u32, _shift: bool) -> Option<String> {
    None
}

pub fn alt_gr_character_active_layout(_vk: u32, _shift: bool) -> Option<String> {
    None
}

/// Linux evdev key code (what winit reports as the scancode on Linux) → Windows virtual-key code.
pub fn vk_from_scan_code(scan: u32, _extended: bool) -> Option<u32> {
    let letters = |s: &str, first: u32| -> Option<u32> { s.bytes().zip(first..).find(|(_, code)| *code == scan).map(|(b, _)| b as u32) };
    if let Some(vk) = letters("QWERTYUIOP", 16).or_else(|| letters("ASDFGHJKL", 30)).or_else(|| letters("ZXCVBNM", 44)) {
        return Some(vk);
    }
    Some(match scan {
        2..=10 => 0x31 + (scan - 2),
        11 => 0x30,
        59..=68 => 0x70 + (scan - 59),
        87 => 0x7A,
        88 => 0x7B,
        183..=194 => 0x7C + (scan - 183),
        1 => 0x1B,
        12 => 0xBD,
        13 => 0xBB,
        14 => 0x08,
        15 => 0x09,
        26 => 0xDB,
        27 => 0xDD,
        28 | 96 => 0x0D,
        29 => 0xA2,
        97 => 0xA3,
        39 => 0xBA,
        40 => 0xDE,
        41 => 0xC0,
        42 => 0xA0,
        54 => 0xA1,
        43 => 0xDC,
        51 => 0xBC,
        52 => 0xBE,
        53 => 0xBF,
        55 => 0x6A,
        56 => 0xA4,
        100 => 0xA5,
        57 => 0x20,
        58 => 0x14,
        69 => 0x90,
        70 => 0x91,
        71 => 0x67,
        72 => 0x68,
        73 => 0x69,
        74 => 0x6D,
        75 => 0x64,
        76 => 0x65,
        77 => 0x66,
        78 => 0x6B,
        79 => 0x61,
        80 => 0x62,
        81 => 0x63,
        82 => 0x60,
        83 => 0x6E,
        86 => 0xE2,
        98 => 0x6F,
        99 => 0x2C,
        102 => 0x24,
        103 => 0x26,
        104 => 0x21,
        105 => 0x25,
        106 => 0x27,
        107 => 0x23,
        108 => 0x28,
        109 => 0x22,
        110 => 0x2D,
        111 => 0x2E,
        113 => 0xAD,
        114 => 0xAE,
        115 => 0xAF,
        119 => 0x13,
        125 => 0x5B,
        126 => 0x5C,
        127 => 0x5D,
        163 => 0xB0,
        164 => 0xB3,
        165 => 0xB1,
        166 => 0xB2,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use vox_core::hotkey::Modifiers;

    #[test]
    fn bindings_and_lists() {
        let accel = ParsedAccelerator { modifiers: Modifiers { ctrl: true, alt: true, shift: false, win: false }, vk: 0x52 };
        assert_eq!(gnome_binding(&accel).as_deref(), Some("<Control><Alt>r"));
        assert_eq!(parse_list("@as []"), Vec::<String>::new());
        let l = parse_list("['/a/custom0/', '/a/vox-stop/']");
        assert_eq!(l, vec!["/a/custom0/".to_string(), "/a/vox-stop/".to_string()]);
        assert_eq!(format_list(&l), "['/a/custom0/', '/a/vox-stop/']");
        assert_eq!(vk_from_scan_code(19, false), Some(0x52));
        assert_eq!(vk_from_scan_code(30, false), Some(0x41));
        assert_eq!(vk_from_scan_code(11, false), Some(0x30));
        assert_eq!(vk_from_scan_code(103, false), Some(0x26));
    }
}
