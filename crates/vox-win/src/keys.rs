//! Copy method per app, synthetic chords and held modifiers (selection.md §8.4, §10).
//!
//! Injected keys go through one `SendInput` call per chord (atomic, so physical input cannot interleave), each
//! `KEYBDINPUT` carrying the virtual key **and** its scan code (`MapVirtualKeyW(vk, MAPVK_VK_TO_VSC) & 0xFF`, plus
//! `KEYEVENTF_EXTENDEDKEY` where a real keyboard sets it) so Chromium apps see a proper `KeyboardEvent.code`. Every
//! injected event is tagged with `VOX_INPUT_TAG` in `dwExtraInfo`; the input hook drops exactly those
//! (ARCHITECTURE.md D-SEL-3), so Vox's own Ctrl+C / Escape never clear a selection hint. When `SendInput` inserts
//! fewer events than asked, the missing key-ups are sent in a second call ("never leave Ctrl stuck").

#![warn(dead_code, unused)]

use std::time::{Duration, Instant};

use vox_core::constants::MODIFIER_POLL_MS;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, MAPVK_VK_TO_VSC, VIRTUAL_KEY,
};

/// `dwExtraInfo` of every event Vox injects ("VOX!"); the input hook drops events carrying it.
pub(crate) const VOX_INPUT_TAG: usize = 0x564F_5821;

/// How to copy the selection from an app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyMethod {
    /// Ctrl+C; `escape_retry` allowed for context-chip captures.
    CtrlC { escape_retry: bool },
    /// Terminals and editors with integrated terminals.
    CtrlInsert,
    /// Copy-on-select terminals (PuTTY, KiTTY, MobaXterm): send nothing, read the clipboard.
    ReadClipboard,
}

/// Consoles where Ctrl+C means SIGINT: they get Ctrl+Insert instead.
const TERMINAL_PROCESSES: [&str; 15] = [
    "windowsterminal.exe",
    "cmd.exe",
    "powershell.exe",
    "pwsh.exe",
    "conhost.exe",
    "wt.exe",
    "mintty.exe",
    "alacritty.exe",
    "wezterm-gui.exe",
    "conemu.exe",
    "conemu64.exe",
    "tabby.exe",
    "hyper.exe",
    "warp.exe",
    "fluentterminal.app.exe",
];

/// Console window classes, for hosts whose exe is not listed (or is a renamed / portable build).
const TERMINAL_CLASSES: [&str; 5] =
    ["consolewindowclass", "cascadia_hosting_window_class", "mintty", "virtualconsoleclass", "org.wezfurlong.wezterm"];

/// Copy-on-select terminals: the selection is already on the clipboard, and every chord would reach the remote shell.
const COPY_ON_SELECT_PROCESSES: [&str; 4] = ["putty.exe", "kitty.exe", "kitty_portable.exe", "mobaxterm.exe"];
const COPY_ON_SELECT_CLASSES: [&str; 2] = ["putty", "kitty"];

/// Editors with an integrated terminal (Ctrl+C with nothing selected interrupts the job; they map Ctrl+Insert).
const TERMINAL_EDITOR_PROCESSES: [&str; 8] = [
    "code.exe",
    "code - insiders.exe",
    "codium.exe",
    "vscodium.exe",
    "cursor.exe",
    "windsurf.exe",
    "zed.exe",
    "devenv.exe",
];

/// `^(idea|pycharm|…|writerside)(64)?\.exe$` (JetBrains IDEs and Android Studio).
const JETBRAINS_STEMS: [&str; 14] = [
    "idea",
    "pycharm",
    "webstorm",
    "phpstorm",
    "clion",
    "goland",
    "rider",
    "rubymine",
    "datagrip",
    "dataspell",
    "rustrover",
    "studio",
    "aqua",
    "writerside",
];

fn is_jetbrains_ide(exe: &str) -> bool {
    let Some(stem) = exe.strip_suffix(".exe") else { return false };
    let stem = stem.strip_suffix("64").unwrap_or(stem);
    JETBRAINS_STEMS.contains(&stem)
}

