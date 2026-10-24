//! Global hotkeys (shell.md §8): a thread with a message-only window that owns every `RegisterHotKey` call and
//! receives `WM_HOTKEY`. Registration takes a pure plan from `vox_core::hotkey::plan_registration` (reserved /
//! duplicate / AltGr already decided) and appends `InUse` failures for refused registrations
//! (`ERROR_HOTKEY_ALREADY_REGISTERED`, F12, …). `MOD_NOREPEAT` is set (decision). The AltGr lookup for registration
//! ([`alt_gr_character`], selection.md §12.1) checks every installed keyboard layout with `ToUnicodeEx`
//! (`TOUNICODE_NO_STATE_CHANGE`); the Settings recorder's guard ([`alt_gr_character_active_layout`],
//! ui-settings-widget.md §3.4) probes the active layout only and applies the recorder's stricter "types a character"
//! rules (one printable code point that differs from the key's own character).
//!
//! `RegisterHotKey` only works for a window of the calling thread, so [`HotkeyService::register`] and
//! [`HotkeyService::unregister_all`] hand their work to the hotkey thread (a channel plus a posted wake-up message)
//! and wait for its answer (≤ 5 s; a dead or stuck thread makes every planned hotkey count as refused). The handler runs
//! on the hotkey thread under `catch_unwind` (`hotkey <action> handler failed`).

use std::cell::RefCell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::thread::{JoinHandle, ThreadId};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use vox_core::hotkey::{
    sort_failures, HotkeyAction, HotkeyFailure, HotkeyFailureReason, Modifiers, ParsedAccelerator, RegistrationPlan,
};
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyboardLayout, GetKeyboardLayoutList, MapVirtualKeyExW, RegisterHotKey, ToUnicodeEx, UnregisterHotKey, HKL,
    HOT_KEY_MODIFIERS, MAPVK_VK_TO_VSC, MAPVK_VSC_TO_VK_EX, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DefWindowProcW, DestroyWindow, PostMessageW, PostQuitMessage, HWND_MESSAGE, WM_APP, WM_DESTROY, WM_HOTKEY,
};

use crate::window::{create_hidden_window, hwnd_from, hwnd_raw, pump_messages, register_class, Hwnd};

/// Hotkey callback (called on the hotkey thread; vox-app enqueues `Command::Hotkey`).
pub type HotkeyHandler = Arc<dyn Fn(HotkeyAction) + Send + Sync>;

/// Posted to the hotkey window whenever a request was queued.
const WM_APP_REQUEST: u32 = WM_APP + 0x48;
/// How long `register` / `unregister_all` wait for the hotkey thread.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);
/// `ToUnicodeEx` flag: do not change the keyboard (dead-key) state (Windows 10 1607+).
const TOUNICODE_NO_STATE_CHANGE: u32 = 0x4;

/// The hotkey thread.
pub struct HotkeyService {
    requests: Sender<Request>,
    hwnd: Hwnd,
    thread: parking_lot::Mutex<Option<JoinHandle<()>>>,
    thread_id: ThreadId,
}

enum Request {
    /// Unregister ours, then register these; replies with the actions the OS refused.
    Register {
        hotkeys: Vec<(HotkeyAction, ParsedAccelerator)>,
        reply: Sender<Vec<HotkeyAction>>,
    },
    UnregisterAll {
        reply: Sender<()>,
    },
    Stop,
}

struct ThreadState {
    hwnd: HWND,
    handler: HotkeyHandler,
    requests: Receiver<Request>,
    /// Ids currently registered.
    registered: Vec<i32>,
}

thread_local! {
    static STATE: RefCell<Option<ThreadState>> = const { RefCell::new(None) };
}

