//! The Settings hotkey recorder (ui-settings-widget.md §3.5). While a row records, the main window's native event
//! dispatcher (`windowing::main_window::on_created`, one `on_winit_window_event` per window) calls
//! [`on_winit_event`] first; it maps each key press to a VK and calls `vox_core::hotkey::record_key(vk, mods,
//! alt_gr_character_active_layout)`: partial → preview, needs-modifier / unsupported / types-character → 1.8 s hint,
//! ok → reserved check (`hotkey::is_reserved` → 6 s rejection, not saved) else save. VKs: winit
//! `PhysicalKey::Code(KeyCode::Numpad0..=Numpad9)` → `VK_NUMPAD0..9`, `NumpadDecimal` → `VK_DECIMAL`,
//! `NumpadAdd/Subtract/Multiply/Divide` → `VK_ADD/SUBTRACT/MULTIPLY/DIVIDE` directly (NumLock-independent, D-HK-5);
//! every other key through `vox_win::hotkeys::vk_from_scan_code`.
//!
//! Recording start sends `Command::SuspendHotkeys(true)` and, while the main window is focused and visible, re-arms it
//! every `HOTKEY_SUSPEND_REARM_MS` (20 s, D-USW-18). Recording stops (value unchanged, `SuspendHotkeys(false)`) on
//! blur/Tab/Escape, when the main window loses focus or is hidden/minimized (`WindowEvent::MainState{focused: false}`
//! or `{visible: false}` as seen by the UI), when the page is left, and on `UiUpdate::HotkeysResumed` (the core's 30 s
//! safety timer fired; also clears the preview). A save sends `UpdateSettings` for the new accelerator **before**
//! `SuspendHotkeys(false)` (same FIFO channel → the resume re-registers the new value).
//!
//! ## How the pieces meet (implementation notes)
//!
//! - **One recording session per UI thread** (a thread-local [`Session`]): the Settings controller drives it from the
//!   Slint callbacks (`settings-hotkey-start(action)` → [`start`], `settings-hotkey-stop` → [`stop`]; page leave and
//!   `HotkeysResumed` → [`stop`] / [`on_resumed`]) and reads it when it builds `HotkeyRowData` ([`row_view`]); the
//!   native dispatcher feeds it through [`on_winit_event`] (which mirrors the result into `AppUi.recorder`). The
//!   session keeps a clone of the `CoreHandle` given at [`start`], so a key press can save and resume on its own.
//! - Rust decides whether a start request is honoured: a request while the main window is not focused (as reported by
//!   winit `Focused`) is declined and the Slint recorder simply stays idle (`triggers.slint` follows
//!   `HotkeyRowData.recording` alone).
//! - Things the Settings controller owns are handed over as [`RecorderEvent`]s ([`take_events`]): a save (its request
//!   id and patch join the optimistic overlay, the 6 s rejection is cleared) and a rejection (reserved chord → the 6 s
//!   message under the row via `settings::HotkeyRows`).
//! - **Rebuild ticks**: the controller interface has no "rebuild this page" hook for work that happens outside a
//!   Slint callback (a native key event, a timer), so the session asks for one with `Command::QueryAppInfo`: the core
//!   answers with `UiUpdate::AppInfo`, which the active Settings page treats as dirty (draining the events first). The
//!   hint's 1.8 s expiry is ticked the same way.
//! - Modifiers come from winit `ModifiersChanged`. winit drops Ctrl and Alt while AltGr (right Alt on a layout with an
//!   AltGr layer) is held; the session tracks the right Alt key and puts Ctrl+Alt back in that case, because Windows
//!   reports AltGr as Ctrl+Alt (the AltGr guard then refuses chords that type a character).
//! - Bare Escape cancels; bare Backspace / Delete clear the shortcut (`""`); Tab / Shift+Tab without Ctrl/Alt/Win pass
//!   through so focus can leave (blur stops recording). Every other key event is consumed while recording. After a
//!   save / rejection / clear, the rest of that key (auto-repeats and its release) is consumed too, so a repeated
//!   Ctrl+1 never reaches the window's shortcuts.

use std::cell::RefCell;
use std::time::Duration;