/// `copyMethodFor(exe, class)` (pure; lower-cased comparisons; sets from selection.md §8.4).
pub fn copy_method_for(process_name: &str, class_name: &str) -> CopyMethod {
    let exe = process_name.to_lowercase();
    let class = class_name.to_lowercase();
    if COPY_ON_SELECT_PROCESSES.contains(&exe.as_str()) || COPY_ON_SELECT_CLASSES.contains(&class.as_str()) {
        return CopyMethod::ReadClipboard;
    }
    let terminal = TERMINAL_PROCESSES.contains(&exe.as_str()) || TERMINAL_CLASSES.contains(&class.as_str());
    let editor_with_terminal = TERMINAL_EDITOR_PROCESSES.contains(&exe.as_str()) || is_jetbrains_ide(&exe);
    if terminal || editor_with_terminal {
        CopyMethod::CtrlInsert
    } else {
        CopyMethod::CtrlC { escape_retry: true }
    }
}

/// One injected key transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeyStroke {
    vk: u16,
    up: bool,
    extended: bool,
}

const VK_C: u16 = 0x43;
const VK_INSERT: u16 = 0x2D;
const VK_ESCAPE: u16 = 0x1B;
const VK_LCONTROL: u16 = 0xA2;

/// The 8 side-specific modifier VKs, polled with `GetAsyncKeyState` (authoritative; selection.md §10.3).
pub(crate) const MODIFIER_VKS: [u32; 8] = [0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0x5B, 0x5C];

/// The extended flag a real keyboard sets for the keys Vox injects (selection.md §10.1 table).
fn is_extended_vk(vk: u16) -> bool {
    matches!(vk, 0x2D | 0xA3 | 0xA5 | 0x5B | 0x5C)
}

fn stroke(vk: u16, up: bool) -> KeyStroke {
    KeyStroke { vk, up, extended: is_extended_vk(vk) }
}

/// `modifier↓ key↓ key↑ modifier↑` (the modifier is always LCtrl); `None` for the copy-on-select method.
pub(crate) fn chord_strokes(method: CopyMethod) -> Option<[KeyStroke; 4]> {
    let key = match method {
        CopyMethod::CtrlC { .. } => VK_C,
        CopyMethod::CtrlInsert => VK_INSERT,
        CopyMethod::ReadClipboard => return None,
    };
    Some([stroke(VK_LCONTROL, false), stroke(key, false), stroke(key, true), stroke(VK_LCONTROL, true)])
}

/// The key-ups among the strokes `SendInput` did not insert (it inserts a prefix): released in order, so a partial
/// send never leaves a key (above all Ctrl) held.
pub(crate) fn missing_releases(strokes: &[KeyStroke], inserted: usize) -> Vec<KeyStroke> {
    strokes.iter().skip(inserted).filter(|s| s.up).copied().collect()
}

fn to_input(s: &KeyStroke) -> INPUT {
    // SAFETY: a pure table lookup.
    let scan = unsafe { MapVirtualKeyW(u32::from(s.vk), MAPVK_VK_TO_VSC) } & 0xFF;
    let mut flags = 0u32;
    if s.up {
        flags |= KEYEVENTF_KEYUP.0;
    }
    if s.extended {
        flags |= KEYEVENTF_EXTENDEDKEY.0;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(s.vk),
                wScan: scan as u16,
                dwFlags: KEYBD_EVENT_FLAGS(flags),
                time: 0,
                dwExtraInfo: VOX_INPUT_TAG,
            },
        },
    }
}

/// One `SendInput` call; returns how many events were inserted.
fn send_strokes(strokes: &[KeyStroke]) -> usize {
    if strokes.is_empty() {
        return 0;
    }
    let inputs: Vec<INPUT> = strokes.iter().map(to_input).collect();
    // SAFETY: `inputs` is a valid slice of keyboard INPUTs and the size argument matches the element type.
    let n = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    n as usize
}