impl HotkeyService {
    pub fn spawn(on_hotkey: HotkeyHandler) -> Result<HotkeyService, crate::WinError> {
        let (requests, rx) = crossbeam_channel::unbounded();
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<Hwnd, crate::WinError>>(1);
        let thread = std::thread::Builder::new()
            .name("vox-hotkeys".into())
            .spawn(move || hotkey_thread(on_hotkey, rx, ready_tx))
            .map_err(|e| crate::WinError(format!("cannot start the hotkey thread: {e}")))?;
        let thread_id = thread.thread().id();
        match ready_rx.recv() {
            Ok(Ok(hwnd)) => {
                Ok(HotkeyService { requests, hwnd, thread: parking_lot::Mutex::new(Some(thread)), thread_id })
            }
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(crate::WinError("the hotkey thread ended before creating its window".into()))
            }
        }
    }

    /// Unregisters ours, then registers the plan; returns the plan's failures plus `InUse` for every OS refusal,
    /// sorted into registration order with `vox_core::hotkey::sort_failures` (each action fails at most once), so
    /// `describe_failures` lists them in the order shell.md §8.6 expects.
    pub fn register(&self, plan: &RegistrationPlan) -> Vec<HotkeyFailure> {
        for f in &plan.failures {
            if f.reason == HotkeyFailureReason::InUse {
                // The plan reports an unparseable accelerator as in-use (Electron's throwing parser).
                tracing::warn!(target: "hotkeys", "invalid accelerator for {}: {}", f.action.id(), f.accelerator);
            }
        }
        let wanted: Vec<(HotkeyAction, ParsedAccelerator)> = plan.register.iter().map(|(a, _, p)| (*a, *p)).collect();
        let refused = if wanted.is_empty() {
            // Nothing to register (hotkeys off, or every one failed the plan): still unregister the previous set.
            self.unregister_all();
            Vec::new()
        } else {
            let (reply, answer) = crossbeam_channel::bounded(1);
            self.send(Request::Register { hotkeys: wanted.clone(), reply })
                .then(|| answer.recv_timeout(REPLY_TIMEOUT).ok())
                .flatten()
                .unwrap_or_else(|| {
                    tracing::warn!(target: "hotkeys", "the hotkey thread did not answer; no hotkey registered");
                    wanted.iter().map(|(a, _)| *a).collect()
                })
        };
        merge_failures(plan, &refused)
    }

    pub fn unregister_all(&self) {
        let (reply, answer) = crossbeam_channel::bounded(1);
        if self.send(Request::UnregisterAll { reply }) && answer.recv_timeout(REPLY_TIMEOUT).is_err() {
            tracing::warn!(target: "hotkeys", "the hotkey thread did not answer the unregister request");
        }
    }

    /// Unregisters everything, destroys the window and joins the thread. Idempotent.
    pub fn shutdown(&self) {
        let Some(thread) = self.thread.lock().take() else { return };
        if !self.send(Request::Stop) {
            return;
        }
        if std::thread::current().id() == self.thread_id {
            return;
        }
        if thread.join().is_err() {
            tracing::warn!(target: "hotkeys", "hotkey thread panicked while stopping");
        }
    }

    /// Queues a request and wakes the hotkey thread. False when the thread is gone.
    fn send(&self, request: Request) -> bool {
        if self.requests.send(request).is_err() {
            return false;
        }
        // SAFETY: posting to our own window; fails only once it is destroyed.
        unsafe { PostMessageW(Some(hwnd_from(self.hwnd)), WM_APP_REQUEST, WPARAM(0), LPARAM(0)) }.is_ok()
    }
}