use slint::winit_030::winit::event::{ElementState, WindowEvent};
use slint::winit_030::winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use vox_core::constants::{HOTKEY_HINT_MS, HOTKEY_SUSPEND_REARM_MS};
use vox_core::hotkey::{HotkeyAction, Modifiers, RecordOutcome};

use crate::command::{Command, CoreHandle, RequestId};

/// Hint for a key without Ctrl/Alt/Win that cannot stand alone.
pub const HINT_NEEDS_MODIFIER: &str = "Add Ctrl, Alt or Win";
/// Hint for a key the accelerator grammar has no name for.
pub const HINT_UNSUPPORTED: &str = "That key can\u{2019}t be used";

/// Hint for a Ctrl+Alt chord that types a character on the active layout (AltGr guard).
pub fn hint_types_character(ch: &str) -> String {
    format!("On this keyboard that types \u{201C}{ch}\u{201D}")
}

/// Recorder state (a mirror of the thread's recording session, refreshed by every [`on_winit_event`] call).
#[derive(Debug, Default)]
pub struct Recorder {
    pub recording: Option<HotkeyAction>,
    pub hint: Option<String>,
    pub preview: Vec<String>,
}

/// What the recorder hands to the Settings controller (drained with [`take_events`]).
#[derive(Debug, Clone, PartialEq)]
pub enum RecorderEvent {
    /// `UpdateSettings{req, patch}` was sent for `action` (before `SuspendHotkeys(false)`).
    Saved { action: HotkeyAction, req: RequestId, patch: serde_json::Value },
    /// A reserved chord was recorded for `action`: not saved, the old shortcut stays (6 s message).
    Rejected { action: HotkeyAction, accel: String },
}

/// A key press / release as the session sees it (pure input, built from winit events by [`on_winit_event`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyInput {
    pub code: Option<KeyCode>,
    /// The virtual key ([`vk_for`]); `None` = no VK for this key.
    pub vk: Option<u32>,
    pub pressed: bool,
    pub repeat: bool,
    /// winit's synthetic presses/releases on focus changes.
    pub synthetic: bool,
}

/// The thread's recording session (module docs).
#[derive(Default)]
pub struct Session {
    core: Option<CoreHandle>,
    recording: Option<HotkeyAction>,
    preview: Vec<String>,
    /// The flashed hint: (row, text, until epoch ms).
    hint: Option<(HotkeyAction, String, f64)>,
    /// winit's modifier state.
    mods: Modifiers,
    /// The right Alt key is down (AltGr on layouts that have it).
    alt_right: bool,
    /// The main window has keyboard focus (winit `Focused`); unknown counts as focused.
    unfocused: bool,
    /// The key whose save/rejection/clear ended the recording: its repeats and release are consumed.
    swallow: Option<KeyCode>,
    events: Vec<RecorderEvent>,
    rearm: slint::Timer,
    hint_timer: slint::Timer,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("recording", &self.recording)
            .field("preview", &self.preview)
            .field("hint", &self.hint)
            .field("mods", &self.mods)
            .field("alt_right", &self.alt_right)
            .field("unfocused", &self.unfocused)
            .field("events", &self.events)
            .finish()
    }
}

thread_local! {
    static SESSION: RefCell<Session> = RefCell::new(Session::default());
}

/// Runs `f` on the thread's session (`None` if it is busy, which only a re-entrant call could cause).
fn with_session<R>(f: impl FnOnce(&mut Session) -> R) -> Option<R> {
    SESSION.with(|s| s.try_borrow_mut().ok().map(|mut s| f(&mut s)))
}

/// Asks the core for an `AppInfo` answer so the Settings page is rebuilt (module docs "Rebuild ticks").
pub(crate) fn request_rebuild(core: &CoreHandle) {
    core.send(Command::QueryAppInfo);
}

/// Recorder modifier names for the preview keycaps (`Super` shows as `Win`).
fn preview_names(mods: Modifiers) -> Vec<String> {
    mods.names().into_iter().map(|n| if n == "Super" { "Win".to_string() } else { n.to_string() }).collect()
}

fn mods_from(state: ModifiersState) -> Modifiers {
    Modifiers { ctrl: state.control_key(), alt: state.alt_key(), shift: state.shift_key(), win: state.super_key() }
}