/// Sends strokes atomically; on a partial insert, releases what is left. True when everything was inserted.
fn send_all(strokes: &[KeyStroke]) -> bool {
    let sent = send_strokes(strokes);
    if sent < strokes.len() {
        let missing = missing_releases(strokes, sent);
        let _ = send_strokes(&missing);
        tracing::debug!(target: "selection", "SendInput inserted {sent} of {} key events", strokes.len());
        return false;
    }
    true
}

/// Sends a chord with `SendInput` (tagged with Vox's `dwExtraInfo`); false when it could not be sent.
/// `ReadClipboard` sends nothing and returns false.
pub fn send_chord(method: CopyMethod) -> bool {
    match chord_strokes(method) {
        Some(strokes) => send_all(&strokes),
        None => false,
    }
}

/// Sends a single Escape.
pub fn send_escape() -> bool {
    send_all(&[stroke(VK_ESCAPE, false), stroke(VK_ESCAPE, true)])
}

/// `GetAsyncKeyState(vk) & 0x8000`: the key is physically down right now.
pub(crate) fn key_down(vk: u32) -> bool {
    // SAFETY: a plain value argument.
    let state = unsafe { GetAsyncKeyState(vk as i32) };
    (state as u16) & 0x8000 != 0
}

/// The side-specific modifiers held right now.
pub(crate) fn held_modifier_vks() -> Vec<u32> {
    MODIFIER_VKS.into_iter().filter(|&vk| key_down(vk)).collect()
}

/// Synthetic key-ups for the given VKs (failures ignored: the next physical press resyncs the key).
pub(crate) fn release_keys(vks: &[u32]) {
    let strokes: Vec<KeyStroke> =
        vks.iter().filter_map(|&vk| u16::try_from(vk).ok()).map(|vk| stroke(vk, true)).collect();
    let _ = send_strokes(&strokes);
}

/// `waitForModifiersReleased` over injected time and key state (selection.md §10.3): while a modifier is held and
/// the deadline has not passed, sleep `poll_ms` and look again; `aborted` is checked only right after each sleep and
/// returns `[]` without releasing anything; at the deadline the still-held keys are released and returned.
pub(crate) fn wait_released_with(
    max_ms: u64,
    poll_ms: u64,
    aborted: &dyn Fn() -> bool,
    held: &mut dyn FnMut() -> Vec<u32>,
    release: &mut dyn FnMut(&[u32]),
    now: &dyn Fn() -> u64,
    sleep: &dyn Fn(u64),
) -> Vec<u32> {
    let deadline = now().saturating_add(max_ms);
    let mut keys = held();
    while !keys.is_empty() && now() < deadline {
        sleep(poll_ms);
        if aborted() {
            return Vec::new();
        }
        keys = held();
    }
    if !keys.is_empty() {
        release(&keys);
    }
    keys
}