impl Drop for HotkeyService {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The plan's failures plus `InUse` for every action the OS refused, in registration order.
fn merge_failures(plan: &RegistrationPlan, refused: &[HotkeyAction]) -> Vec<HotkeyFailure> {
    let mut failures = plan.failures.clone();
    for (action, accelerator, _) in &plan.register {
        if refused.contains(action) {
            failures.push(HotkeyFailure {
                action: *action,
                accelerator: accelerator.clone(),
                reason: HotkeyFailureReason::InUse,
            });
        }
    }
    sort_failures(&mut failures);
    failures
}

/// `RegisterHotKey` id of an action (1-based registration order).
fn hotkey_id(action: HotkeyAction) -> i32 {
    HotkeyAction::ALL.iter().position(|a| *a == action).map_or(0, |i| i as i32 + 1)
}

fn action_for_id(id: i32) -> Option<HotkeyAction> {
    id.checked_sub(1).and_then(|i| usize::try_from(i).ok()).and_then(|i| HotkeyAction::ALL.get(i).copied())
}

/// `RegisterHotKey` modifiers: the accelerator's modifiers plus `MOD_NOREPEAT` (D-HK-3).
fn hotkey_modifiers(m: Modifiers) -> HOT_KEY_MODIFIERS {
    let mut out = MOD_NOREPEAT;
    if m.ctrl {
        out |= MOD_CONTROL;
    }
    if m.alt {
        out |= MOD_ALT;
    }
    if m.shift {
        out |= MOD_SHIFT;
    }
    if m.win {
        out |= MOD_WIN;
    }
    out
}

fn hotkey_thread(handler: HotkeyHandler, requests: Receiver<Request>, ready: Sender<Result<Hwnd, crate::WinError>>) {
    let class = w!("VoxHotkeyWindow");
    let hwnd = match register_class(class, Some(hotkey_proc))
        .and_then(|_| create_hidden_window(class, "Vox Hotkeys", Some(HWND_MESSAGE)))
    {
        Ok(h) => h,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let _ = STATE.try_with(|s| *s.borrow_mut() = Some(ThreadState { hwnd, handler, requests, registered: Vec::new() }));
    let _ = ready.send(Ok(hwnd_raw(hwnd)));
    pump_messages();
    let _ = STATE.try_with(|s| {
        if let Some(mut state) = s.borrow_mut().take() {
            unregister_ours(&mut state);
        }
    });
}

unsafe extern "system" fn hotkey_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_HOTKEY => {
            if let Some(action) = action_for_id(wparam.0 as i32) {
                fire(action);
            }
            LRESULT(0)
        }
        WM_APP_REQUEST => {
            let caught = catch_unwind(AssertUnwindSafe(serve_requests));
            if caught.is_err() {
                tracing::warn!(target: "hotkeys", "hotkey request handling panicked");
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: default handling.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn fire(action: HotkeyAction) {
    let handler =
        STATE.try_with(|s| s.try_borrow().ok().and_then(|g| g.as_ref().map(|st| st.handler.clone()))).ok().flatten();
    if let Some(handler) = handler {
        if catch_unwind(AssertUnwindSafe(|| handler(action))).is_err() {
            tracing::warn!(target: "hotkeys", "hotkey {} handler failed", action.id());
        }
    }
}

fn serve_requests() {
    let _ = STATE.try_with(|s| {
        let Ok(mut guard) = s.try_borrow_mut() else { return };
        let Some(state) = guard.as_mut() else { return };
        while let Ok(request) = state.requests.try_recv() {
            match request {
                Request::Register { hotkeys, reply } => {
                    unregister_ours(state);
                    let mut refused = Vec::new();
                    for (action, parsed) in hotkeys {
                        let id = hotkey_id(action);
                        // SAFETY: our own window, owned by this thread.
                        match unsafe { RegisterHotKey(Some(state.hwnd), id, hotkey_modifiers(parsed.modifiers), parsed.vk) } {
                            Ok(()) => state.registered.push(id),
                            Err(e) => {
                                tracing::debug!(target: "hotkeys", "RegisterHotKey refused {}: {}", action.id(), e.message());
                                refused.push(action);
                            }
                        }
                    }
                    let _ = reply.send(refused);
                }
                Request::UnregisterAll { reply } => {
                    unregister_ours(state);
                    let _ = reply.send(());
                }
                Request::Stop => {
                    unregister_ours(state);
                    // SAFETY: destroys our own window on its thread; WM_DESTROY ends the loop.
                    if unsafe { DestroyWindow(state.hwnd) }.is_err() {
                        // SAFETY: ends the loop anyway.
                        unsafe { PostQuitMessage(0) };
                    }
                    return;
                }
            }
        }
    });
}

fn unregister_ours(state: &mut ThreadState) {
    for id in state.registered.drain(..) {
        // SAFETY: an id we registered on this thread's window; errors (already gone) are ignored.
        let _ = unsafe { UnregisterHotKey(Some(state.hwnd), id) };
    }
}

/// What Ctrl+Alt(+Shift)+`vk` types on any installed layout: `Some("")` dead key, `Some(ch)`, or `None`.
pub fn alt_gr_character(vk: u32, shift: bool) -> Option<String> {
    // SAFETY: querying the count only.
    let n = unsafe { GetKeyboardLayoutList(None) };
    if n <= 0 {
        return None;
    }
    let mut layouts = vec![HKL::default(); n as usize];
    // SAFETY: `layouts` has room for `n` handles.
    let got = unsafe { GetKeyboardLayoutList(Some(&mut layouts)) };
    layouts.truncate(got.max(0) as usize);
    layouts.into_iter().find_map(|hkl| probe_layout(vk, shift, hkl))
}

/// The Settings recorder's AltGr guard (ui-settings-widget.md §3.4) on the **active** layout (the calling thread's,
/// i.e. the UI thread's): what Ctrl+Alt(+Shift)+`vk` types, judged by the recorder's rules rather than the
/// registration probe's ([`alt_gr_character`], shell.md §8.4). `Some(ch)` only when the result is exactly one
/// printable code point (not whitespace, not in a `C*` category), not an ASCII letter or digit, not a Shift
/// combination on a key outside the letter block, and different (ignoring case) from what the key types without
/// modifiers (`ToUnicodeEx` with no key held: layouts without an AltGr layer report the base character); when the key
/// types nothing on its own, only a letter-block key whose result is not a letter (`\p{L}`) counts. `Some("")` for an
/// AltGr dead key (the recorder accepts the chord, as the original did); `None` otherwise. vox-app passes the result
/// to `vox_core::hotkey::record_key`, which refuses the chord for any non-empty character.
pub fn alt_gr_character_active_layout(vk: u32, shift: bool) -> Option<String> {
    // SAFETY: the calling thread's layout (the UI thread for the recorder); a plain value query.
    let (hkl, scan) = unsafe {
        let hkl = GetKeyboardLayout(0);
        (hkl, MapVirtualKeyExW(vk, MAPVK_VK_TO_VSC, Some(hkl)))
    };
    let (r, buf) = to_unicode(vk, scan, &alt_gr_key_state(shift), hkl);
    let produced = recorder_typed(r, &buf);
    let (r, buf) = to_unicode(vk, scan, &[0u8; 256], hkl);
    let base = recorder_typed(r, &buf);
    recorder_alt_gr_char(produced, base, is_letter_block_scan(scan), shift)
}

/// The key state of Ctrl+Alt (+Shift) held (selection.md §12.1).
fn alt_gr_key_state(shift: bool) -> [u8; 256] {
    let mut state = [0u8; 256];
    for k in [0x11usize, 0x12, 0xA2, 0xA5] {
        state[k] = 0x80;
    }
    if shift {
        state[0x10] = 0x80;
        state[0xA0] = 0x80;
    }
    state
}

/// `ToUnicodeEx` without touching the keyboard's dead-key state: `(result, buffer)`.
fn to_unicode(vk: u32, scan: u32, state: &[u8; 256], hkl: HKL) -> (i32, [u16; 8]) {
    let mut buf = [0u16; 8];
    // SAFETY: plain value arguments and buffers that outlive the call.
    let r = unsafe { ToUnicodeEx(vk, scan, state, &mut buf, TOUNICODE_NO_STATE_CHANGE, Some(hkl)) };
    (r, buf)
}

/// `ToUnicodeEx` with Ctrl+Alt (+Shift) held on one layout (selection.md §12.1).
fn probe_layout(vk: u32, shift: bool, hkl: HKL) -> Option<String> {
    // SAFETY: a plain value query.
    let scan = unsafe { MapVirtualKeyExW(vk, MAPVK_VK_TO_VSC, Some(hkl)) };
    let (r, buf) = to_unicode(vk, scan, &alt_gr_key_state(shift), hkl);
    typed_text(r, &buf)
}

/// One `ToUnicodeEx` result as the recorder sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Typed {
    /// Nothing typed, or text with an unpaired surrogate (category `Cs`: never printable).
    Nothing,
    /// A dead key, with the spacing character `ToUnicodeEx` reports for it.
    Dead(String),
    /// The typed text.
    Text(String),
}

fn recorder_typed(r: i32, buf: &[u16]) -> Typed {
    if r < 0 {
        let end = buf.iter().position(|&u| u == 0).unwrap_or(buf.len());
        let spacing = char::decode_utf16(buf[..end].iter().copied()).next().and_then(Result::ok);
        return Typed::Dead(spacing.map(String::from).unwrap_or_default());
    }
    let units = &buf[..(r as usize).min(buf.len())];
    match char::decode_utf16(units.iter().copied()).collect::<Result<String, _>>() {
        Ok(text) if !text.is_empty() => Typed::Text(text),
        _ => Typed::Nothing,
    }
}

/// The US-position letter keys (`KeyA`–`KeyZ`): scan codes Q–P, A–L and Z–M.
fn is_letter_block_scan(scan: u32) -> bool {
    matches!(scan, 0x10..=0x19 | 0x1E..=0x26 | 0x2C..=0x32)
}

/// ui-settings-widget.md §3.4 "printable": exactly one code point, not whitespace, not `C*` (Cc, Cf, Cs, Co, Cn).
fn is_printable_single(text: &str) -> bool {
    static PRINTABLE: std::sync::OnceLock<Option<regex::Regex>> = std::sync::OnceLock::new();
    PRINTABLE.get_or_init(|| regex::Regex::new(r"^[^\p{C}\s]$").ok()).as_ref().is_some_and(|re| re.is_match(text))
}

/// `\p{L}`.
fn is_unicode_letter(text: &str) -> bool {
    static LETTER: std::sync::OnceLock<Option<regex::Regex>> = std::sync::OnceLock::new();
    LETTER.get_or_init(|| regex::Regex::new(r"^\p{L}$").ok()).as_ref().is_some_and(|re| re.is_match(text))
}

/// The recorder's rule (ui-settings-widget.md §3.4) over the probe results (pure; table-tested).
fn recorder_alt_gr_char(produced: Typed, base: Typed, letter_key: bool, shift: bool) -> Option<String> {
    let produced = match produced {
        Typed::Nothing => return None,
        Typed::Dead(_) => return Some(String::new()),
        Typed::Text(text) => text,
    };
    if !is_printable_single(&produced) || produced.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    if shift && !letter_key {
        // Can't tell.
        return None;
    }
    let base = match base {
        Typed::Text(text) | Typed::Dead(text) if !text.is_empty() => Some(text),
        _ => None,
    };
    let counts = match base {
        Some(base) => produced.to_lowercase() != base.to_lowercase(),
        None => letter_key && !is_unicode_letter(&produced),
    };
    counts.then_some(produced)
}

/// Interprets a `ToUnicodeEx` result: negative → dead key (`Some("")`); positive → the text when any code point is
/// a typed character (≥ U+0020 and not DEL); control characters (Ctrl+letter) and nothing typed → `None`.
fn typed_text(r: i32, buf: &[u16]) -> Option<String> {
    if r < 0 {
        return Some(String::new());
    }
    if r == 0 {
        return None;
    }
    let units = &buf[..(r as usize).min(buf.len())];
    let text = String::from_utf16_lossy(units);
    text.chars().any(|c| c as u32 >= 0x20 && c as u32 != 0x7F).then_some(text)
}

/// Scan code (+ extended flag) → virtual key on the active layout (`MapVirtualKeyExW(MAPVK_VSC_TO_VK_EX)`), for the
/// recorder fed by winit key events. Ignores NumLock, so the recorder maps winit's `KeyCode::Numpad0..=Numpad9`,
/// `NumpadDecimal`, `NumpadAdd/Subtract/Multiply/Divide` to `VK_NUMPAD0..9` / `VK_DECIMAL` / `VK_ADD…` itself and
/// calls this only for the other keys (D-HK-5).
pub fn vk_from_scan_code(scan: u32, extended: bool) -> Option<u32> {
    // SAFETY: the calling thread's layout; plain values.
    let vk =
        unsafe { MapVirtualKeyExW(scan_code_argument(scan, extended), MAPVK_VSC_TO_VK_EX, Some(GetKeyboardLayout(0))) };
    (vk != 0).then_some(vk)
}

/// The `uCode` for `MAPVK_VSC_TO_VK_EX`: an extended scan code carries `0xE0` in its high byte.
fn scan_code_argument(scan: u32, extended: bool) -> u32 {
    let low = scan & 0xFF;
    if extended {
        0xE000 | low
    } else {
        low
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vox_core::hotkey::{parse_accelerator, plan_registration};
    use vox_core::settings::{HotkeySettings, TriggerSettings};

    fn triggers(read_selection: &str, read_clipboard: &str, play_pause: &str, stop: &str) -> TriggerSettings {
        TriggerSettings {
            hotkeys_enabled: true,
            hotkeys: HotkeySettings {
                read_selection: read_selection.into(),
                read_clipboard: read_clipboard.into(),
                play_pause: play_pause.into(),
                stop: stop.into(),
            },
            ..TriggerSettings::default()
        }
    }

    #[test]
    fn ids_follow_the_registration_order() {
        for (i, action) in HotkeyAction::ALL.into_iter().enumerate() {
            assert_eq!(hotkey_id(action), i as i32 + 1);
            assert_eq!(action_for_id(i as i32 + 1), Some(action));
        }
        assert_eq!(action_for_id(0), None);
        assert_eq!(action_for_id(5), None);
        assert_eq!(action_for_id(-3), None);
        // System hotkey ids (IDHOT_SNAPWINDOW / IDHOT_SNAPDESKTOP) and extremes are not ours.
        assert_eq!(action_for_id(-1), None);
        assert_eq!(action_for_id(i32::MIN), None);
        assert_eq!(action_for_id(i32::MAX), None);
    }

    #[test]
    fn modifiers_always_carry_no_repeat() {
        assert_eq!(hotkey_modifiers(Modifiers::default()), MOD_NOREPEAT);
        let all = Modifiers { ctrl: true, alt: true, shift: true, win: true };
        assert_eq!(hotkey_modifiers(all), MOD_NOREPEAT | MOD_CONTROL | MOD_ALT | MOD_SHIFT | MOD_WIN);
        // `Ctrl++` registers Ctrl+Shift+VK_OEM_PLUS (Electron's implicit Shift, R-Hotkeys).
        let p = parse_accelerator("Ctrl++").expect("parse");
        assert_eq!(p.vk, 0xBB);
        assert_eq!(hotkey_modifiers(p.modifiers), MOD_NOREPEAT | MOD_CONTROL | MOD_SHIFT);
    }

    #[test]
    fn os_refusals_merge_into_registration_order() {
        let triggers = triggers("Ctrl+Alt+R", "Ctrl+C", "Ctrl+Alt+R", "Ctrl+Alt+S");
        let plan = plan_registration(&triggers, |_, _| None);
        // The OS refuses readSelection and stop.
        let failures = merge_failures(&plan, &[HotkeyAction::Stop, HotkeyAction::ReadSelection]);
        let got: Vec<(HotkeyAction, &str, &str)> =
            failures.iter().map(|f| (f.action, f.accelerator.as_str(), f.reason.id())).collect();
        assert_eq!(
            got,
            vec![
                (HotkeyAction::ReadSelection, "Ctrl+Alt+R", "in-use"),
                (HotkeyAction::ReadClipboard, "Ctrl+C", "reserved"),
                (HotkeyAction::PlayPause, "Ctrl+Alt+R", "duplicate"),
                (HotkeyAction::Stop, "Ctrl+Alt+S", "in-use"),
            ]
        );
        assert!(merge_failures(&RegistrationPlan::default(), &[]).is_empty());
    }

    #[test]
    fn to_unicode_results_are_interpreted_like_the_original() {
        let w = |s: &str| -> Vec<u16> {
            let mut v: Vec<u16> = s.encode_utf16().collect();
            v.resize(8, 0);
            v
        };
        assert_eq!(typed_text(-1, &w("")), Some(String::new()));
        assert_eq!(typed_text(0, &w("")), None);
        assert_eq!(typed_text(1, &w("@")), Some("@".into()));
        assert_eq!(typed_text(1, &w("ś")), Some("ś".into()));
        // Ctrl+V on a layout without an AltGr layer types SYN (0x16): not a character.
        assert_eq!(typed_text(1, &w("\u{16}")), None);
        assert_eq!(typed_text(1, &w("\u{7F}")), None);
        // Two units (a surrogate pair) and a count past the buffer are handled.
        assert_eq!(typed_text(2, &w("😀")), Some("😀".into()));
        assert_eq!(typed_text(20, &w("ab")), Some("ab\0\0\0\0\0\0".into()));
    }

    fn units(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    fn text(s: &str) -> Typed {
        Typed::Text(s.into())
    }

    /// ui-settings-widget.md §3.4 vectors (review finding: the recorder used to refuse every non-empty result).
    #[test]
    fn the_recorder_applies_the_altgr_rules() {
        let rule = |produced: Typed, base: Typed, letter_key: bool, shift: bool| {
            recorder_alt_gr_char(produced, base, letter_key, shift)
        };
        // Types a character: Polish AltGr+S, Hungarian AltGr+V, German AltGr+E, AltGr+Shift+S on Polish.
        assert_eq!(rule(text("ś"), text("s"), true, false), Some("ś".into()));
        assert_eq!(rule(text("@"), text("v"), true, false), Some("@".into()));
        assert_eq!(rule(text("€"), text("e"), true, false), Some("€".into()));
        assert_eq!(rule(text("Ś"), text("s"), true, true), Some("Ś".into()));
        // A punctuation key without Shift: judged by its base character.
        assert_eq!(rule(text("~"), text("+"), false, false), Some("~".into()));
        // An ASCII letter or digit never counts.
        assert_eq!(rule(text("q"), text("a"), true, false), None);
        assert_eq!(rule(text("7"), text("&"), false, false), None);
        // Whitespace (AltGr+Space = U+00A0), more than one code point, C* categories.
        assert_eq!(rule(text("\u{A0}"), text(" "), false, false), None);
        assert_eq!(rule(text("ab"), text("x"), true, false), None);
        assert_eq!(rule(text("ś!"), text("s"), true, false), None);
        for c in ["\u{1B}", "\u{7F}", "\u{200C}", "\u{200E}", "\u{AD}", "\u{E000}", "\u{F0000}", "\u{378}"] {
            assert_eq!(rule(text(c), text("s"), true, false), None, "{:?}", c);
        }
        // Shift on a key outside the letter block: can't tell.
        assert_eq!(rule(text("¿"), text("'"), false, true), None);
        // Same as the base character ignoring case (layouts without an AltGr layer report it).
        assert_eq!(rule(text("ф"), text("ф"), true, false), None);
        assert_eq!(rule(text("É"), text("é"), false, false), None);
        // A dead base key's spacing character is its base.
        assert_eq!(rule(text("^"), Typed::Dead("^".into()), false, false), None);
        assert_eq!(rule(text("~"), Typed::Dead("^".into()), false, false), Some("~".into()));
        // No base character: only letter-block keys whose result is not a letter count.
        assert_eq!(rule(text("@"), Typed::Nothing, true, false), Some("@".into()));
        assert_eq!(rule(text("ś"), Typed::Nothing, true, false), None);
        assert_eq!(rule(text("@"), Typed::Nothing, false, false), None);
        // Dead keys and nothing typed.
        assert_eq!(rule(Typed::Dead("´".into()), text("e"), true, false), Some(String::new()));
        assert_eq!(rule(Typed::Nothing, text("e"), true, false), None);
    }

    #[test]
    fn recorder_probe_results_are_decoded_strictly() {
        let mut buf = [0u16; 8];
        buf[0] = 0x15B; // ś
        assert_eq!(recorder_typed(1, &buf), text("ś"));
        assert_eq!(recorder_typed(0, &buf), Typed::Nothing);
        // A dead key: ToUnicodeEx leaves its spacing character in the buffer.
        buf[0] = u16::from(b'^');
        assert_eq!(recorder_typed(-1, &buf), Typed::Dead("^".into()));
        assert_eq!(recorder_typed(-1, &[0u16; 8]), Typed::Dead(String::new()));
        // A lone surrogate (Cs) is never printable.
        let mut lone = [0u16; 8];
        lone[0] = 0xD83D;
        assert_eq!(recorder_typed(1, &lone), Typed::Nothing);
        // A surrogate pair is one code point.
        let pair = units("😀");
        assert_eq!(recorder_typed(2, &pair), text("😀"));
        assert!(is_printable_single("😀"));
    }

    #[test]
    fn the_letter_block_is_the_us_position_letter_keys() {
        for (scan, letter) in [(0x10, true), (0x19, true), (0x1E, true), (0x26, true), (0x2C, true), (0x32, true)] {
            assert_eq!(is_letter_block_scan(scan), letter, "{scan:#x}");
        }
        // 1, [, ;, ', `, \, ',', Space, F1.
        for scan in [0x02, 0x1A, 0x27, 0x28, 0x29, 0x2B, 0x33, 0x39, 0x3B] {
            assert!(!is_letter_block_scan(scan), "{scan:#x}");
        }
    }

    #[test]
    fn extended_scan_codes_carry_e0() {
        assert_eq!(scan_code_argument(0x1D, false), 0x1D);
        assert_eq!(scan_code_argument(0x1D, true), 0xE01D);
        assert_eq!(scan_code_argument(0xE04B, true), 0xE04B);
    }

    #[test]
    fn layout_probes_never_fail_on_keys_without_altgr() {
        // F5 never types anything with Ctrl+Alt, on any layout.
        assert_eq!(alt_gr_character(0x74, false), None);
        assert_eq!(alt_gr_character_active_layout(0x74, true), None);
        // 'A' (0x1E) maps to some VK on every layout.
        assert!(vk_from_scan_code(0x1E, false).is_some());
        // Right Ctrl is the extended Ctrl scan code.
        assert_eq!(vk_from_scan_code(0x1D, true), Some(0xA3));
    }

    /// `cargo test -p vox-win hotkeys_register_and_conflict -- --ignored --nocapture` on a desktop session: registers
    /// Ctrl+Alt+Shift+F11 twice (the second service is refused with `ERROR_HOTKEY_ALREADY_REGISTERED` → in-use), then
    /// waits 5 s for you to press it.
    #[test]
    #[ignore = "registers global hotkeys on the interactive desktop"]
    fn hotkeys_register_and_conflict() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let first = HotkeyService::spawn(Arc::new(move |a| {
            let _ = tx.send(a);
        }))
        .expect("spawn");
        let triggers = triggers("Ctrl+Alt+Shift+F11", "", "", "F12");
        let plan = plan_registration(&triggers, alt_gr_character);
        let failures = first.register(&plan);
        println!("first: {failures:?}");
        // F12 is reserved for debuggers.
        assert!(failures.iter().any(|f| f.action == HotkeyAction::Stop && f.reason == HotkeyFailureReason::InUse));
        let second = HotkeyService::spawn(Arc::new(|_| {})).expect("spawn second");
        let refused = second.register(&plan);
        assert!(refused
            .iter()
            .any(|f| f.action == HotkeyAction::ReadSelection && f.reason == HotkeyFailureReason::InUse));
        println!("press Ctrl+Alt+Shift+F11 within 5 s…");
        println!("{:?}", rx.recv_timeout(Duration::from_secs(5)));
        first.unregister_all();
        first.shutdown();
        second.shutdown();
        println!(
            "AltGr+Q/E/V on installed layouts: {:?} {:?} {:?}",
            alt_gr_character(0x51, false),
            alt_gr_character(0x45, false),
            alt_gr_character(0x56, false)
        );
    }
}