/// The VK the recorder uses for a physical key (D-HK-4/5): numpad keys directly (NumLock-independent), everything
/// else through the active layout's scan-code mapping.
pub fn vk_for(key: PhysicalKey) -> Option<u32> {
    if let PhysicalKey::Code(code) = key {
        if let Some(vk) = numpad_vk(code) {
            return Some(vk);
        }
    }
    use slint::winit_030::winit::platform::scancode::PhysicalKeyExtScancode;
    let sc = key.to_scancode()?;
    vox_win::hotkeys::vk_from_scan_code(sc & 0xFF, sc & 0xFF00 == 0xE000)
}

/// `VK_NUMPAD0..9`, `VK_DECIMAL`, `VK_ADD`, `VK_SUBTRACT`, `VK_MULTIPLY`, `VK_DIVIDE` for the numpad keys (D-HK-5).
pub fn numpad_vk(code: KeyCode) -> Option<u32> {
    Some(match code {
        KeyCode::Numpad0 => 0x60,
        KeyCode::Numpad1 => 0x61,
        KeyCode::Numpad2 => 0x62,
        KeyCode::Numpad3 => 0x63,
        KeyCode::Numpad4 => 0x64,
        KeyCode::Numpad5 => 0x65,
        KeyCode::Numpad6 => 0x66,
        KeyCode::Numpad7 => 0x67,
        KeyCode::Numpad8 => 0x68,
        KeyCode::Numpad9 => 0x69,
        KeyCode::NumpadMultiply => 0x6A,
        KeyCode::NumpadAdd => 0x6B,
        KeyCode::NumpadSubtract => 0x6D,
        KeyCode::NumpadDecimal => 0x6E,
        KeyCode::NumpadDivide => 0x6F,
        _ => return None,
    })
}

fn is_letter_key(code: Option<KeyCode>) -> bool {
    matches!(
        code,
        Some(
            KeyCode::KeyA
                | KeyCode::KeyB
                | KeyCode::KeyC
                | KeyCode::KeyD
                | KeyCode::KeyE
                | KeyCode::KeyF
                | KeyCode::KeyG
                | KeyCode::KeyH
                | KeyCode::KeyI
                | KeyCode::KeyJ
                | KeyCode::KeyK
                | KeyCode::KeyL
                | KeyCode::KeyM
                | KeyCode::KeyN
                | KeyCode::KeyO
                | KeyCode::KeyP
                | KeyCode::KeyQ
                | KeyCode::KeyR
                | KeyCode::KeyS
                | KeyCode::KeyT
                | KeyCode::KeyU
                | KeyCode::KeyV
                | KeyCode::KeyW
                | KeyCode::KeyX
                | KeyCode::KeyY
                | KeyCode::KeyZ
        )
    )
}

/// The AltGr guard's character (ui-settings-widget.md §3.4): only one printable code point counts, never an ASCII
/// letter or digit, and with Shift only on letter keys (otherwise it cannot be told apart).
fn printable_alt_gr(produced: Option<String>, shift: bool, code: Option<KeyCode>) -> Option<String> {
    let s = produced?;
    let mut chars = s.chars();
    let c = chars.next()?;
    if chars.next().is_some() || c.is_whitespace() || c.is_control() || c.is_ascii_alphanumeric() {
        return None;
    }
    if shift && !is_letter_key(code) {
        return None;
    }
    Some(s)
}

impl Session {
    /// The modifiers of a key press: winit's state, with Ctrl+Alt restored while AltGr is held (module docs).
    fn effective_mods(&self) -> Modifiers {
        let mut m = self.mods;
        if self.alt_right && !m.ctrl && !m.alt {
            m.ctrl = true;
            m.alt = true;
        }
        m
    }

    /// Records `action` (a Slint start request). False when declined (the main window is not focused).
    pub fn start(&mut self, core: &CoreHandle, action: HotkeyAction) -> bool {
        if self.unfocused {
            return false;
        }
        self.core = Some(core.clone());
        let was_recording = self.recording.is_some();
        self.recording = Some(action);
        self.preview.clear();
        self.hint = None;
        self.hint_timer.stop();
        self.swallow = None;
        if !was_recording {
            core.send(Command::SuspendHotkeys(true));
            let core = core.clone();
            self.rearm.start(slint::TimerMode::Repeated, Duration::from_millis(HOTKEY_SUSPEND_REARM_MS), move || {
                super::shell::guarded("hotkey suspension re-arm", || {
                    // Only while still recording with the window focused (D-USW-18).
                    let live = SESSION.with(|s| s.try_borrow().map(|s| s.recording.is_some() && !s.unfocused).unwrap_or(false));
                    if live {
                        core.send(Command::SuspendHotkeys(true));
                    }
                });
            });
        }
        true
    }