/// Waits up to `max_ms` (polling 10 ms with `GetAsyncKeyState`) until no modifier is physically held; then releases
/// any still held with synthetic key-ups. Returns the VKs that had to be released.
pub fn wait_modifiers_released(max_ms: u64, aborted: &dyn Fn() -> bool) -> Vec<u32> {
    let start = Instant::now();
    let now = || u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    let sleep = |ms: u64| std::thread::sleep(Duration::from_millis(ms));
    wait_released_with(
        max_ms,
        MODIFIER_POLL_MS,
        aborted,
        &mut held_modifier_vks,
        &mut |k| release_keys(k),
        &now,
        &sleep,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[test]
    fn copy_method_table() {
        let esc = CopyMethod::CtrlC { escape_retry: true };
        let rows: [(&str, &str, CopyMethod); 24] = [
            ("chrome.exe", "Chrome_WidgetWin_1", esc),
            ("notepad.exe", "Notepad", esc),
            ("", "", esc),
            ("WindowsTerminal.exe", "CASCADIA_HOSTING_WINDOW_CLASS", CopyMethod::CtrlInsert),
            ("cmd.exe", "", CopyMethod::CtrlInsert),
            ("conhost.exe", "", CopyMethod::CtrlInsert),
            ("fluentterminal.app.exe", "", CopyMethod::CtrlInsert),
            ("renamed.exe", "ConsoleWindowClass", CopyMethod::CtrlInsert),
            ("x.exe", "org.wezfurlong.wezterm", CopyMethod::CtrlInsert),
            ("x.exe", "VirtualConsoleClass", CopyMethod::CtrlInsert),
            ("Code.exe", "Chrome_WidgetWin_1", CopyMethod::CtrlInsert),
            ("code - insiders.exe", "", CopyMethod::CtrlInsert),
            ("devenv.exe", "", CopyMethod::CtrlInsert),
            ("zed.exe", "", CopyMethod::CtrlInsert),
            ("idea64.exe", "SunAwtFrame", CopyMethod::CtrlInsert),
            ("pycharm.exe", "", CopyMethod::CtrlInsert),
            ("studio64.exe", "", CopyMethod::CtrlInsert),
            ("writerside64.exe", "", CopyMethod::CtrlInsert),
            ("putty.exe", "PuTTY", CopyMethod::ReadClipboard),
            ("renamed.exe", "PuTTY", CopyMethod::ReadClipboard),
            ("KiTTY_portable.exe", "", CopyMethod::ReadClipboard),
            ("mobaxterm.exe", "", CopyMethod::ReadClipboard),
            // Copy-on-select wins over a terminal class.
            ("putty.exe", "ConsoleWindowClass", CopyMethod::ReadClipboard),
            ("ideaxx.exe", "", esc),
        ];
        for (exe, class, want) in rows {
            assert_eq!(copy_method_for(exe, class), want, "{exe} / {class}");
        }
    }

    #[test]
    fn jetbrains_regex_equivalent() {
        for exe in ["idea.exe", "idea64.exe", "rider64.exe", "rustrover.exe", "aqua64.exe", "datagrip.exe"] {
            assert!(is_jetbrains_ide(exe), "{exe}");
        }
        for exe in ["idea6464.exe", "idea", "idea64", "64.exe", "xidea.exe", "idea32.exe", "idea64.exe.bak"] {
            assert!(!is_jetbrains_ide(exe), "{exe}");
        }
    }

    #[test]
    fn chords_press_lctrl_then_the_key_and_release_in_reverse() {
        let c = chord_strokes(CopyMethod::CtrlC { escape_retry: false }).unwrap();
        assert_eq!(
            c,
            [
                KeyStroke { vk: 0xA2, up: false, extended: false },
                KeyStroke { vk: 0x43, up: false, extended: false },
                KeyStroke { vk: 0x43, up: true, extended: false },
                KeyStroke { vk: 0xA2, up: true, extended: false },
            ]
        );
        let ins = chord_strokes(CopyMethod::CtrlInsert).unwrap();
        assert_eq!(ins[1], KeyStroke { vk: 0x2D, up: false, extended: true });
        assert_eq!(ins[2], KeyStroke { vk: 0x2D, up: true, extended: true });
        assert_eq!(chord_strokes(CopyMethod::ReadClipboard), None);
        assert!(!send_chord(CopyMethod::ReadClipboard));
    }

    #[test]
    fn a_partial_send_input_sends_the_missing_key_ups() {
        let c = chord_strokes(CopyMethod::CtrlC { escape_retry: true }).unwrap();
        let ups = |v: Vec<KeyStroke>| v.iter().map(|s| (s.vk, s.up)).collect::<Vec<_>>();
        assert_eq!(ups(missing_releases(&c, 4)), vec![]);
        assert_eq!(ups(missing_releases(&c, 3)), vec![(0xA2, true)]);
        assert_eq!(ups(missing_releases(&c, 2)), vec![(0x43, true), (0xA2, true)]);
        // Ctrl went down, C did not: both key-ups still go out (the original's `finally` releases).
        assert_eq!(ups(missing_releases(&c, 1)), vec![(0x43, true), (0xA2, true)]);
        assert_eq!(ups(missing_releases(&c, 0)), vec![(0x43, true), (0xA2, true)]);
    }

    #[test]
    fn extended_flags_follow_a_real_keyboard() {
        for vk in [0x2D, 0xA3, 0xA5, 0x5B, 0x5C] {
            assert!(is_extended_vk(vk), "{vk:#x}");
        }
        for vk in [0x43, 0x1B, 0xA0, 0xA1, 0xA2, 0xA4] {
            assert!(!is_extended_vk(vk), "{vk:#x}");
        }
    }

    /// A fake clock where `sleep` advances time and the held keys come from a script keyed by time.
    struct Clock {
        t: Cell<u64>,
        sleeps: Cell<u32>,
    }

    fn run_wait(
        max: u64,
        held_until: Option<u64>,
        abort_after_sleeps: Option<u32>,
    ) -> (Vec<u32>, Vec<Vec<u32>>, u64, u32) {
        let clock = Clock { t: Cell::new(1000), sleeps: Cell::new(0) };
        let released = RefCell::new(Vec::new());
        let aborted = || abort_after_sleeps.is_some_and(|n| clock.sleeps.get() >= n);
        let mut held = || match held_until {
            Some(until) if clock.t.get() < until => vec![0xA0, 0xA2],
            Some(_) => vec![],
            None => vec![0xA0, 0xA2],
        };
        let mut release = |k: &[u32]| released.borrow_mut().push(k.to_vec());
        let now = || clock.t.get();
        let sleep = |ms: u64| {
            clock.t.set(clock.t.get() + ms);
            clock.sleeps.set(clock.sleeps.get() + 1);
        };
        let out = wait_released_with(max, 10, &aborted, &mut held, &mut release, &now, &sleep);
        let released = released.into_inner();
        (out, released, clock.t.get() - 1000, clock.sleeps.get())
    }

    #[test]
    fn nothing_held_returns_at_once() {
        let (out, released, elapsed, sleeps) = run_wait(700, Some(0), None);
        assert!(out.is_empty());
        assert!(released.is_empty());
        assert_eq!((elapsed, sleeps), (0, 0));
    }

    #[test]
    fn released_in_time_needs_no_synthetic_key_ups() {
        let (out, released, elapsed, _) = run_wait(700, Some(1035), None);
        assert!(out.is_empty());
        assert!(released.is_empty());
        assert_eq!(elapsed, 40);
    }

    #[test]
    fn still_held_at_the_deadline_are_force_released() {
        let (out, released, elapsed, sleeps) = run_wait(700, None, None);
        assert_eq!(out, vec![0xA0, 0xA2]);
        assert_eq!(released, vec![vec![0xA0, 0xA2]]);
        assert_eq!(elapsed, 700);
        assert_eq!(sleeps, 70);
    }

    #[test]
    fn abort_is_checked_after_each_sleep_and_releases_nothing() {
        let (out, released, elapsed, sleeps) = run_wait(700, None, Some(1));
        assert!(out.is_empty());
        assert!(released.is_empty());
        assert_eq!((elapsed, sleeps), (10, 1));
        // An abort that is already set before the first check does not skip the first look (nothing held → []).
        let (out, released, _, sleeps) = run_wait(700, Some(0), Some(0));
        assert!(out.is_empty() && released.is_empty());
        assert_eq!(sleeps, 0);
    }

    /// `cargo test -p vox-win injected_keys_are_tagged -- --ignored` on an interactive desktop: sends a lone
    /// Escape (harmless) and checks that SendInput accepted both events.
    #[test]
    #[ignore = "injects input into the interactive desktop"]
    fn injected_keys_are_tagged() {
        assert!(send_escape());
        assert!(held_modifier_vks().len() <= MODIFIER_VKS.len());
    }
}