    /// Stops recording (value unchanged) and resumes the global hotkeys. True when it was recording.
    pub fn stop(&mut self, core: &CoreHandle) -> bool {
        if self.recording.take().is_none() {
            return false;
        }
        self.preview.clear();
        self.rearm.stop();
        core.send(Command::SuspendHotkeys(false));
        true
    }

    /// The core's 30 s safety timer already resumed the hotkeys: stop recording, clear the preview.
    pub fn on_resumed(&mut self) -> bool {
        self.rearm.stop();
        let was = self.recording.take().is_some();
        self.preview.clear();
        was
    }

    /// A hint for `action` until `now_ms + 1.8 s` (ticked at expiry).
    fn flash(&mut self, action: HotkeyAction, text: String, now_ms: f64) {
        self.hint = Some((action, text, now_ms + HOTKEY_HINT_MS as f64));
        if let Some(core) = self.core.clone() {
            self.hint_timer.start(slint::TimerMode::SingleShot, Duration::from_millis(HOTKEY_HINT_MS + 20), move || {
                super::shell::guarded("hotkey hint expiry", || request_rebuild(&core));
            });
        }
    }

    /// Saves `accel` for `action` (`UpdateSettings` first), then stops (`SuspendHotkeys(false)`).
    fn save(&mut self, core: &CoreHandle, action: HotkeyAction, accel: &str) {
        let req = core.request_id();
        let patch = vox_core::settings::patch_at(&action.settings_path(), serde_json::Value::String(accel.to_string()));
        core.send(Command::UpdateSettings { req: Some(req), patch: patch.clone() });
        self.events.push(RecorderEvent::Saved { action, req, patch });
        self.stop(core);
    }

    /// winit `ModifiersChanged`.
    pub fn on_modifiers(&mut self, mods: Modifiers) -> bool {
        self.mods = mods;
        if self.recording.is_some() {
            let preview = preview_names(self.effective_mods());
            if preview != self.preview {
                self.preview = preview;
                return true;
            }
        }
        false
    }

    /// winit `Focused`: losing focus stops recording. Returns whether the visible state changed.
    pub fn on_focus(&mut self, focused: bool) -> bool {
        self.unfocused = !focused;
        self.alt_right = false;
        self.swallow = None;
        if focused {
            false
        } else {
            self.on_hidden()
        }
    }

    /// The window was hidden or minimized (`Occluded(true)`): stop recording (focus tracking is left to `Focused`).
    pub fn on_hidden(&mut self) -> bool {
        match self.core.clone() {
            Some(core) => self.stop(&core),
            None => false,
        }
    }

    /// A key event. Returns `(consumed, changed)`: consumed = the window must not see it; changed = the page must be
    /// rebuilt. `alt_gr` = what Ctrl+Alt(+Shift)+vk types on the active layout.
    pub fn on_key(&mut self, key: KeyInput, now_ms: f64, alt_gr: &dyn Fn(u32, bool) -> Option<String>) -> (bool, bool) {
        if key.code == Some(KeyCode::AltRight) {
            self.alt_right = key.pressed;
        }
        if let (Some(code), Some(swallowed)) = (key.code, self.swallow) {
            if code == swallowed {
                if !key.pressed {
                    self.swallow = None;
                }
                return (true, false);
            }
        }
        let (Some(action), Some(core)) = (self.recording, self.core.clone()) else { return (false, false) };
        if key.synthetic {
            return (true, false);
        }
        let mods = self.effective_mods();
        if !key.pressed {
            // Releasing a modifier updates the preview (without the layout map).
            let preview = preview_names(mods);
            let changed = preview != self.preview;
            self.preview = preview;
            return (true, changed);
        }
        let strong = mods.ctrl || mods.alt || mods.win;
        let bare = !strong && !mods.shift;
        match key.code {
            Some(KeyCode::Tab) if !strong => return (false, false),
            // A held key repeats: the first press already did what it does (no hint flood, no tick storm).
            _ if key.repeat => return (true, false),
            Some(KeyCode::Escape) if bare => {
                self.stop(&core);
                return (true, true);
            }
            Some(code @ (KeyCode::Backspace | KeyCode::Delete)) if bare => {
                self.swallow = Some(code);
                self.save(&core, action, "");
                return (true, true);
            }
            _ => {}
        }
        let outcome = match key.vk {
            None => RecordOutcome::Unsupported,
            Some(vk) => {
                let ch = if mods.ctrl && mods.alt && !mods.win { printable_alt_gr(alt_gr(vk, mods.shift), mods.shift, key.code) } else { None };
                vox_core::hotkey::record_key(vk, mods, ch)
            }
        };
        match outcome {
            RecordOutcome::Partial(_) => {
                let preview = preview_names(mods);
                let changed = preview != self.preview;
                self.preview = preview;
                (true, changed)
            }
            RecordOutcome::NeedsModifier(_) => {
                self.flash(action, HINT_NEEDS_MODIFIER.to_string(), now_ms);
                (true, true)
            }
            RecordOutcome::Unsupported => {
                self.flash(action, HINT_UNSUPPORTED.to_string(), now_ms);
                (true, true)
            }
            RecordOutcome::TypesCharacter { ch, .. } => {
                self.flash(action, hint_types_character(&ch), now_ms);
                (true, true)
            }
            RecordOutcome::Ok(accel) => {
                self.swallow = key.code;
                if vox_core::hotkey::is_reserved(&accel) {
                    self.events.push(RecorderEvent::Rejected { action, accel });
                    self.stop(&core);
                } else {
                    self.save(&core, action, &accel);
                }
                (true, true)
            }
        }
    }

    /// What one row shows: (recording, preview, hint).
    pub fn row_view(&self, action: HotkeyAction, now_ms: f64) -> (bool, Vec<String>, String) {
        let recording = self.recording == Some(action);
        let preview = if recording { self.preview.clone() } else { Vec::new() };
        let hint = match &self.hint {
            Some((a, text, until)) if *a == action && now_ms < *until => text.clone(),
            _ => String::new(),
        };
        (recording, preview, hint)
    }

    fn mirror(&self, recorder: &mut Recorder, now_ms: f64) {
        recorder.recording = self.recording;
        recorder.preview = self.preview.clone();
        recorder.hint = self.hint.as_ref().filter(|(_, _, until)| now_ms < *until).map(|(_, t, _)| t.clone());
    }
}

// ───────────────────────────── thread-session API (used by `ui::settings`) ─────────────────────────────

/// Starts recording `action` (Slint `hotkey-start`). False when declined.
pub(crate) fn start(core: &CoreHandle, action: HotkeyAction) -> bool {
    with_session(|s| s.start(core, action)).unwrap_or(false)
}

/// Stops recording (Slint `hotkey-stop`, page leave). True when it was recording.
pub(crate) fn stop(core: &CoreHandle) -> bool {
    with_session(|s| s.stop(core)).unwrap_or(false)
}

/// `UiUpdate::HotkeysResumed`. True when it was recording.
pub(crate) fn on_resumed() -> bool {
    with_session(|s| s.on_resumed()).unwrap_or(false)
}

/// The events the Settings controller must apply.
pub(crate) fn take_events() -> Vec<RecorderEvent> {
    with_session(|s| std::mem::take(&mut s.events)).unwrap_or_default()
}

/// Test helper: queue an event as if a key press had produced it.
#[cfg(test)]
pub(crate) fn push_event_for_tests(ev: RecorderEvent) {
    with_session(|s| s.events.push(ev));
}

/// What one hotkey row shows: (recording, preview, hint).
pub(crate) fn row_view(action: HotkeyAction, now_ms: f64) -> (bool, Vec<String>, String) {
    with_session(|s| s.row_view(action, now_ms)).unwrap_or((false, Vec::new(), String::new()))
}

/// The main window's key events while recording: returns true when the event was consumed (the dispatcher then
/// answers `EventResult::PreventDefault`).
pub fn on_winit_event(recorder: &mut Recorder, event: &slint::winit_030::winit::event::WindowEvent) -> bool {
    let now = crate::windowing::now_ms(); // the wall clock of `UiCtx::now_ms` (Settings compares hint expiries with it)
    let outcome = with_session(|s| {
        let (consumed, changed) = match event {
            WindowEvent::ModifiersChanged(m) => (false, s.on_modifiers(mods_from(m.state()))),
            WindowEvent::Focused(focused) => (false, s.on_focus(*focused)),
            WindowEvent::Occluded(true) => (false, s.on_hidden()),
            WindowEvent::KeyboardInput { event: ke, is_synthetic, .. } => {
                let code = match ke.physical_key {
                    PhysicalKey::Code(c) => Some(c),
                    PhysicalKey::Unidentified(_) => None,
                };
                // Only look the VK up when it can matter (a press while recording).
                let vk = if s.recording.is_some() && ke.state == ElementState::Pressed { vk_for(ke.physical_key) } else { None };
                let input = KeyInput { code, vk, pressed: ke.state == ElementState::Pressed, repeat: ke.repeat, synthetic: *is_synthetic };
                s.on_key(input, now, &vox_win::hotkeys::alt_gr_character_active_layout)
            }
            _ => (false, false),
        };
        s.mirror(recorder, now);
        if changed {
            if let Some(core) = &s.core {
                request_rebuild(core);
            }
        }
        consumed
    });
    outcome.unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::{unbounded, Receiver};

    fn core() -> (CoreHandle, Receiver<Command>) {
        let (tx, rx) = unbounded();
        (CoreHandle::new(tx), rx)
    }

    /// The commands sent, as short labels.
    fn sent(rx: &Receiver<Command>) -> Vec<String> {
        rx.try_iter()
            .map(|c| match c {
                Command::SuspendHotkeys(b) => format!("suspend {b}"),
                Command::UpdateSettings { patch, .. } => format!("save {patch}"),
                Command::QueryAppInfo => "tick".into(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    fn press(code: KeyCode, vk: u32) -> KeyInput {
        KeyInput { code: Some(code), vk: Some(vk), pressed: true, repeat: false, synthetic: false }
    }
    fn release(code: KeyCode, vk: u32) -> KeyInput {
        KeyInput { pressed: false, ..press(code, vk) }
    }
    fn no_alt_gr(_: u32, _: bool) -> Option<String> {
        None
    }
    fn mods(ctrl: bool, alt: bool, shift: bool, win: bool) -> Modifiers {
        Modifiers { ctrl, alt, shift, win }
    }

    #[test]
    fn start_suspends_and_stop_resumes() {
        let (c, rx) = core();
        let mut s = Session::default();
        assert!(s.start(&c, HotkeyAction::ReadSelection));
        assert_eq!(s.recording, Some(HotkeyAction::ReadSelection));
        // Switching rows while recording does not suspend twice.
        assert!(s.start(&c, HotkeyAction::Stop));
        assert_eq!(sent(&rx), vec!["suspend true"]);
        assert!(s.stop(&c));
        assert!(!s.stop(&c), "a second stop does nothing");
        assert_eq!(sent(&rx), vec!["suspend false"]);
    }

    #[test]
    fn a_start_while_unfocused_is_declined() {
        let (c, rx) = core();
        let mut s = Session::default();
        s.on_focus(false);
        assert!(!s.start(&c, HotkeyAction::ReadSelection));
        assert!(sent(&rx).is_empty());
        s.on_focus(true);
        assert!(s.start(&c, HotkeyAction::ReadSelection));
    }

    #[test]
    fn modifiers_preview_with_win_label() {
        let (c, _rx) = core();
        let mut s = Session::default();
        s.start(&c, HotkeyAction::PlayPause);
        assert!(s.on_modifiers(mods(true, false, true, true)));
        assert_eq!(s.row_view(HotkeyAction::PlayPause, 0.0).1, vec!["Ctrl", "Shift", "Win"]);
        assert!(s.row_view(HotkeyAction::Stop, 0.0).1.is_empty(), "other rows show nothing");
        // A modifier key press is a partial: consumed, preview unchanged.
        assert_eq!(s.on_key(press(KeyCode::ControlLeft, 0xA2), 0.0, &no_alt_gr), (true, false));
    }

    #[test]
    fn hints_flash_for_1_8_s_and_survive_the_stop() {
        let (c, _rx) = core();
        let mut s = Session::default();
        s.start(&c, HotkeyAction::ReadSelection);
        s.on_modifiers(mods(false, false, true, false));
        assert_eq!(s.on_key(press(KeyCode::KeyR, 0x52), 1000.0, &no_alt_gr), (true, true));
        assert_eq!(s.row_view(HotkeyAction::ReadSelection, 1000.0).2, HINT_NEEDS_MODIFIER);
        assert!(s.recording.is_some(), "a hint keeps recording");
        // Unsupported VK.
        s.on_modifiers(mods(true, false, false, false));
        s.on_key(KeyInput { vk: None, ..press(KeyCode::Fn, 0) }, 1100.0, &no_alt_gr);
        assert_eq!(s.row_view(HotkeyAction::ReadSelection, 1100.0).2, HINT_UNSUPPORTED);
        s.stop(&c);
        assert_eq!(s.row_view(HotkeyAction::ReadSelection, 2800.0).2, HINT_UNSUPPORTED, "kept until its timer ends");
        assert_eq!(s.row_view(HotkeyAction::ReadSelection, 2900.0).2, "");
    }

    #[test]
    fn ok_saves_before_resuming() {
        let (c, rx) = core();
        let mut s = Session::default();
        s.start(&c, HotkeyAction::ReadClipboard);
        sent(&rx);
        s.on_modifiers(mods(true, true, false, false));
        assert_eq!(s.on_key(press(KeyCode::KeyB, 0x42), 0.0, &no_alt_gr), (true, true));
        assert_eq!(sent(&rx), vec![r#"save {"triggers":{"hotkeys":{"readClipboard":"Ctrl+Alt+B"}}}"#, "suspend false"]);
        assert!(s.recording.is_none());
        let ev = std::mem::take(&mut s.events);
        assert!(matches!(&ev[..], [RecorderEvent::Saved { action: HotkeyAction::ReadClipboard, .. }]));
        // The rest of that key is consumed: its auto-repeat and its release.
        assert_eq!(s.on_key(KeyInput { repeat: true, ..press(KeyCode::KeyB, 0x42) }, 0.0, &no_alt_gr), (true, false));
        assert_eq!(s.on_key(release(KeyCode::KeyB, 0x42), 0.0, &no_alt_gr), (true, false));
        assert_eq!(s.on_key(press(KeyCode::KeyB, 0x42), 0.0, &no_alt_gr), (false, false), "idle again");
    }

    #[test]
    fn a_reserved_chord_is_rejected_not_saved() {
        let (c, rx) = core();
        let mut s = Session::default();
        s.start(&c, HotkeyAction::Stop);
        sent(&rx);
        s.on_modifiers(mods(true, false, false, false));
        s.on_key(press(KeyCode::KeyC, 0x43), 0.0, &no_alt_gr);
        assert_eq!(sent(&rx), vec!["suspend false"]);
        assert_eq!(s.events, vec![RecorderEvent::Rejected { action: HotkeyAction::Stop, accel: "Ctrl+C".into() }]);
    }

    #[test]
    fn numpad_keys_map_to_numpad_vks() {
        assert_eq!(numpad_vk(KeyCode::Numpad7), Some(0x67));
        assert_eq!(numpad_vk(KeyCode::NumpadDecimal), Some(0x6E));
        assert_eq!(numpad_vk(KeyCode::NumpadAdd), Some(0x6B));
        assert_eq!(numpad_vk(KeyCode::Digit7), None);
        let (c, rx) = core();
        let mut s = Session::default();
        s.start(&c, HotkeyAction::PlayPause);
        sent(&rx);
        s.on_modifiers(mods(true, true, false, false));
        s.on_key(press(KeyCode::Numpad7, 0x67), 0.0, &no_alt_gr);
        assert_eq!(sent(&rx)[0], r#"save {"triggers":{"hotkeys":{"playPause":"Ctrl+Alt+num7"}}}"#);
    }

    #[test]
    fn escape_cancels_backspace_clears_tab_passes() {
        let (c, rx) = core();
        let mut s = Session::default();
        s.start(&c, HotkeyAction::ReadSelection);
        sent(&rx);
        // Tab (and Shift+Tab) go to the window so focus can leave.
        assert_eq!(s.on_key(press(KeyCode::Tab, 0x09), 0.0, &no_alt_gr), (false, false));
        s.on_modifiers(mods(false, false, true, false));
        assert_eq!(s.on_key(press(KeyCode::Tab, 0x09), 0.0, &no_alt_gr), (false, false));
        // Shift+Escape is an ordinary key (needs a modifier).
        assert_eq!(s.on_key(press(KeyCode::Escape, 0x1B), 0.0, &no_alt_gr), (true, true));
        assert!(s.recording.is_some());
        s.on_modifiers(Modifiers::default());
        assert_eq!(s.on_key(press(KeyCode::Escape, 0x1B), 0.0, &no_alt_gr), (true, true));
        assert!(s.recording.is_none());
        assert_eq!(sent(&rx), vec!["suspend false"]);
        // Bare Backspace clears.
        s.start(&c, HotkeyAction::ReadSelection);
        sent(&rx);
        s.on_key(press(KeyCode::Backspace, 0x08), 0.0, &no_alt_gr);
        assert_eq!(sent(&rx), vec![r#"save {"triggers":{"hotkeys":{"readSelection":""}}}"#, "suspend false"]);
    }

    #[test]
    fn losing_focus_stops_recording() {
        let (c, rx) = core();
        let mut s = Session::default();
        s.start(&c, HotkeyAction::ReadSelection);
        sent(&rx);
        assert!(s.on_focus(false));
        assert_eq!(sent(&rx), vec!["suspend false"]);
        // Synthetic and idle key events are left alone.
        assert_eq!(s.on_key(press(KeyCode::KeyR, 0x52), 0.0, &no_alt_gr), (false, false));
    }

    #[test]
    fn resumed_by_the_core_stops_without_resuming_again() {
        let (c, rx) = core();
        let mut s = Session::default();
        s.start(&c, HotkeyAction::ReadSelection);
        s.on_modifiers(mods(true, false, false, false));
        sent(&rx);
        assert!(s.on_resumed());
        assert!(s.row_view(HotkeyAction::ReadSelection, 0.0).1.is_empty());
        assert!(sent(&rx).is_empty());
    }

    #[test]
    fn alt_gr_restores_ctrl_alt_and_refuses_typing_chords() {
        let (c, rx) = core();
        let mut s = Session::default();
        s.start(&c, HotkeyAction::ReadSelection);
        sent(&rx);
        // AltGr held: winit reports no Ctrl/Alt.
        s.on_key(press(KeyCode::AltRight, 0xA5), 0.0, &no_alt_gr);
        assert_eq!(s.row_view(HotkeyAction::ReadSelection, 0.0).1, vec!["Ctrl", "Alt"]);
        let at = |_: u32, _: bool| Some("@".to_string());
        s.on_key(press(KeyCode::KeyQ, 0x51), 0.0, &at);
        assert_eq!(s.row_view(HotkeyAction::ReadSelection, 0.0).2, "On this keyboard that types \u{201C}@\u{201D}");
        assert!(sent(&rx).is_empty(), "nothing saved");
        // An ASCII letter from the layout map is not a typed character.
        let letter = |_: u32, _: bool| Some("q".to_string());
        s.on_key(press(KeyCode::KeyQ, 0x51), 0.0, &letter);
        assert_eq!(sent(&rx)[0], r#"save {"triggers":{"hotkeys":{"readSelection":"Ctrl+Alt+Q"}}}"#);
    }

    #[test]
    fn printable_rules() {
        assert_eq!(printable_alt_gr(Some("@".into()), false, Some(KeyCode::KeyV)), Some("@".into()));
        assert_eq!(printable_alt_gr(Some("ś".into()), true, Some(KeyCode::KeyS)), Some("ś".into()));
        assert_eq!(printable_alt_gr(Some("{".into()), true, Some(KeyCode::Digit7)), None, "shift on a non-letter");
        assert_eq!(printable_alt_gr(Some("7".into()), false, Some(KeyCode::Digit7)), None);
        assert_eq!(printable_alt_gr(Some(String::new()), false, Some(KeyCode::KeyV)), None, "dead key");
        assert_eq!(printable_alt_gr(Some("ab".into()), false, Some(KeyCode::KeyV)), None);
        assert_eq!(printable_alt_gr(None, false, None), None);
    }
}
