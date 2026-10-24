//! The selection service (selection.md §2, §6, §8): runs the hook (only while a chip trigger is on), feeds the
//! [`crate::gestures::GestureMachine`], applies decisions (chip show/hide/activate) and runs the clipboard round-trip
//! captures.
//!
//! **Threads** (ARCHITECTURE.md §3.1):
//! - *selection consumer* (created by [`SelectionService::new`], app lifetime): receives `RawInput` from the hook,
//!   builds `GestureEvent`s (modifier flags below, libuiohook click counter with `GetDoubleClickTime`, wheel reset),
//!   runs the gesture machine, its single deadline timer, chip decisions, the 300 ms activation dedupe and the
//!   hook-side chip hit test. It never touches the clipboard and never blocks: a capture request is **posted** to the
//!   clipboard worker (`Capture { source, via_context_menu }`), as is `Flush { reply }`.
//! - *clipboard worker* (created by `new`, app lifetime, one message-only window for `OpenClipboard`): runs strictly
//!   serially — captures, `flushRestore` (§8.3 step 1), the scheduled restores (150 ms / 0 ms timers), the late-copy
//!   watch and `finishLateCopy`, the echo guard, and `flush(timeout)`. Being one serial thread gives the §8.8 "at most
//!   one pending job + restoring chain" for free and keeps WM_RENDERFORMAT waits (Word/Excel delayed rendering) and
//!   192 MiB restores off the consumer, so chips, auto-hide and hook-side queries stay prompt. The busy flag
//!   ([`SelectionService::is_busy`]) is an `AtomicBool` set by the worker.
//! - the *hook thread* ([`crate::hook::InputHook`]).
//!
//! **Modifier flags** on `GestureEvent` (selection.md §4.1, §10.3, §14): for mouse presses the consumer takes
//! shift/ctrl/alt/meta from a `GetAsyncKeyState(0xA0–0xA5, 0x5B, 0x5C) & 0x8000` snapshot when it processes the
//! event — authoritative, immune to lost key-ups (released while an elevated window had focus, on the secure desktop,
//! during UAC). For key events it uses a mask tracked from `RawInput::Key`, updated before the machine sees the key;
//! only the side-specific VKs count (the generic 0x10–0x12 never set flags). The mask is zeroed after an extended
//! Delete keydown with ctrl && alt set (Ctrl+Alt+Del), and re-seeded from `GetAsyncKeyState` whenever
//! `GetForegroundWindow()` differs from the value seen on the previous event, after every capture, and when the hook
//! starts. No `SetWinEventHook`. A stuck Shift is therefore cleared by the next press.
//!
//! **Capture order** (selection.md §8.3 steps 1–5, §8.11; ARCHITECTURE.md §4.1): 1. `flushRestore` (every capture,
//! refusals included). 2. `foreground_window_info`. 3. Password manager → `Notice` (refused, nothing sent).
//! 4. `method = copy_method_for(..)`; `ReadClipboard` (copy-on-select terminals: PuTTY, KiTTY, MobaXterm) takes the
//! copy-on-select path with **no** UIPI check (it injects no input). 5. Only for chord methods:
//! `foreground::input_blocked_by_uipi(hwnd, pid) == Some(true)` → `Notice{Info, copy::elevated_app_refused()}`, no
//! `NoSelection`, no clipboard access (D-SEL-2). 6. Modifier wait (≤ 700 ms) and the rest: settle 40 ms (120 ms for
//! context-chip captures); snapshot; sequence number; chord; poll every 15 ms ≤ 450 ms (+ ≤ 1.5 s once the sequence
//! moved and a text format is listed); context chip with Ctrl+C and an unmoved sequence: one Escape + retry; privacy
//! markers → refused (a secret is restored regardless of `restoreClipboard`; the clipboard is cleared when the
//! snapshot failed); restore the snapshot ~150 ms later when `restore_clipboard` (read live); late-copy watch 1.5 s.
//! Sequence numbers: native Win32 always has them, so the spec's "no sequence number" fallback (§8.3 steps 12–13,
//! §8.6 `staleText`, the cleared/stale branches of §8.7) is **not ported**; a sequence number of 0 (no window-station
//! access) is logged once as `clipboard sequence unavailable` and the capture reports `NoSelection` (D-SEL-5).
//!
//! **Testability** (ARCHITECTURE.md §9, P-WIN-SEL definition of done): the capture/restore pipeline runs over a
//! private `trait CaptureEnv { seq(), text(), has_text_format(), privacy(), snapshot(), restore(), clear(),
//! send_chord(), send_escape(), held_modifiers(), now(), sleep() }` with a fake for unit tests; the real
//! implementation forwards to [`crate::clipboard`], [`crate::keys`] and the clock. The consumer's event handling runs
//! over a private `ConsumerEnv` (window lookups, key state, foreground, double-click time) the same way.
//!
//! Chip activation is de-duplicated within 300 ms between the chip UI ([`SelectionService::chip_activated`]) and the
//! hook's own press detection inside the chip bounds (which also checks that the window under the cursor is our own
//! no-activate window); the global dedupe falls back to the last chip mode.
//!
//! Product-ruling changes (ARCHITECTURE.md §8 D-SEL-1..5):
//! - **Hook failure is retried**: a failed `SetWindowsHookExW` is reported as `HookStatus(false)` (and logged once);
//!   every [`SelectionService::apply_settings`] whose `right_click_chip`/`selection_popup` differs from the last
//!   applied values (or that turns one on) tries again; success → `HookStatus(true)`. The consumer also re-installs
//!   the hook when it saw no hook event for `HOOK_SILENCE_REINSTALL_MS` (10 s) while `GetLastInputInfo` reports newer
//!   input (Windows silently removes LL hooks that exceed `LowLevelHooksTimeout`), logging `global hook was removed by
//!   Windows; reinstalling`.
//! - **UIPI** refusal as above (D-SEL-2).
//! - Clipboard snapshot/restore per `crate::clipboard` (adds `CF_HDROP`, `CF_DIB`, "do not record" markers).
//! - Chip coordinates are physical pixels (Per-Monitor-V2); vox-app converts to the target monitor's DIP.
//!
//! Every public method only sets flags / enqueues and returns at once; [`SelectionService::flush`] is the only call
//! that waits.

#![warn(dead_code, unused)]

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{select, Receiver, RecvTimeoutError, Sender};
use parking_lot::Mutex;
use vox_core::app::ChipMode;
use vox_core::constants::{
    CAPTURE_SETTLE_MS, CHIP_ACTIVATION_DEDUPE_MS, CLIPBOARD_POLL_MS, CLIPBOARD_POLL_TIMEOUT_MS,
    CLIPBOARD_RESTORE_DELAY_MS, CLIPBOARD_TEXT_SETTLE_MS, CONTEXT_MENU_SETTLE_MS, ESCAPE_SETTLE_MS,
    HOOK_SILENCE_REINSTALL_MS, LATE_COPY_GRACE_MS, LATE_POLL_MS, MODIFIER_POLL_MS, MODIFIER_WAIT_MS,
};
use vox_core::copy;
use vox_core::history::ReadSource;
use vox_core::js::js_trim;
use vox_core::read::ForegroundInfo;
use vox_core::settings::Settings;
use windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

use crate::clipboard::{ClipboardSnapshot, Privacy};
use crate::foreground::{is_password_manager, WindowInfo};
use crate::gestures::{
    Decision, GestureConfig, GestureDeps, GestureEvent, GestureEventKind, GestureMachine, GestureTarget, MouseButton,
};
use crate::hook::{InputHook, RawInput};
use crate::keys::{CopyMethod, MODIFIER_VKS};
use crate::window::PhysRect;

/// The settings the service consumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionConfig {
    pub right_click_chip: bool,
    pub selection_popup: bool,
    /// Normalised lower-case exe names.
    pub excluded_apps: Vec<String>,
    pub restore_clipboard: bool,
}

impl From<&Settings> for SelectionConfig {
    fn from(s: &Settings) -> Self {
        Self {
            right_click_chip: s.triggers.right_click_chip,
            selection_popup: s.triggers.selection_popup,
            excluded_apps: s.triggers.excluded_apps.clone(),
            restore_clipboard: s.triggers.restore_clipboard,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Info,
    Error,
}

/// What the service reports.
#[derive(Debug, Clone, PartialEq)]
pub enum SelectionEvent {
    /// Text captured (trimmed, non-empty). Also fires for a late copy after `NoSelection`.
    Read { text: String, source: ReadSource, foreground: Option<ForegroundInfo> },
    /// The capture found no text → toast `Select some text first`.
    NoSelection(ReadSource),
    /// A refusal (password, private text, password manager) or a clipboard-restore problem → toast.
    Notice { kind: NoticeKind, message: String },
    /// Show the chip at the cursor (physical pixels).
    ChipShow { x: i32, y: i32, mode: ChipMode },
    /// Hide the chip (every hide decision, activation and dismissal).
    ChipHide,
    /// Hook availability after an install attempt (`true` = installed, `false` = `SetWindowsHookExW` failed; sent after
    /// every attempt, including retries). The core keeps the last value as `Option<bool>` (`None` until the first
    /// attempt) and builds `AppInfo::native_hook_available` with `vox_core::app::native_hook_available` (only
    /// `Some(false)` shows the Settings → Triggers notice).
    HookStatus(bool),
}

/// Event listener. Called on the service's threads; must not block (vox-app routes ChipShow/ChipHide straight to the
/// UI thread and the rest to the core).
pub type SelectionEvents = Arc<dyn Fn(SelectionEvent) + Send + Sync>;

/// Thread-safe handle.
#[derive(Clone)]
pub struct SelectionService {
    inner: Arc<Inner>,
}

/// The handle's channels. Dropping the last handle disconnects the consumer (which then drops its hook and its
/// worker sender), which in turn ends the worker.
struct Inner {
    consumer: Sender<ConsumerMsg>,
    worker: Sender<WorkerMsg>,
    shared: Arc<Shared>,
}

impl SelectionService {
    /// Creates the service and its two long-lived threads (the selection consumer and the clipboard worker, module
    /// docs). The hook is not installed yet.
    pub fn new(events: SelectionEvents) -> Self {
        let shared = Arc::new(Shared::new(events));
        let (consumer_tx, consumer_rx) = crossbeam_channel::unbounded::<ConsumerMsg>();
        let (worker_tx, worker_rx) = crossbeam_channel::unbounded::<WorkerMsg>();
        {
            let shared = shared.clone();
            let worker = worker_tx.clone();
            let spawned = std::thread::Builder::new()
                .name("vox-selection".into())
                .spawn(move || consumer_thread(shared, consumer_rx, worker));
            if let Err(e) = spawned {
                tracing::error!(target: "selection", "cannot start the selection thread: {e}");
            }
        }
        {
            let shared = shared.clone();
            let spawned = std::thread::Builder::new()
                .name("vox-clipboard".into())
                .spawn(move || worker_thread(shared, worker_rx));
            if let Err(e) = spawned {
                tracing::error!(target: "selection", "cannot start the clipboard worker: {e}");
            }
        }
        SelectionService { inner: Arc::new(Inner { consumer: consumer_tx, worker: worker_tx, shared }) }
    }

    /// Starts with the given settings: installs the hook only if a chip trigger is on (each install attempt reports
    /// `HookStatus`). Never blocks (sets flags and enqueues).
    pub fn start(&self, cfg: SelectionConfig) {
        let shared = &self.inner.shared;
        shared.stopped.store(false, Ordering::SeqCst);
        shared.restore_clipboard.store(cfg.restore_clipboard, Ordering::SeqCst);
        let _ = self.inner.consumer.send(ConsumerMsg::Start(cfg));
    }

    /// Re-applies settings (may hide a chip; starts/stops the hook; retries a failed hook, D-SEL-1). Never blocks.
    pub fn apply_settings(&self, cfg: SelectionConfig) {
        // Read live by a capture in progress (selection.md §2.1).
        self.inner.shared.restore_clipboard.store(cfg.restore_clipboard, Ordering::SeqCst);
        let _ = self.inner.consumer.send(ConsumerMsg::Apply(cfg));
    }

    /// Stops the hook, aborts a running capture, starts flushing a pending clipboard restore. Only sets flags and
    /// enqueues; never blocks (the hook thread is joined by the consumer, not by the caller).
    pub fn stop(&self) {
        let shared = &self.inner.shared;
        shared.stopped.store(true, Ordering::SeqCst);
        if shared.busy.load(Ordering::SeqCst) {
            shared.abort.store(true, Ordering::SeqCst);
        }
        let _ = self.inner.consumer.send(ConsumerMsg::Stop);
        let _ = self.inner.worker.send(WorkerMsg::FlushRestore);
    }

    /// Waits (≤ `timeout`) for an in-flight capture and pending restore to finish: posts `Flush{reply}` to the
    /// clipboard worker and waits on the reply. The only method that waits.
    pub fn flush(&self, timeout: Duration) {
        let (tx, rx) = crossbeam_channel::bounded(1);
        if self.inner.worker.send(WorkerMsg::Flush(tx)).is_ok() && rx.recv_timeout(timeout).is_err() {
            tracing::warn!(target: "selection", "clipboard flush did not finish within {} ms", timeout.as_millis());
        }
    }

    /// `false` only after an install attempt failed; `true` before any attempt (chips off at launch, shots mode) and
    /// after a successful one. The core does not use it for `AppInfo` (it keeps the last `HookStatus`,
    /// `vox_core::app::native_hook_available`); kept for logs and tests.
    pub fn hook_available(&self) -> bool {
        self.inner.shared.hook_available.load(Ordering::SeqCst)
    }

    /// A capture is queued or running on the clipboard worker (an `AtomicBool` the worker sets).
    pub fn is_busy(&self) -> bool {
        self.inner.shared.busy.load(Ordering::SeqCst)
    }

    /// **Never blocks**: enqueues a capture on the clipboard worker and returns; ignored (debug log only, no event)
    /// while a capture is queued or running. Results arrive as `SelectionEvent`s. `via_context_menu` =
    /// `source == ContextChip`.
    pub fn read_selection(&self, source: ReadSource) {
        request_capture(&self.inner.shared, &self.inner.worker, source);
    }

    /// The chip window's real bounds while visible (physical px), `None` as soon as it starts hiding. vox-app reports
    /// bounds only after the overlay windows were styled (`AppUi::overlays_ready`).
    pub fn set_chip_bounds(&self, bounds: Option<PhysRect>) {
        *self.inner.shared.chip_bounds.lock() = bounds;
    }

    /// The chip UI got a left press (non-blocking enqueue; deduped with the hook's own detection within 300 ms).
    pub fn chip_activated(&self) {
        let _ = self.inner.consumer.send(ConsumerMsg::ChipActivated);
    }

    /// The chip UI got a non-left press.
    pub fn chip_dismissed(&self) {
        let _ = self.inner.consumer.send(ConsumerMsg::ChipDismissed);
    }
}

// ───────────────────────────── shared state ─────────────────────────────

/// State shared by the handle, the consumer and the worker (no channel senders: see [`Inner`]).
struct Shared {
    events: SelectionEvents,
    /// A capture is queued or running.
    busy: AtomicBool,
    /// Shutting down: captures emit nothing and new ones are ignored.
    stopped: AtomicBool,
    /// The running capture must give up (no more keys, no more polling).
    abort: AtomicBool,
    /// `triggers.restoreClipboard`, read live.
    restore_clipboard: AtomicBool,
    hook_available: AtomicBool,
    /// The consumer re-seeds its modifier mask (set by the worker after every capture).
    reseed_modifiers: AtomicBool,
    chip_bounds: Mutex<Option<PhysRect>>,
}

impl Shared {
    fn new(events: SelectionEvents) -> Self {
        Shared {
            events,
            busy: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            abort: AtomicBool::new(false),
            restore_clipboard: AtomicBool::new(true),
            hook_available: AtomicBool::new(true),
            reseed_modifiers: AtomicBool::new(false),
            chip_bounds: Mutex::new(None),
        }
    }

    /// Calls the listener; a panicking listener is logged and never propagates.
    fn emit(&self, ev: SelectionEvent) {
        let name = event_name(&ev);
        if catch_unwind(AssertUnwindSafe(|| (self.events)(ev))).is_err() {
            tracing::warn!(target: "selection", "'{name}' listener panicked");
        }
    }

    fn notice(&self, message: String) {
        self.emit(SelectionEvent::Notice { kind: NoticeKind::Info, message });
    }
}

fn event_name(ev: &SelectionEvent) -> &'static str {
    match ev {
        SelectionEvent::Read { .. } => "read",
        SelectionEvent::NoSelection(_) => "no-selection",
        SelectionEvent::Notice { .. } => "notice",
        SelectionEvent::ChipShow { .. } => "chip-show",
        SelectionEvent::ChipHide => "chip-hide",
        SelectionEvent::HookStatus(_) => "hook-status",
    }
}

/// `readSelection` entry: ignored while stopped or while a capture is queued or running; otherwise queued.
fn request_capture(shared: &Shared, worker: &Sender<WorkerMsg>, source: ReadSource) {
    if shared.stopped.load(Ordering::SeqCst) {
        tracing::debug!(target: "selection", "stopped, ignoring trigger from {source:?}");
        return;
    }
    if shared.busy.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        tracing::debug!(target: "selection", "capture already running, ignoring trigger from {source:?}");
        return;
    }
    let via_context_menu = source == ReadSource::ContextChip;
    if worker.send(WorkerMsg::Capture { source, via_context_menu }).is_err() {
        shared.busy.store(false, Ordering::SeqCst);
    }
}

/// `normalizeExe` (selection.md §7.2): trim, lower-case, last path segment, `.exe` forced; empty → `None`.
fn normalize_exe(name: &str) -> Option<String> {
    let lower = js_trim(name).to_lowercase();
    let base = lower.rsplit(['\\', '/']).next().unwrap_or("");
    if base.is_empty() {
        return None;
    }
    Some(if base.ends_with(".exe") { base.to_string() } else { format!("{base}.exe") })
}

/// `classify(info)` (selection.md §7.1): Vox itself, excluded apps and password managers are blocked; an unknown
/// process (`""`) is never blocked unless it is Vox.
fn classify(info: Option<&WindowInfo>, own_pid: u32, excluded: &[String]) -> Option<GestureTarget> {
    let info = info?;
    let name = info.info.process_name.as_str();
    let blocked = info.info.pid == own_pid
        || (!name.is_empty() && (excluded.iter().any(|e| e == name) || is_password_manager(name)));
    Some(GestureTarget { id: info.window_id(), blocked })
}

/// D-SEL-1: a failed hook is retried when the chip triggers differ from the last applied ones.
fn chip_flags_changed(prev: Option<(bool, bool)>, next: (bool, bool)) -> bool {
    prev != Some(next)
}

// ───────────────────────────── consumer: modifiers, clicks, activation ─────────────────────────────

/// Modifier state carried by gesture events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Mods {
    shift: bool,
    ctrl: bool,
    alt: bool,
    meta: bool,
}

/// The tracked side-specific modifier mask (D-SEL-7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct ModifierFlags {
    held: u8,
}

const VK_DELETE: u32 = 0x2E;

impl ModifierFlags {
    fn bit(vk: u32) -> Option<u8> {
        MODIFIER_VKS.iter().position(|&m| m == vk).map(|i| 1u8 << i)
    }

    fn mods(&self) -> Mods {
        // MODIFIER_VKS order: LShift, RShift, LCtrl, RCtrl, LAlt, RAlt, LWin, RWin.
        Mods {
            shift: self.held & 0b0000_0011 != 0,
            ctrl: self.held & 0b0000_1100 != 0,
            alt: self.held & 0b0011_0000 != 0,
            meta: self.held & 0b1100_0000 != 0,
        }
    }

    /// A keydown: the modifier's own keydown already carries its flag; Ctrl+Alt+(extended) Delete zeroes the mask
    /// afterwards (the secure desktop swallows the key-ups). Returns the flags for this event.
    fn key_down(&mut self, vk: u32, extended: bool) -> Mods {
        if let Some(bit) = Self::bit(vk) {
            self.held |= bit;
        }
        let mods = self.mods();
        if vk == VK_DELETE && extended && mods.ctrl && mods.alt {
            self.held = 0;
        }
        mods
    }

    fn key_up(&mut self, vk: u32) {
        if let Some(bit) = Self::bit(vk) {
            self.held &= !bit;
        }
    }

    fn reseed(&mut self, key_down: &dyn Fn(u32) -> bool) {
        self.held =
            MODIFIER_VKS.iter().filter(|&&vk| key_down(vk)).fold(0, |acc, &vk| acc | Self::bit(vk).unwrap_or(0));
    }

    fn clear(&mut self) {
        self.held = 0;
    }
}

/// libuiohook's multi-click counter (selection.md §4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct ClickCounter {
    count: u32,
    button: Option<MouseButton>,
    press_at: Option<u64>,
    press_point: Option<(i32, i32)>,
}

impl ClickCounter {
    fn press(&mut self, button: MouseButton, x: i32, y: i32, now: u64, double_click_ms: u64) -> u32 {
        let within = self.press_at.is_some_and(|t| now.saturating_sub(t) <= double_click_ms);
        if self.button == Some(button) && within {
            self.count = self.count.saturating_add(1);
        } else {
            self.count = 1;
            self.button = Some(button);
        }
        self.press_at = Some(now);
        self.press_point = Some((x, y));
        self.count
    }

    fn release(&mut self, now: u64, double_click_ms: u64) -> u32 {
        if self.press_at.is_some_and(|t| now.saturating_sub(t) > double_click_ms) {
            self.count = 0;
        }
        self.count
    }

    /// `None` when libuiohook would not dispatch the move (it is on the last press point).
    fn moved(&mut self, x: i32, y: i32, now: u64, double_click_ms: u64) -> Option<u32> {
        if self.press_point == Some((x, y)) {
            return None;
        }
        if self.press_at.is_some_and(|t| now.saturating_sub(t) > double_click_ms) {
            self.count = 0;
        }
        Some(self.count)
    }

    fn wheel(&mut self) -> u32 {
        self.count = 1;
        self.button = None;
        1
    }
}

/// The 300 ms global activation dedupe (selection.md §6.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct ActivationGate {
    last: Option<u64>,
}

impl ActivationGate {
    fn try_pass(&mut self, now: u64) -> bool {
        if self.last.is_some_and(|t| now.saturating_sub(t) < CHIP_ACTIVATION_DEDUPE_MS) {
            return false;
        }
        self.last = Some(now);
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MouseKind {
    Down(MouseButton),
    Up(MouseButton),
    Move,
    Wheel,
}

/// A hook mouse message → what it means (`XBUTTON1/2` from the high word of `mouseData`).
fn mouse_kind(msg: u32, mouse_data: u32) -> Option<MouseKind> {
    let x_button = || match mouse_data >> 16 {
        1 => Some(MouseButton::X1),
        2 => Some(MouseButton::X2),
        _ => None,
    };
    Some(match msg {
        WM_LBUTTONDOWN => MouseKind::Down(MouseButton::Left),
        WM_LBUTTONUP => MouseKind::Up(MouseButton::Left),
        WM_RBUTTONDOWN => MouseKind::Down(MouseButton::Right),
        WM_RBUTTONUP => MouseKind::Up(MouseButton::Right),
        WM_MBUTTONDOWN => MouseKind::Down(MouseButton::Middle),
        WM_MBUTTONUP => MouseKind::Up(MouseButton::Middle),
        WM_XBUTTONDOWN => MouseKind::Down(x_button()?),
        WM_XBUTTONUP => MouseKind::Up(x_button()?),
        WM_MOUSEMOVE => MouseKind::Move,
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => MouseKind::Wheel,
        _ => return None,
    })
}

// ───────────────────────────── consumer core (testable) ─────────────────────────────

/// What the consumer asks of Windows (real: Win32; tests: a fake).
trait ConsumerEnv {
    fn window_info_at(&self, x: i32, y: i32) -> Option<WindowInfo>;
    fn foreground_window_info(&self) -> Option<WindowInfo>;
    fn own_overlay_at(&self, x: i32, y: i32) -> Option<bool>;
    fn text_cursor(&self) -> Option<bool>;
    fn fullscreen(&self) -> bool;
    /// `GetAsyncKeyState(vk) & 0x8000`.
    fn key_down(&self, vk: u32) -> bool;
    fn foreground_hwnd(&self) -> isize;
    fn double_click_ms(&self) -> u64;
    fn own_pid(&self) -> u32;
}

/// What the consumer core wants done.
#[derive(Debug, Clone, PartialEq)]
enum Output {
    Event(SelectionEvent),
    Capture(ReadSource),
}

/// The machine's queries answered from the environment and the consumer's own state.
struct Deps<'a, E: ConsumerEnv> {
    env: &'a E,
    excluded: &'a [String],
    bounds: Option<PhysRect>,
}

impl<E: ConsumerEnv> GestureDeps for Deps<'_, E> {
    fn is_inside_chip(&self, x: i32, y: i32) -> bool {
        // Half-open bounds first; then a menu opened later above the chip must not count (unknown: bounds decide).
        self.bounds.is_some_and(|b| b.contains(x, y)) && self.env.own_overlay_at(x, y).unwrap_or(true)
    }
    fn target_at(&self, x: i32, y: i32) -> Option<GestureTarget> {
        let info = self.env.window_info_at(x, y).or_else(|| self.env.foreground_window_info());
        classify(info.as_ref(), self.env.own_pid(), self.excluded)
    }
    fn focused_target(&self) -> Option<GestureTarget> {
        classify(self.env.foreground_window_info().as_ref(), self.env.own_pid(), self.excluded)
    }
    fn own_overlay_at(&self, x: i32, y: i32) -> bool {
        self.env.own_overlay_at(x, y) == Some(true)
    }
    fn text_cursor(&self) -> Option<bool> {
        self.env.text_cursor()
    }
    fn chips_suppressed(&self) -> bool {
        self.env.fullscreen()
    }
}

/// The consumer's event handling (selection.md §4.1, §5.6, §6): raw input → machine → outputs.
struct Core<E: ConsumerEnv> {
    env: E,
    machine: GestureMachine,
    mods: ModifierFlags,
    clicks: ClickCounter,
    excluded: Vec<String>,
    last_chip_mode: ChipMode,
    gate: ActivationGate,
    last_foreground: Option<isize>,
}

impl<E: ConsumerEnv> Core<E> {
    fn new(env: E) -> Self {
        Core {
            env,
            machine: GestureMachine::new(GestureConfig { context_chip: false, selection_chip: false }),
            mods: ModifierFlags::default(),
            clicks: ClickCounter::default(),
            excluded: Vec::new(),
            last_chip_mode: ChipMode::Context,
            gate: ActivationGate::default(),
            last_foreground: None,
        }
    }

    fn reseed_modifiers(&mut self) {
        let env = &self.env;
        self.mods.reseed(&|vk| env.key_down(vk));
    }

    /// Re-seeds the mask when the foreground window changed since the previous event.
    fn check_foreground(&mut self) {
        let fg = self.env.foreground_hwnd();
        if self.last_foreground != Some(fg) {
            self.last_foreground = Some(fg);
            self.reseed_modifiers();
        }
    }

    fn configure(&mut self, cfg: &SelectionConfig, now: u64, out: &mut Vec<Output>) {
        self.excluded = cfg.excluded_apps.iter().filter_map(|e| normalize_exe(e)).collect();
        let decisions = self
            .machine
            .configure(GestureConfig { context_chip: cfg.right_click_chip, selection_chip: cfg.selection_popup });
        self.apply(decisions, now, out);
    }

    /// The hook stopped: forget held modifiers and every gesture.
    fn reset(&mut self, now: u64, out: &mut Vec<Output>) {
        self.mods.clear();
        let decisions = self.machine.reset();
        self.apply(decisions, now, out);
    }

    fn tick(&mut self, now: u64, out: &mut Vec<Output>) {
        let decisions = self.machine.tick(now);
        self.apply(decisions, now, out);
    }

    fn on_raw(&mut self, raw: RawInput, now: u64, busy: bool, bounds: Option<PhysRect>, out: &mut Vec<Output>) {
        self.check_foreground();
        let (kind, x, y, clicks, mods) = match raw {
            RawInput::Key { down: false, vk, .. } => {
                self.mods.key_up(vk);
                return;
            }
            RawInput::Key { down: true, vk, extended, .. } => {
                let mods = self.mods.key_down(vk, extended);
                // Our own synthetic Ctrl+C / Escape (and the user's keys) must not touch the hint during a capture.
                if busy {
                    return;
                }
                (GestureEventKind::KeyDown { vk, extended }, 0, 0, 0, mods)
            }
            RawInput::Mouse { msg, x, y, mouse_data, .. } => {
                let Some(kind) = mouse_kind(msg, mouse_data) else { return };
                let dbl = self.env.double_click_ms();
                match kind {
                    MouseKind::Down(button) => {
                        // Authoritative modifier state for presses (immune to lost key-ups).
                        self.reseed_modifiers();
                        let clicks = self.clicks.press(button, x, y, now, dbl);
                        (GestureEventKind::MouseDown(button), x, y, clicks, self.mods.mods())
                    }
                    MouseKind::Up(button) => {
                        let clicks = self.clicks.release(now, dbl);
                        (GestureEventKind::MouseUp(button), x, y, clicks, self.mods.mods())
                    }
                    MouseKind::Move => {
                        if !self.machine.wants_mouse_move() {
                            return;
                        }
                        let Some(clicks) = self.clicks.moved(x, y, now, dbl) else { return };
                        (GestureEventKind::MouseMove, x, y, clicks, self.mods.mods())
                    }
                    MouseKind::Wheel => (GestureEventKind::Wheel, x, y, self.clicks.wheel(), self.mods.mods()),
                }
            }
        };
        let ev = GestureEvent {
            kind,
            x,
            y,
            clicks,
            time_ms: now,
            shift: mods.shift,
            ctrl: mods.ctrl,
            alt: mods.alt,
            meta: mods.meta,
        };
        let decisions = {
            let deps = Deps { env: &self.env, excluded: &self.excluded, bounds };
            self.machine.handle(&ev, &deps)
        };
        self.apply(decisions, now, out);
    }

    /// selection.md §5.6.
    fn apply(&mut self, decisions: Vec<Decision>, now: u64, out: &mut Vec<Output>) {
        for d in decisions {
            match d {
                Decision::ShowContextChip { x, y } => self.show_chip(ChipMode::Context, x, y, out),
                Decision::ShowSelectionChip { x, y } => self.show_chip(ChipMode::Selection, x, y, out),
                Decision::HideChip(_) => out.push(Output::Event(SelectionEvent::ChipHide)),
                Decision::ActivateChip(mode) => self.activate(mode, now, out),
                Decision::HintSet(kind) => tracing::debug!(target: "selection", "hint-set {kind:?}"),
                Decision::HintCleared(reason) => tracing::debug!(target: "selection", "hint-cleared {reason:?}"),
            }
        }
    }

    fn show_chip(&mut self, mode: ChipMode, x: i32, y: i32, out: &mut Vec<Output>) {
        self.last_chip_mode = mode;
        out.push(Output::Event(SelectionEvent::ChipShow { x, y, mode }));
    }

    /// selection.md §6.3 (global 300 ms dedupe; the machine's hide decision is replaced by a direct `ChipHide`).
    fn activate(&mut self, mode: ChipMode, now: u64, out: &mut Vec<Output>) {
        if !self.gate.try_pass(now) {
            return;
        }
        let _ = self.machine.hide_chip(crate::gestures::ChipHideReason::Activated);
        out.push(Output::Event(SelectionEvent::ChipHide));
        out.push(Output::Capture(mode.read_source()));
    }

    fn chip_activated(&mut self, now: u64, out: &mut Vec<Output>) {
        let mode = self.machine.chip_mode().unwrap_or(self.last_chip_mode);
        self.activate(mode, now, out);
    }

    fn chip_dismissed(&mut self, out: &mut Vec<Output>) {
        let _ = self.machine.hide_chip(crate::gestures::ChipHideReason::Dismissed);
        out.push(Output::Event(SelectionEvent::ChipHide));
    }
}

/// D-SEL-9: a hook Windows removed silently (no procedure call for a while although the session saw input).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Watchdog {
    last_count: u64,
    last_input: Option<u32>,
    /// When the hook last showed life (ms).
    alive_at: u64,
    /// Input arrived that the hook did not see.
    suspect: bool,
    /// The previous check's foreground was one whose input our hook sees. Input that arrived across an untrusted
    /// period (the PIN typed on the lock screen, keys sent to an elevated window) raises no suspicion.
    last_trusted: bool,
    /// Reinstalls since the hook last showed life (backs the silence window off: 10 s, 20 s, 40 s … ≤ 5 min).
    strikes: u32,
}

impl Watchdog {
    fn new(now: u64, count: u64, input: Option<u32>) -> Self {
        Watchdog { last_count: count, last_input: input, alive_at: now, suspect: false, last_trusted: true, strikes: 0 }
    }

    fn silence_needed(&self) -> u64 {
        HOOK_SILENCE_REINSTALL_MS.saturating_mul(1u64 << self.strikes.min(5)).min(300_000)
    }

    /// One periodic check; `true` = reinstall now. `foreground_visible` probes whether our hook sees input to the
    /// current foreground window (false for an elevated window, the secure desktop, the lock screen); it is called
    /// at **every** check, idle ones included, because the next check judges input that arrived since this one by
    /// both ends: a burst typed on the lock screen (or in an elevated window) right before a switch to a normal window
    /// must not count against the hook just because the previous check happened to see no input.
    fn check(&mut self, now: u64, count: u64, input: Option<u32>, foreground_visible: impl FnOnce() -> bool) -> bool {
        let trusted_foreground = foreground_visible();
        if count != self.last_count {
            *self = Watchdog { last_trusted: trusted_foreground, ..Watchdog::new(now, count, input) };
            return false;
        }
        let moved = input.is_some() && input != self.last_input;
        if moved && trusted_foreground && self.last_trusted {
            self.suspect = true;
        }
        self.last_input = input;
        self.last_trusted = trusted_foreground;
        self.suspect && now.saturating_sub(self.alive_at) >= self.silence_needed()
    }

    /// After a reinstall: start over, with one more strike.
    fn reinstalled(&mut self, now: u64, count: u64, input: Option<u32>) {
        let strikes = self.strikes.saturating_add(1);
        *self = Watchdog { strikes, ..Watchdog::new(now, count, input) };
    }
}

const WATCHDOG_INTERVAL_MS: u64 = 2_000;

// ───────────────────────────── consumer thread ─────────────────────────────

enum ConsumerMsg {
    Start(SelectionConfig),
    Apply(SelectionConfig),
    Stop,
    ChipActivated,
    ChipDismissed,
}

/// The real consumer environment.
struct Win32Consumer {
    own_pid: u32,
}

impl ConsumerEnv for Win32Consumer {
    fn window_info_at(&self, x: i32, y: i32) -> Option<WindowInfo> {
        crate::foreground::window_info_at(x, y)
    }
    fn foreground_window_info(&self) -> Option<WindowInfo> {
        crate::foreground::foreground_window_info()
    }
    fn own_overlay_at(&self, x: i32, y: i32) -> Option<bool> {
        crate::foreground::is_own_overlay_at(x, y)
    }
    fn text_cursor(&self) -> Option<bool> {
        crate::foreground::text_cursor_shown()
    }
    fn fullscreen(&self) -> bool {
        crate::foreground::fullscreen_app_running()
    }
    fn key_down(&self, vk: u32) -> bool {
        crate::keys::key_down(vk)
    }
    fn foreground_hwnd(&self) -> isize {
        // SAFETY: no arguments.
        unsafe { GetForegroundWindow() }.0 as isize
    }
    fn double_click_ms(&self) -> u64 {
        // SAFETY: no arguments; read on every press so a Control Panel change applies at once.
        u64::from(unsafe { GetDoubleClickTime() })
    }
    fn own_pid(&self) -> u32 {
        self.own_pid
    }
}

/// The consumer thread's state around [`Core`]: hook lifecycle, settings, timers.
struct Consumer {
    shared: Arc<Shared>,
    worker: Sender<WorkerMsg>,
    core: Core<Win32Consumer>,
    epoch: Instant,
    raw_tx: Sender<RawInput>,
    raw_rx: Receiver<RawInput>,
    hook: Option<InputHook>,
    started: bool,
    cfg: SelectionConfig,
    last_flags: Option<(bool, bool)>,
    last_attempt_failed: bool,
    watchdog: Option<Watchdog>,
    next_watchdog_at: u64,
}

impl Consumer {
    fn now(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn deliver(&self, out: Vec<Output>) {
        for o in out {
            match o {
                Output::Event(ev) => self.shared.emit(ev),
                Output::Capture(source) => request_capture(&self.shared, &self.worker, source),
            }
        }
    }

    fn want_hook(&self) -> bool {
        self.started && (self.cfg.right_click_chip || self.cfg.selection_popup)
    }

    fn apply_config(&mut self, cfg: SelectionConfig, force_retry: bool) {
        let flags = (cfg.right_click_chip, cfg.selection_popup);
        let retry = force_retry || chip_flags_changed(self.last_flags, flags);
        self.cfg = cfg;
        self.last_flags = Some(flags);
        let mut out = Vec::new();
        let now = self.now();
        self.core.configure(&self.cfg, now, &mut out);
        self.deliver(out);
        self.sync_hook(retry);
    }

    /// selection.md §3 `syncHook`, with the D-SEL-1 retry.
    fn sync_hook(&mut self, retry: bool) {
        let want = self.want_hook();
        if want && self.hook.is_none() {
            if self.last_attempt_failed && !retry {
                return;
            }
            self.install_hook();
        } else if !want && self.hook.is_some() {
            self.remove_hook();
        }
    }

    fn install_hook(&mut self) {
        match InputHook::start(self.raw_tx.clone()) {
            Ok(hook) => {
                hook.set_wants_mouse_move(self.core.machine.wants_mouse_move());
                let now = self.now();
                self.watchdog = Some(Watchdog::new(now, hook.event_count(), crate::hook::last_input_tick()));
                self.next_watchdog_at = now + WATCHDOG_INTERVAL_MS;
                self.hook = Some(hook);
                self.last_attempt_failed = false;
                self.shared.hook_available.store(true, Ordering::SeqCst);
                self.core.reseed_modifiers();
                tracing::info!(target: "selection", "global input hook installed");
                self.shared.emit(SelectionEvent::HookStatus(true));
            }
            Err(e) => {
                self.last_attempt_failed = true;
                self.shared.hook_available.store(false, Ordering::SeqCst);
                tracing::warn!(target: "selection", "global hook failed to start: {e}");
                self.shared.emit(SelectionEvent::HookStatus(false));
            }
        }
    }

    fn remove_hook(&mut self) {
        if let Some(hook) = self.hook.take() {
            hook.stop();
        }
        self.watchdog = None;
        // Events still queued from the old hook belong to nothing now.
        for _ in self.raw_rx.try_iter() {}
        let mut out = Vec::new();
        let now = self.now();
        self.core.reset(now, &mut out);
        self.deliver(out);
    }

    fn handle_msg(&mut self, msg: ConsumerMsg) {
        let now = self.now();
        let mut out = Vec::new();
        match msg {
            ConsumerMsg::Start(cfg) => {
                self.started = true;
                self.apply_config(cfg, true);
            }
            ConsumerMsg::Apply(cfg) => self.apply_config(cfg, false),
            ConsumerMsg::Stop => {
                self.started = false;
                self.sync_hook(false);
                self.core.reset(now, &mut out);
            }
            ConsumerMsg::ChipActivated => self.core.chip_activated(now, &mut out),
            ConsumerMsg::ChipDismissed => self.core.chip_dismissed(&mut out),
        }
        self.deliver(out);
    }

    fn handle_raw(&mut self, raw: RawInput) {
        if self.hook.is_none() {
            return;
        }
        if self.shared.reseed_modifiers.swap(false, Ordering::SeqCst) {
            self.core.reseed_modifiers();
        }
        let now = self.now();
        let busy = self.shared.busy.load(Ordering::SeqCst);
        let bounds = *self.shared.chip_bounds.lock();
        let mut out = Vec::new();
        self.core.on_raw(raw, now, busy, bounds, &mut out);
        if let Some(hook) = &self.hook {
            hook.set_wants_mouse_move(self.core.machine.wants_mouse_move());
        }
        self.deliver(out);
    }

    /// The single deadline timer (hint expiry / chip auto-hide), armed only while started.
    fn gesture_deadline(&self) -> Option<u64> {
        if self.started {
            self.core.machine.next_deadline()
        } else {
            None
        }
    }

    fn run_timers(&mut self) {
        let now = self.now();
        if self.gesture_deadline().is_some_and(|d| d <= now) {
            let mut out = Vec::new();
            self.core.tick(now, &mut out);
            self.deliver(out);
        }
        if self.hook.is_some() && now >= self.next_watchdog_at {
            self.next_watchdog_at = now + WATCHDOG_INTERVAL_MS;
            self.check_watchdog(now);
        }
    }

    fn check_watchdog(&mut self, now: u64) {
        let (Some(hook), Some(mut dog)) = (self.hook.as_ref(), self.watchdog) else { return };
        let count = hook.event_count();
        let input = crate::hook::last_input_tick();
        // The foreground is probed at every check (one PostMessage per 2 s), see `Watchdog::check`.
        let reinstall = dog.check(now, count, input, foreground_input_visible);
        self.watchdog = Some(dog);
        if reinstall {
            tracing::warn!(target: "selection", "global hook was removed by Windows; reinstalling");
            if let Some(old) = self.hook.take() {
                old.stop();
            }
            self.install_hook();
            let now = self.now();
            if let (Some(hook), Some(fresh)) = (self.hook.as_ref(), self.watchdog.as_mut()) {
                // Keep the strike count of the old hook's watchdog (back-off).
                *fresh = dog;
                fresh.reinstalled(now, hook.event_count(), crate::hook::last_input_tick());
            }
        }
    }

    fn next_wake(&self) -> Option<u64> {
        let gesture = self.gesture_deadline().map(|d| d.saturating_add(5));
        let watchdog = self.hook.as_ref().map(|_| self.next_watchdog_at);
        match (gesture, watchdog) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

/// False when the foreground window's input is invisible to a non-elevated LL hook (an elevated window) or there is
/// no foreground window (the secure desktop, a locked session). Cheap: no window text or process image is read.
fn foreground_input_visible() -> bool {
    match crate::foreground::foreground_hwnd_pid() {
        Some((hwnd, pid)) => crate::foreground::input_blocked_by_uipi(hwnd, pid) != Some(true),
        None => false,
    }
}

fn consumer_thread(shared: Arc<Shared>, rx: Receiver<ConsumerMsg>, worker: Sender<WorkerMsg>) {
    let (raw_tx, raw_rx) = crossbeam_channel::unbounded::<RawInput>();
    // SAFETY: no arguments.
    let own_pid = unsafe { windows::Win32::System::Threading::GetCurrentProcessId() };
    let mut c = Consumer {
        shared,
        worker,
        core: Core::new(Win32Consumer { own_pid }),
        epoch: Instant::now(),
        raw_tx,
        raw_rx: raw_rx.clone(),
        hook: None,
        started: false,
        cfg: SelectionConfig {
            right_click_chip: false,
            selection_popup: false,
            excluded_apps: Vec::new(),
            restore_clipboard: true,
        },
        last_flags: None,
        last_attempt_failed: false,
        watchdog: None,
        next_watchdog_at: 0,
    };
    let mut last_warn: Option<Instant> = None;
    loop {
        let wait = c.next_wake().map(|at| Duration::from_millis(at.saturating_sub(c.now())));
        enum Woke {
            Msg(ConsumerMsg),
            Raw(RawInput),
            Timer,
            Closed,
        }
        let woke = match wait {
            Some(wait) => select! {
                recv(rx) -> m => m.map_or(Woke::Closed, Woke::Msg),
                recv(raw_rx) -> r => r.map_or(Woke::Timer, Woke::Raw),
                default(wait) => Woke::Timer,
            },
            None => select! {
                recv(rx) -> m => m.map_or(Woke::Closed, Woke::Msg),
                recv(raw_rx) -> r => r.map_or(Woke::Timer, Woke::Raw),
            },
        };
        let closed = matches!(woke, Woke::Closed);
        let result = catch_unwind(AssertUnwindSafe(|| {
            match woke {
                Woke::Msg(m) => c.handle_msg(m),
                Woke::Raw(r) => c.handle_raw(r),
                Woke::Timer | Woke::Closed => {}
            }
            c.run_timers();
        }));
        if result.is_err() {
            // Never let one bad event end gesture handling; log at most once per 10 s.
            let now = Instant::now();
            if last_warn.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(10)) {
                last_warn = Some(now);
                tracing::warn!(target: "selection", "gesture handling failed (panic)");
            }
        }
        if closed {
            break;
        }
    }
    if let Some(hook) = c.hook.take() {
        hook.stop();
    }
}

// ───────────────────────────── clipboard worker ─────────────────────────────

enum WorkerMsg {
    Capture {
        source: ReadSource,
        via_context_menu: bool,
    },
    /// `stop()`: run the pending clipboard job now (not awaited).
    FlushRestore,
    /// `flush(timeout)`: the in-flight capture has finished (FIFO); flush and reply.
    Flush(Sender<()>),
}

/// What the capture pipeline asks of Windows (real: clipboard/keys/foreground; tests: a simulated clipboard and
/// app with a virtual clock).
trait CaptureEnv {
    fn seq(&self) -> u32;
    /// `CF_UNICODETEXT` (`""` on any error).
    fn text(&self) -> String;
    fn has_text_format(&self) -> bool;
    fn privacy(&self) -> Option<Privacy>;
    fn snapshot(&self) -> ClipboardSnapshot;
    fn restore(&self, snapshot: &Arc<ClipboardSnapshot>) -> bool;
    fn clear(&self) -> bool;
    fn send_chord(&self, method: CopyMethod) -> bool;
    fn send_escape(&self) -> bool;
    fn held_modifiers(&self) -> Vec<u32>;
    fn release_keys(&self, vks: &[u32]);
    fn foreground(&self) -> Option<WindowInfo>;
    fn uipi_blocked(&self, hwnd: isize, pid: u32) -> Option<bool>;
    fn now(&self) -> u64;
    fn sleep(&self, ms: u64);
}

/// The real capture environment.
struct Win32Capture {
    epoch: Instant,
}

impl CaptureEnv for Win32Capture {
    fn seq(&self) -> u32 {
        crate::clipboard::sequence_number()
    }
    fn text(&self) -> String {
        crate::clipboard::read_text_raw().unwrap_or_default()
    }
    fn has_text_format(&self) -> bool {
        crate::clipboard::has_text_format()
    }
    fn privacy(&self) -> Option<Privacy> {
        crate::clipboard::privacy()
    }
    fn snapshot(&self) -> ClipboardSnapshot {
        crate::clipboard::snapshot()
    }
    fn restore(&self, snapshot: &Arc<ClipboardSnapshot>) -> bool {
        match crate::clipboard::restore_shared(snapshot.clone()) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(target: "selection", "clipboard restore failed: {e}");
                false
            }
        }
    }
    fn clear(&self) -> bool {
        match crate::clipboard::clear_clipboard() {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(target: "selection", "clipboard clear failed: {e}");
                false
            }
        }
    }
    fn send_chord(&self, method: CopyMethod) -> bool {
        crate::keys::send_chord(method)
    }
    fn send_escape(&self) -> bool {
        crate::keys::send_escape()
    }
    fn held_modifiers(&self) -> Vec<u32> {
        crate::keys::held_modifier_vks()
    }
    fn release_keys(&self, vks: &[u32]) {
        crate::keys::release_keys(vks);
    }
    fn foreground(&self) -> Option<WindowInfo> {
        crate::foreground::foreground_window_info()
    }
    fn uipi_blocked(&self, hwnd: isize, pid: u32) -> Option<bool> {
        crate::foreground::input_blocked_by_uipi(hwnd, pid)
    }
    fn now(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
    fn sleep(&self, ms: u64) {
        std::thread::sleep(Duration::from_millis(ms));
    }
}

/// What one capture produced (selection.md §8.2).
#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    Text { text: String, foreground: Option<ForegroundInfo>, notice: Option<String> },
    Empty(Option<String>),
    Refused(String),
    Aborted,
}

/// One capture's clipboard context (selection.md §8.3 `ctx`; no stale/cleared fallback, D-SEL-5).
#[derive(Debug, Clone)]
struct CaptureCtx {
    snapshot: Arc<ClipboardSnapshot>,
    since: u32,
    foreground: Option<ForegroundInfo>,
    chords: u32,
    last_chord_at: u64,
    source: ReadSource,
}

/// The pending clipboard job (at most one, selection.md §8.8).
enum Job {
    Restore { snapshot: Arc<ClipboardSnapshot>, if_still: Option<(String, u32)>, secret: bool, echo_of: Option<String> },
    LateCopy { ctx: CaptureCtx, deadline: u64 },
    Echo { snapshot: Arc<ClipboardSnapshot>, text: String, secret: bool, from: u32, deadline: u64 },
}

struct Pending {
    job: Job,
    due: u64,
}

/// `flushRestore` rounds (a finish can queue the next job: late copy → restore → echo guard).
const FLUSH_ROUNDS: usize = 4;

/// Runs the pending job now, then whatever it queued, at most [`FLUSH_ROUNDS`] times; returns how many ran.
fn drain_bounded<S, J>(state: &mut S, take: fn(&mut S) -> Option<J>, run: fn(&mut S, J)) -> usize {
    let mut ran = 0;
    while ran < FLUSH_ROUNDS {
        let Some(job) = take(state) else { break };
        run(state, job);
        ran += 1;
    }
    ran
}

/// `refusal(privacy)`.
fn refusal(p: Privacy) -> String {
    match p {
        Privacy::Secret => copy::secret_refused(),
        Privacy::Private => copy::private_refused(),
    }
}

/// `lossNotice(snapshot)` (selection.md §2.4).
fn loss_notice(snapshot: &ClipboardSnapshot) -> Option<String> {
    match (snapshot.failed, snapshot.incomplete) {
        (_, true) => Some(copy::CLIPBOARD_TOO_LARGE.to_string()),
        (true, false) => Some(copy::CLIPBOARD_UNREADABLE.to_string()),
        (false, false) => None,
    }
}

fn is_blank(s: &str) -> bool {
    js_trim(s).is_empty()
}

/// The serial clipboard worker (captures, restores, watches).
struct Worker<E: CaptureEnv> {
    env: E,
    shared: Arc<Shared>,
    pending: Option<Pending>,
    seq_warned: bool,
}

impl<E: CaptureEnv> Worker<E> {
    fn new(env: E, shared: Arc<Shared>) -> Self {
        Worker { env, shared, pending: None, seq_warned: false }
    }

    fn aborted(&self) -> bool {
        self.shared.abort.load(Ordering::SeqCst)
    }

    fn handle(&mut self, msg: WorkerMsg) {
        match msg {
            WorkerMsg::Capture { source, via_context_menu } => self.capture(source, via_context_menu),
            WorkerMsg::FlushRestore => self.flush_restore(),
            WorkerMsg::Flush(reply) => {
                self.flush_restore();
                let _ = reply.send(());
            }
        }
    }

    /// `readSelection` after the busy check (selection.md §8.2): capture, then emit.
    fn capture(&mut self, source: ReadSource, via_context_menu: bool) {
        self.shared.abort.store(false, Ordering::SeqCst);
        let outcome = if self.shared.stopped.load(Ordering::SeqCst) {
            Outcome::Aborted
        } else {
            match catch_unwind(AssertUnwindSafe(|| self.run_capture(source, via_context_menu))) {
                Ok(outcome) => outcome,
                Err(_) => {
                    tracing::warn!(target: "selection", "selection capture failed (panic)");
                    Outcome::Empty(None)
                }
            }
        };
        self.shared.abort.store(false, Ordering::SeqCst);
        self.shared.busy.store(false, Ordering::SeqCst);
        self.shared.reseed_modifiers.store(true, Ordering::SeqCst);
        // Shutting down: no TTS request, no toast.
        if self.shared.stopped.load(Ordering::SeqCst) {
            return;
        }
        match outcome {
            Outcome::Text { text, foreground, notice } => {
                self.shared.emit(SelectionEvent::Read { text, source, foreground });
                if let Some(n) = notice {
                    self.shared.notice(n);
                }
            }
            Outcome::Empty(notice) => {
                self.shared.emit(SelectionEvent::NoSelection(source));
                if let Some(n) = notice {
                    self.shared.notice(n);
                }
            }
            Outcome::Refused(message) => self.shared.notice(message),
            Outcome::Aborted => {}
        }
    }

    /// selection.md §8.3 with the D-SEL-2 order and D-SEL-5.
    fn run_capture(&mut self, source: ReadSource, via_context_menu: bool) -> Outcome {
        self.flush_restore();
        let win = self.env.foreground();
        let foreground = win.as_ref().map(|w| w.info.clone());
        let process = win.as_ref().map_or("", |w| w.info.process_name.as_str());
        // In KeePass, Ctrl+C in the entry list copies the selected entry's password: send nothing at all.
        if is_password_manager(process) {
            return Outcome::Refused(copy::password_manager_refused());
        }
        let class = win.as_ref().map_or("", |w| w.class_name.as_str());
        let method = crate::keys::copy_method_for(process, class);
        if method == CopyMethod::ReadClipboard {
            return self.read_copy_on_select(foreground);
        }
        if let Some(w) = &win {
            if self.env.uipi_blocked(w.hwnd, w.info.pid) == Some(true) {
                tracing::info!(target: "selection", "foreground app runs elevated; not sending the copy shortcut");
                return Outcome::Refused(copy::elevated_app_refused());
            }
        }
        let forced = {
            let env = &self.env;
            let shared = &self.shared;
            crate::keys::wait_released_with(
                MODIFIER_WAIT_MS,
                MODIFIER_POLL_MS,
                &|| shared.abort.load(Ordering::SeqCst),
                &mut || env.held_modifiers(),
                &mut |k| env.release_keys(k),
                &|| env.now(),
                &|ms| env.sleep(ms),
            )
        };
        if !forced.is_empty() {
            tracing::debug!(target: "selection", "force-released modifiers {forced:x?}");
        }
        self.env.sleep(if via_context_menu { CONTEXT_MENU_SETTLE_MS } else { CAPTURE_SETTLE_MS });
        if self.aborted() {
            return Outcome::Aborted;
        }
        let snapshot = Arc::new(self.env.snapshot());
        if self.aborted() {
            return Outcome::Aborted;
        }
        // Taken once, before the first chord: a late copy from attempt 1 still counts during attempt 2, and the
        // clipboard is never cleared (stale text is told apart by the sequence number).
        let since = self.env.seq();
        if since == 0 {
            if !self.seq_warned {
                self.seq_warned = true;
                tracing::warn!(target: "selection", "clipboard sequence unavailable");
            }
            return Outcome::Empty(None);
        }
        let mut ctx =
            CaptureCtx { snapshot, since, foreground: foreground.clone(), chords: 0, last_chord_at: 0, source };
        let escape_retry = via_context_menu && matches!(method, CopyMethod::CtrlC { escape_retry: true });
        let attempt = catch_unwind(AssertUnwindSafe(|| {
            let raw = self.copy_and_read(method, escape_retry, &mut ctx);
            // Check the markers while the copied data is still on the clipboard. The text itself is never logged.
            let privacy = if is_blank(&raw) { None } else { self.env.privacy() };
            (raw, privacy)
        }));
        let (raw, privacy, failed) = match attempt {
            Ok((raw, privacy)) => (raw, privacy, false),
            Err(_) => {
                tracing::warn!(target: "selection", "selection copy failed (panic)");
                (String::new(), None, true)
            }
        };
        // On abort and on errors too: once a chord might have been sent, the restore decision is never skipped.
        let notice = self.settle_clipboard(&ctx, &raw, privacy.is_some());
        if let Some(p) = privacy {
            return Outcome::Refused(refusal(p));
        }
        if failed {
            // D-SEL-Q8: the notice is lost and the outcome is "no selection".
            return Outcome::Empty(None);
        }
        if self.aborted() {
            return Outcome::Aborted;
        }
        let text = js_trim(&raw);
        if text.is_empty() {
            Outcome::Empty(notice)
        } else {
            Outcome::Text { text: text.to_string(), foreground, notice }
        }
    }

    /// Copy-on-select terminals (§8.11): no modifier wait, no snapshot, no keys, no restore, no watch.
    fn read_copy_on_select(&mut self, foreground: Option<ForegroundInfo>) -> Outcome {
        let raw = self.env.text();
        let text = js_trim(&raw);
        if text.is_empty() {
            return Outcome::Empty(None);
        }
        if let Some(p) = self.env.privacy() {
            return Outcome::Refused(refusal(p));
        }
        Outcome::Text { text: text.to_string(), foreground, notice: None }
    }

    /// selection.md §8.5 (untrimmed text, or `""`).
    fn copy_and_read(&self, method: CopyMethod, escape_retry: bool, ctx: &mut CaptureCtx) -> String {
        let attempts = if escape_retry { 2 } else { 1 };
        for attempt in 0..attempts {
            if self.aborted() {
                return String::new();
            }
            if attempt > 0 {
                // The first copy did arrive, only without text (an image, files): asking again would repeat it.
                if self.env.seq() != ctx.since {
                    break;
                }
                // A context menu may still own the keyboard: close it and try once more.
                if !self.env.send_escape() {
                    break;
                }
                self.env.sleep(ESCAPE_SETTLE_MS);
            }
            if !self.env.send_chord(method) {
                tracing::warn!(target: "selection", "cannot send the copy shortcut (SendInput failed)");
                return String::new();
            }
            ctx.chords += 1;
            ctx.last_chord_at = self.env.now();
            let text = self.wait_for_text(ctx.since, &|| self.aborted());
            if !is_blank(&text) {
                return text;
            }
        }
        String::new()
    }

    /// `waitForClipboardText` (selection.md §8.6, sequence-number variant): fresh non-empty text after `since`.
    fn wait_for_text(&self, since: u32, aborted: &dyn Fn() -> bool) -> String {
        let deadline = self.env.now().saturating_add(CLIPBOARD_POLL_TIMEOUT_MS);
        let settle_deadline = deadline.saturating_add(CLIPBOARD_TEXT_SETTLE_MS);
        loop {
            if aborted() {
                return String::new();
            }
            let moved = self.env.seq() != since;
            if moved {
                let text = self.env.text();
                if !text.is_empty() {
                    return text;
                }
            }
            let now = self.env.now();
            if now >= deadline {
                let settling = moved && now < settle_deadline && self.env.has_text_format();
                if !settling {
                    return String::new();
                }
            }
            self.env.sleep(CLIPBOARD_POLL_MS);
        }
    }

    /// selection.md §8.7: what happens to the user's clipboard; returns the loss notice.
    fn settle_clipboard(&mut self, ctx: &CaptureCtx, raw: &str, secret: bool) -> Option<String> {
        // The context chip's retry sent a second chord: a slow app may still answer it after the restore.
        let echo_of = (ctx.chords > 1 && !is_blank(raw)).then(|| raw.to_string());
        if secret {
            // Never leave a secret on the clipboard on our account, whatever restoreClipboard says.
            self.schedule_restore(ctx.snapshot.clone(), 0, None, true, echo_of);
            return None;
        }
        if !is_blank(raw) {
            if !self.shared.restore_clipboard.load(Ordering::SeqCst) {
                return None;
            }
            let seq = self.env.seq();
            self.schedule_restore(
                ctx.snapshot.clone(),
                CLIPBOARD_RESTORE_DELAY_MS,
                Some((raw.to_string(), seq)),
                false,
                echo_of,
            );
            return loss_notice(&ctx.snapshot);
        }
        if self.aborted() && ctx.chords > 0 {
            self.await_in_flight_copy(ctx);
        }
        if self.env.seq() != ctx.since {
            // Replaced by something that is not text (files, an image): put it back now.
            self.schedule_restore(ctx.snapshot.clone(), 0, None, false, None);
            return loss_notice(&ctx.snapshot);
        }
        if !self.aborted() {
            self.watch_late_copy(ctx.clone());
        }
        None
    }

    /// Quitting mid-capture: give the copy the rest of its normal poll window so the restore can undo it.
    fn await_in_flight_copy(&self, ctx: &CaptureCtx) {
        let until = ctx.last_chord_at.saturating_add(CLIPBOARD_POLL_TIMEOUT_MS);
        while self.env.seq() == ctx.since && self.env.now() < until {
            self.env.sleep(CLIPBOARD_POLL_MS);
        }
    }

    fn set_pending(&mut self, job: Job, due: u64) {
        // Only one job at a time: an older one gets its decision now (and so does anything it queued, bounded).
        for _ in 0..FLUSH_ROUNDS {
            let Some(old) = self.pending.take() else { break };
            self.finish(old.job);
        }
        self.pending = Some(Pending { job, due });
    }

    fn schedule_restore(
        &mut self,
        snapshot: Arc<ClipboardSnapshot>,
        delay: u64,
        if_still: Option<(String, u32)>,
        secret: bool,
        echo_of: Option<String>,
    ) {
        let due = self.env.now().saturating_add(delay);
        self.set_pending(Job::Restore { snapshot, if_still, secret, echo_of }, due);
    }

    fn watch_late_copy(&mut self, ctx: CaptureCtx) {
        let now = self.env.now();
        self.set_pending(Job::LateCopy { ctx, deadline: now.saturating_add(LATE_COPY_GRACE_MS) }, now + LATE_POLL_MS);
    }

    /// After a restore, a copy of the very same text is the answer to our second chord: undo it once.
    fn guard_echo(&mut self, snapshot: Arc<ClipboardSnapshot>, text: String, secret: bool) {
        let from = self.env.seq();
        if from == 0 {
            return;
        }
        let now = self.env.now();
        self.set_pending(
            Job::Echo { snapshot, text, secret, from, deadline: now.saturating_add(LATE_COPY_GRACE_MS) },
            now + LATE_POLL_MS,
        );
    }

    fn put_back(&self, snapshot: &Arc<ClipboardSnapshot>, clear_if_unrestorable: bool) {
        if snapshot.failed {
            if clear_if_unrestorable {
                self.env.clear();
            }
            return;
        }
        self.env.restore(snapshot);
    }

    /// A job's decision, run when it is due (restores) or early (flush, a newer job).
    fn finish(&mut self, job: Job) {
        match job {
            Job::Restore { snapshot, if_still, secret, echo_of } => {
                if let Some((text, seq)) = if_still {
                    let untouched = seq != 0 && self.env.seq() == seq;
                    if !untouched && self.env.text() != text {
                        // The user copied something else meanwhile.
                        return;
                    }
                }
                self.put_back(&snapshot, secret);
                if let Some(echo) = echo_of {
                    self.guard_echo(snapshot, echo, secret);
                }
            }
            // Ended early (next capture, quitting): a late copy that already landed is only undone, not read.
            Job::LateCopy { ctx, .. } => {
                if self.env.seq() != ctx.since {
                    self.put_back(&ctx.snapshot, false);
                }
            }
            Job::Echo { snapshot, text, secret, from, .. } => self.undo_echo(&snapshot, &text, secret, from),
        }
    }

    fn undo_echo(&self, snapshot: &Arc<ClipboardSnapshot>, text: &str, secret: bool, from: u32) {
        // A different text is the user's own copy: leave it.
        if self.env.seq() != from && self.env.text() == text {
            self.put_back(snapshot, secret);
        }
    }

    /// `finishLateCopy` (selection.md §8.8): the sequence moved during the watch.
    fn finish_late_copy(&mut self, ctx: CaptureCtx) {
        let shared = self.shared.clone();
        let superseded = || shared.busy.load(Ordering::SeqCst) || shared.stopped.load(Ordering::SeqCst);
        let raw = self.wait_for_text(ctx.since, &superseded);
        if is_blank(&raw) || superseded() {
            self.put_back(&ctx.snapshot, false);
            return;
        }
        let echo_of = (ctx.chords > 1).then(|| raw.clone());
        if let Some(p) = self.env.privacy() {
            self.schedule_restore(ctx.snapshot.clone(), 0, None, true, echo_of);
            self.shared.notice(refusal(p));
            return;
        }
        let seq = self.env.seq();
        tracing::debug!(target: "selection", "late copy arrived, reading it");
        if !self.shared.stopped.load(Ordering::SeqCst) {
            let text = js_trim(&raw).to_string();
            self.shared.emit(SelectionEvent::Read { text, source: ctx.source, foreground: ctx.foreground.clone() });
        }
        if !self.shared.restore_clipboard.load(Ordering::SeqCst) {
            return;
        }
        self.schedule_restore(ctx.snapshot.clone(), CLIPBOARD_RESTORE_DELAY_MS, Some((raw, seq)), false, echo_of);
        if let Some(n) = loss_notice(&ctx.snapshot) {
            self.shared.notice(n);
        }
    }

    /// `flushRestore()` (selection.md §8.8).
    fn flush_restore(&mut self) {
        drain_bounded(self, |w| w.pending.take().map(|p| p.job), |w, job| w.finish(job));
    }

    /// When the pending job wants the worker next (ms on the env clock).
    fn next_due(&self) -> Option<u64> {
        self.pending.as_ref().map(|p| p.due)
    }

    /// Runs the pending job if it is due: restores decide; watches poll the sequence every 25 ms.
    fn run_due(&mut self) {
        for _ in 0..8 {
            let now = self.env.now();
            let due = self.pending.as_ref().is_some_and(|p| p.due <= now);
            if !due {
                return;
            }
            let Some(Pending { job, .. }) = self.pending.take() else { return };
            match job {
                Job::Restore { .. } => self.finish(job),
                Job::LateCopy { ctx, deadline } => {
                    if self.env.seq() == ctx.since {
                        if now < deadline {
                            self.pending =
                                Some(Pending { job: Job::LateCopy { ctx, deadline }, due: now + LATE_POLL_MS });
                        }
                        // else: nothing moved in the grace window; expire silently.
                    } else {
                        self.finish_late_copy(ctx);
                    }
                }
                Job::Echo { snapshot, text, secret, from, deadline } => {
                    if self.env.seq() == from {
                        if now < deadline {
                            self.pending = Some(Pending {
                                job: Job::Echo { snapshot, text, secret, from, deadline },
                                due: now + LATE_POLL_MS,
                            });
                        }
                    } else {
                        self.undo_echo(&snapshot, &text, secret, from);
                    }
                }
            }
        }
    }
}

fn worker_thread(shared: Arc<Shared>, rx: Receiver<WorkerMsg>) {
    let mut w = Worker::new(Win32Capture { epoch: Instant::now() }, shared);
    loop {
        let msg = match w.next_due() {
            Some(due) => {
                let wait = Duration::from_millis(due.saturating_sub(w.env.now()));
                match rx.recv_timeout(wait) {
                    Ok(m) => Some(m),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }
            None => match rx.recv() {
                Ok(m) => Some(m),
                Err(_) => break,
            },
        };
        let result = catch_unwind(AssertUnwindSafe(|| {
            if let Some(m) = msg {
                w.handle(m);
            }
            w.run_due();
        }));
        if result.is_err() {
            tracing::warn!(target: "selection", "clipboard worker job panicked");
            w.shared.busy.store(false, Ordering::SeqCst);
        }
    }
    // The service is gone: put back whatever is still pending.
    let _ = catch_unwind(AssertUnwindSafe(|| w.flush_restore()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;

    // ───────────── helpers ─────────────

    fn win(hwnd: isize, pid: u32, exe: &str, class: &str) -> WindowInfo {
        WindowInfo {
            hwnd,
            info: ForegroundInfo {
                process_name: exe.into(),
                app_name: crate::foreground::friendly_app_name(exe),
                title: format!("{exe} window"),
                pid,
            },
            class_name: class.into(),
        }
    }

    type Log = Arc<Mutex<Vec<SelectionEvent>>>;

    fn shared() -> (Arc<Shared>, Log) {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let sink = log.clone();
        (Arc::new(Shared::new(Arc::new(move |ev| sink.lock().push(ev)))), log)
    }

    fn info(msg: String) -> SelectionEvent {
        SelectionEvent::Notice { kind: NoticeKind::Info, message: msg }
    }

    // ───────────── pure helpers ─────────────

    #[test]
    fn normalize_exe_forces_a_lowercase_exe_basename() {
        assert_eq!(normalize_exe(r"C:\Tools\Foo"), Some("foo.exe".into()));
        assert_eq!(normalize_exe("  Notepad.EXE "), Some("notepad.exe".into()));
        assert_eq!(normalize_exe("a/b/c.exe"), Some("c.exe".into()));
        assert_eq!(normalize_exe("soffice.bin"), Some("soffice.bin.exe".into()));
        assert_eq!(normalize_exe(""), None);
        assert_eq!(normalize_exe(r"C:\dir\"), None);
    }

    #[test]
    fn classify_blocks_vox_excluded_apps_and_password_managers() {
        let excluded = vec!["notepad.exe".to_string()];
        let own = 4242;
        let t = |w: &WindowInfo| classify(Some(w), own, &excluded);
        assert_eq!(t(&win(10, 1, "chrome.exe", "")), Some(GestureTarget { id: "10".into(), blocked: false }));
        assert!(t(&win(11, own, "vox.exe", "")).unwrap().blocked, "our own pid");
        assert!(t(&win(12, own, "", "")).unwrap().blocked, "'' is blocked only when it is us");
        assert!(!t(&win(13, 2, "", "")).unwrap().blocked, "an unknown process is never blocked");
        assert!(t(&win(14, 3, "notepad.exe", "")).unwrap().blocked, "excluded");
        assert!(t(&win(15, 4, "keepassxc.exe", "")).unwrap().blocked, "password manager");
        assert_eq!(classify(None, own, &excluded), None);
    }

    #[test]
    fn a_failed_hook_is_retried_only_when_the_chip_triggers_change() {
        assert!(chip_flags_changed(None, (true, false)));
        assert!(!chip_flags_changed(Some((true, false)), (true, false)));
        assert!(chip_flags_changed(Some((true, false)), (true, true)));
        assert!(chip_flags_changed(Some((false, false)), (true, false)));
        assert!(chip_flags_changed(Some((true, false)), (false, false)));
    }

    #[test]
    fn loss_notice_has_four_combinations() {
        let s = |failed, incomplete| ClipboardSnapshot { formats: Vec::new(), incomplete, failed };
        assert_eq!(loss_notice(&s(true, true)).as_deref(), Some(copy::CLIPBOARD_TOO_LARGE));
        assert_eq!(loss_notice(&s(true, false)).as_deref(), Some(copy::CLIPBOARD_UNREADABLE));
        assert_eq!(loss_notice(&s(false, true)).as_deref(), Some(copy::CLIPBOARD_TOO_LARGE));
        assert_eq!(loss_notice(&s(false, false)), None);
    }

    #[test]
    fn refusals_pick_the_secret_or_private_copy() {
        assert_eq!(refusal(Privacy::Secret), copy::secret_refused());
        assert_eq!(refusal(Privacy::Private), copy::private_refused());
    }

    #[test]
    fn flush_restore_stops_after_four_rounds() {
        struct Chain {
            pending: Option<u32>,
            ran: Vec<u32>,
        }
        let mut c = Chain { pending: Some(0), ran: Vec::new() };
        let n = drain_bounded(
            &mut c,
            |c| c.pending.take(),
            |c, j| {
                c.ran.push(j);
                c.pending = Some(j + 1); // every job queues another
            },
        );
        assert_eq!(n, 4);
        assert_eq!(c.ran, vec![0, 1, 2, 3]);
        assert_eq!(c.pending, Some(4), "the fifth job is left for later");
        // Stops early when nothing new is pending.
        let mut c = Chain { pending: Some(7), ran: Vec::new() };
        assert_eq!(drain_bounded(&mut c, |c| c.pending.take(), |c, j| c.ran.push(j)), 1);
        let mut c = Chain { pending: None, ran: Vec::new() };
        assert_eq!(drain_bounded(&mut c, |c| c.pending.take(), |c, j| c.ran.push(j)), 0);
    }

    #[test]
    fn a_panicking_listener_never_propagates() {
        let s = Shared::new(Arc::new(|_| panic!("listener bug")));
        s.emit(SelectionEvent::ChipHide);
        s.notice("x".into());
    }

    #[test]
    fn mouse_messages_map_to_buttons() {
        assert_eq!(mouse_kind(WM_LBUTTONDOWN, 0), Some(MouseKind::Down(MouseButton::Left)));
        assert_eq!(mouse_kind(WM_RBUTTONUP, 0), Some(MouseKind::Up(MouseButton::Right)));
        assert_eq!(mouse_kind(WM_MBUTTONDOWN, 0), Some(MouseKind::Down(MouseButton::Middle)));
        assert_eq!(mouse_kind(WM_XBUTTONDOWN, 1 << 16), Some(MouseKind::Down(MouseButton::X1)));
        assert_eq!(mouse_kind(WM_XBUTTONUP, 2 << 16), Some(MouseKind::Up(MouseButton::X2)));
        assert_eq!(mouse_kind(WM_XBUTTONDOWN, 3 << 16), None);
        assert_eq!(mouse_kind(WM_MOUSEWHEEL, 0xFF88_0000), Some(MouseKind::Wheel));
        assert_eq!(mouse_kind(WM_MOUSEHWHEEL, 120 << 16), Some(MouseKind::Wheel));
        assert_eq!(mouse_kind(WM_MOUSEMOVE, 0), Some(MouseKind::Move));
        assert_eq!(mouse_kind(0x0001, 0), None);
    }

    // ───────────── modifier flags (fake key-state provider) ─────────────

    #[test]
    fn modifier_keydowns_carry_their_own_flag_and_generic_vks_never_count() {
        let mut m = ModifierFlags::default();
        assert_eq!(m.key_down(0xA0, false), Mods { shift: true, ..Mods::default() });
        assert_eq!(m.key_down(0xA3, true), Mods { shift: true, ctrl: true, ..Mods::default() });
        m.key_up(0xA0);
        assert_eq!(m.mods(), Mods { ctrl: true, ..Mods::default() });
        let mut g = ModifierFlags::default();
        for vk in [0x10, 0x11, 0x12] {
            assert_eq!(g.key_down(vk, false), Mods::default(), "{vk:#x}");
        }
        assert_eq!(g.key_down(0x5C, true), Mods { meta: true, ..Mods::default() });
        assert_eq!(g.key_down(0xA5, true), Mods { alt: true, meta: true, ..Mods::default() });
    }

    #[test]
    fn ctrl_alt_del_zeroes_the_mask_after_the_event() {
        let mut m = ModifierFlags::default();
        m.key_down(0xA2, false);
        m.key_down(0xA4, false);
        // The non-extended (numpad) Delete does not zero it.
        assert_eq!(m.key_down(VK_DELETE, false), Mods { ctrl: true, alt: true, ..Mods::default() });
        assert_eq!(m.mods(), Mods { ctrl: true, alt: true, ..Mods::default() });
        // The extended Delete carries the flags, then the mask is gone.
        assert_eq!(m.key_down(VK_DELETE, true), Mods { ctrl: true, alt: true, ..Mods::default() });
        assert_eq!(m.mods(), Mods::default());
        // Ctrl+Delete alone keeps the mask.
        let mut m = ModifierFlags::default();
        m.key_down(0xA2, false);
        m.key_down(VK_DELETE, true);
        assert_eq!(m.mods(), Mods { ctrl: true, ..Mods::default() });
    }

    #[test]
    fn reseeding_reads_the_side_specific_key_state() {
        let mut m = ModifierFlags::default();
        m.key_down(0xA0, false); // the key-up gets lost
        m.reseed(&|vk| vk == 0xA4 || vk == 0x11);
        assert_eq!(m.mods(), Mods { alt: true, ..Mods::default() }, "stale Shift gone, generic Ctrl ignored");
    }

    // ───────────── click counter (GetDoubleClickTime injected) ─────────────

    #[test]
    fn click_counter_follows_libuiohook() {
        let l = MouseButton::Left;
        let mut c = ClickCounter::default();
        assert_eq!(c.press(l, 0, 0, 1000, 500), 1);
        assert_eq!(c.release(1050, 500), 1);
        assert_eq!(c.press(l, 50, 50, 1500, 500), 2, "position is ignored; 500 ms inclusive");
        assert_eq!(c.press(l, 0, 0, 1700, 500), 3);
        assert_eq!(c.press(l, 0, 0, 2201, 500), 1, "too late");
        assert_eq!(c.press(MouseButton::Right, 0, 0, 2300, 500), 1, "another button restarts");
        assert_eq!(c.press(l, 0, 0, 2400, 500), 1);
        // The wheel resets the count and the remembered button, even within the double-click time.
        assert_eq!(c.wheel(), 1);
        assert_eq!(c.press(l, 0, 0, 2500, 500), 1);
        // A release long after its press zeroes the count; so does a real move after the double-click time.
        assert_eq!(c.release(3100, 500), 0);
        let mut c = ClickCounter::default();
        c.press(l, 5, 5, 0, 500);
        assert_eq!(c.moved(5, 5, 900, 500), None, "a move onto the press point is not dispatched");
        assert_eq!(c.moved(6, 5, 400, 500), Some(1));
        assert_eq!(c.moved(7, 5, 900, 500), Some(0));
        // A long system double-click time (900 ms).
        let mut c = ClickCounter::default();
        c.press(l, 0, 0, 0, 900);
        assert_eq!(c.press(l, 0, 0, 800, 900), 2);
    }

    #[test]
    fn activation_gate_dedupes_within_300_ms() {
        let mut g = ActivationGate::default();
        assert!(g.try_pass(0), "starts at minus infinity");
        assert!(!g.try_pass(299));
        assert!(g.try_pass(300));
        assert!(!g.try_pass(301));
    }

    // ───────────── watchdog (D-SEL-9) ─────────────

    /// Probe results for `Watchdog::check`.
    fn visible() -> bool {
        true
    }
    fn hidden() -> bool {
        false
    }

    #[test]
    fn watchdog_reinstalls_after_10_s_of_silence_with_unseen_input() {
        let mut d = Watchdog::new(0, 5, Some(100));
        // Input without hook events → suspicious, but not yet 10 s.
        assert!(!d.check(2000, 5, Some(1900), visible));
        assert!(!d.check(8000, 5, Some(1900), visible));
        assert!(d.check(10_000, 5, Some(1900), visible));
        // Hook events reset everything.
        let mut d = Watchdog::new(0, 5, Some(100));
        assert!(!d.check(2000, 5, Some(1900), visible));
        assert!(!d.check(12_000, 9, Some(11_900), visible));
        assert!(!d.check(14_000, 9, Some(11_900), visible), "no new input: nothing to suspect");
        assert!(!d.check(30_000, 9, None, visible));
    }

    #[test]
    fn watchdog_ignores_input_the_hook_cannot_see_and_backs_off() {
        let mut d = Watchdog::new(0, 5, Some(100));
        assert!(!d.check(2000, 5, Some(1900), hidden), "elevated foreground: not suspicious");
        assert!(!d.check(20_000, 5, Some(1900), hidden));
        // Back-off after a reinstall that saw nothing: 20 s, then 40 s.
        let mut d = Watchdog::new(0, 5, Some(100));
        assert!(d.check(10_000, 5, Some(9000), visible));
        d.reinstalled(10_000, 0, Some(9000));
        assert!(!d.check(12_000, 0, Some(11_000), visible));
        assert!(!d.check(28_000, 0, Some(11_000), visible));
        assert!(d.check(30_000, 0, Some(11_000), visible));
        d.reinstalled(30_000, 0, Some(11_000));
        assert!(!d.check(60_000, 0, Some(31_000), visible));
        assert!(d.check(70_000, 0, Some(31_000), visible));
        // Life resets the strikes.
        d.reinstalled(70_000, 0, Some(31_000));
        assert!(!d.check(72_000, 3, Some(71_000), visible));
        assert_eq!(d.strikes, 0);
    }

    #[test]
    fn watchdog_does_not_blame_the_hook_for_input_typed_on_the_lock_screen() {
        let mut d = Watchdog::new(0, 5, Some(100));
        // Locked: no foreground window, the PIN is typed.
        assert!(!d.check(30_000, 5, Some(29_000), hidden));
        // Unlocked: the last PIN key landed after the previous check; still not the hook's fault.
        assert!(!d.check(32_000, 5, Some(31_000), visible));
        assert!(!d.check(40_000, 5, Some(31_000), visible));
        // Fresh input in a normal window that the hook did not see: now it is suspicious (and long silent).
        assert!(d.check(42_000, 5, Some(41_000), visible));
    }

    /// Review finding: an idle check on the lock screen used to be recorded as "trusted" without probing, so a PIN
    /// typed and an unlock between two checks looked like input the hook had missed in a normal window.
    #[test]
    fn watchdog_probes_idle_checks_too_and_ignores_a_burst_typed_before_an_unlock() {
        let mut probes = 0;
        let mut d = Watchdog::new(0, 5, Some(100));
        // Win+L at 10 s is seen by the hook; the lock screen is in front at the next check.
        assert!(!d.check(12_000, 6, Some(10_000), || {
            probes += 1;
            false
        }));
        assert!(!d.last_trusted, "the probe result is kept even when the hook showed life");
        // Idle on the lock screen: the probe still runs (and says "not visible").
        assert!(!d.check(42_000, 6, Some(10_000), || {
            probes += 1;
            false
        }));
        assert_eq!(probes, 2);
        // The PIN is typed and the session unlocked between 42 s and 44 s: not the hook's fault.
        assert!(!d.check(44_000, 6, Some(43_900), visible));
        assert!(!d.suspect);
        assert!(!d.check(46_000, 6, Some(43_900), visible));
        // Fresh unseen input in a normal window afterwards is suspicious (and the hook has been silent for long).
        assert!(d.check(48_000, 6, Some(47_000), visible));
        // Alt+Tab out of an elevated window: the check that saw the elevated window in front was idle.
        let mut d = Watchdog::new(0, 5, Some(100));
        assert!(!d.check(30_000, 5, Some(100), hidden));
        assert!(!d.check(32_000, 5, Some(31_500), visible));
        assert!(!d.suspect);
    }

    // ───────────── consumer core over a fake environment ─────────────

    struct FakeDesk {
        windows: RefCell<Vec<(i32, Option<WindowInfo>)>>,
        foreground: RefCell<Option<WindowInfo>>,
        overlay: Cell<Option<bool>>,
        text: Cell<Option<bool>>,
        fullscreen: Cell<bool>,
        held: RefCell<Vec<u32>>,
        fg_hwnd: Cell<isize>,
        dbl: Cell<u64>,
    }

    const OWN_PID: u32 = 4242;

    impl FakeDesk {
        fn new() -> Self {
            FakeDesk {
                windows: RefCell::new(vec![
                    (0, Some(win(100, 7, "chrome.exe", "Chrome_WidgetWin_1"))),
                    (5000, Some(win(200, 8, "notepad.exe", "Notepad"))),
                ]),
                foreground: RefCell::new(Some(win(100, 7, "chrome.exe", "Chrome_WidgetWin_1"))),
                overlay: Cell::new(Some(false)),
                text: Cell::new(Some(true)),
                fullscreen: Cell::new(false),
                held: RefCell::new(Vec::new()),
                fg_hwnd: Cell::new(100),
                dbl: Cell::new(500),
            }
        }
    }

    impl ConsumerEnv for FakeDesk {
        fn window_info_at(&self, x: i32, _y: i32) -> Option<WindowInfo> {
            self.windows.borrow().iter().rev().find(|(from, _)| x >= *from).and_then(|(_, w)| w.clone())
        }
        fn foreground_window_info(&self) -> Option<WindowInfo> {
            self.foreground.borrow().clone()
        }
        fn own_overlay_at(&self, _x: i32, _y: i32) -> Option<bool> {
            self.overlay.get()
        }
        fn text_cursor(&self) -> Option<bool> {
            self.text.get()
        }
        fn fullscreen(&self) -> bool {
            self.fullscreen.get()
        }
        fn key_down(&self, vk: u32) -> bool {
            self.held.borrow().contains(&vk)
        }
        fn foreground_hwnd(&self) -> isize {
            self.fg_hwnd.get()
        }
        fn double_click_ms(&self) -> u64 {
            self.dbl.get()
        }
        fn own_pid(&self) -> u32 {
            OWN_PID
        }
    }

    struct Rig {
        core: Core<FakeDesk>,
        bounds: Option<PhysRect>,
        busy: bool,
    }

    fn cfg(context: bool, selection: bool) -> SelectionConfig {
        SelectionConfig {
            right_click_chip: context,
            selection_popup: selection,
            excluded_apps: Vec::new(),
            restore_clipboard: true,
        }
    }

    impl Rig {
        fn new(context: bool, selection: bool) -> Self {
            let mut core = Core::new(FakeDesk::new());
            let mut out = Vec::new();
            core.configure(&cfg(context, selection), 0, &mut out);
            assert!(out.is_empty());
            Rig { core, bounds: None, busy: false }
        }
        fn raw(&mut self, raw: RawInput, t: u64) -> Vec<Output> {
            let mut out = Vec::new();
            self.core.on_raw(raw, t, self.busy, self.bounds, &mut out);
            out
        }
        fn mouse(&mut self, msg: u32, x: i32, y: i32, t: u64) -> Vec<Output> {
            self.raw(RawInput::Mouse { msg, x, y, mouse_data: 0, time: 0 }, t)
        }
        fn key(&mut self, down: bool, vk: u32, extended: bool, t: u64) -> Vec<Output> {
            self.raw(RawInput::Key { down, vk, scan: 0, extended, injected: false, time: 0 }, t)
        }
        fn drag(&mut self, x: i32, y: i32, dx: i32, t: u64) -> Vec<Output> {
            let mut out = self.mouse(WM_LBUTTONDOWN, x, y, t);
            out.extend(self.mouse(WM_MOUSEMOVE, x + dx, y, t + 20));
            out.extend(self.mouse(WM_LBUTTONUP, x + dx, y, t + 40));
            out
        }
        fn click(&mut self, x: i32, y: i32, t: u64) -> Vec<Output> {
            let mut out = self.mouse(WM_LBUTTONDOWN, x, y, t);
            out.extend(self.mouse(WM_LBUTTONUP, x, y, t + 20));
            out
        }
        fn right(&mut self, x: i32, y: i32, t: u64) -> Vec<Output> {
            let mut out = self.mouse(WM_RBUTTONDOWN, x, y, t);
            out.extend(self.mouse(WM_RBUTTONUP, x, y, t + 20));
            out
        }
    }

    fn show(x: i32, y: i32, mode: ChipMode) -> Output {
        Output::Event(SelectionEvent::ChipShow { x, y, mode })
    }
    const HIDE: Output = Output::Event(SelectionEvent::ChipHide);

    #[test]
    fn chip_events_carry_physical_points_and_hides() {
        let mut r = Rig::new(true, false);
        assert!(r.drag(100, 100, 50, 0).is_empty());
        assert_eq!(r.right(-1200, 1500, 1000), vec![show(-1200, 1500, ChipMode::Context)]);
        assert_eq!(r.key(true, 0x1B, false, 1100), vec![HIDE]);
        // Turning the right-click chip off while it shows hides it.
        r.right(-1200, 1500, 1200);
        let mut out = Vec::new();
        r.core.configure(&cfg(false, false), 1300, &mut out);
        assert_eq!(out, vec![HIDE]);
    }

    #[test]
    fn hook_side_activation_and_the_chip_ui_are_deduped_within_300_ms() {
        let mut r = Rig::new(true, false);
        r.drag(100, 100, 50, 0);
        r.right(150, 100, 1000);
        r.bounds = Some(PhysRect { x: 160, y: 80, width: 150, height: 38 });
        r.core.env.overlay.set(Some(true));
        // The hook sees the press inside the chip: hide + capture (context chip, context-menu path).
        assert_eq!(r.mouse(WM_LBUTTONDOWN, 200, 90, 2000), vec![HIDE, Output::Capture(ReadSource::ContextChip)]);
        r.bounds = None;
        // The chip UI's own press arrives 120 ms later: deduped.
        let mut out = Vec::new();
        r.core.chip_activated(2120, &mut out);
        assert!(out.is_empty());
        // 300 ms later the gate is open again; with no machine chip the last chip mode is used.
        r.core.chip_activated(2300, &mut out);
        assert_eq!(out, vec![HIDE, Output::Capture(ReadSource::ContextChip)]);
    }

    #[test]
    fn the_chip_ui_alone_activates_a_selection_chip_with_its_mode() {
        let mut r = Rig::new(false, true);
        assert_eq!(r.drag(100, 100, 50, 0), vec![show(150, 100, ChipMode::Selection)]);
        let mut out = Vec::new();
        r.core.chip_activated(500, &mut out);
        assert_eq!(out, vec![HIDE, Output::Capture(ReadSource::SelectionPopup)]);
        // Later activations fall back to the last chip mode (selection).
        let mut out = Vec::new();
        r.core.chip_activated(5000, &mut out);
        assert_eq!(out, vec![HIDE, Output::Capture(ReadSource::SelectionPopup)]);
        // Dismissal always reports a hide.
        let mut out = Vec::new();
        r.core.chip_dismissed(&mut out);
        assert_eq!(out, vec![HIDE]);
    }

    #[test]
    fn chip_hit_test_is_half_open_then_own_overlay_or_unknown() {
        let setup = |overlay: Option<bool>| {
            let mut r = Rig::new(true, false);
            r.drag(100, 100, 50, 0);
            r.right(150, 100, 1000);
            r.bounds = Some(PhysRect { x: 10, y: 10, width: 20, height: 20 });
            r.core.env.overlay.set(overlay);
            r
        };
        let activates = vec![HIDE, Output::Capture(ReadSource::ContextChip)];
        assert_eq!(setup(Some(true)).mouse(WM_LBUTTONDOWN, 29, 29, 2000), activates);
        assert_eq!(setup(Some(true)).mouse(WM_LBUTTONDOWN, 10, 10, 2000), activates);
        assert_eq!(setup(Some(true)).mouse(WM_LBUTTONDOWN, 30, 10, 2000), vec![HIDE], "right edge is outside");
        assert_eq!(setup(Some(true)).mouse(WM_LBUTTONDOWN, 10, 30, 2000), vec![HIDE], "bottom edge is outside");
        // A menu opened later sits above the chip: the window under the cursor is not ours.
        assert_eq!(setup(Some(false)).mouse(WM_LBUTTONDOWN, 15, 15, 2000), vec![HIDE]);
        // Unknown: the bounds decide.
        assert_eq!(setup(None).mouse(WM_LBUTTONDOWN, 15, 15, 2000), activates);
        // No bounds reported (hidden / hiding): never inside.
        let mut r = setup(Some(true));
        r.bounds = None;
        assert_eq!(r.mouse(WM_LBUTTONDOWN, 15, 15, 2000), vec![HIDE]);
    }

    #[test]
    fn a_stuck_shift_is_cleared_by_the_next_press() {
        let mut r = Rig::new(false, true);
        // Shift goes down; its key-up is lost (e.g. released inside an elevated window).
        r.key(true, 0xA0, false, 0);
        // The press reads the real key state (nothing held): a plain click, no shift-click chip.
        assert!(r.click(300, 300, 1000).is_empty());
        // Really held: shift-click → hint → selection chip.
        r.core.env.held.borrow_mut().push(0xA0);
        assert_eq!(r.click(600, 600, 3000), vec![show(600, 600, ChipMode::Selection)]);
    }

    #[test]
    fn the_mask_is_reseeded_when_the_foreground_window_changes() {
        let mut r = Rig::new(true, false);
        r.key(true, 0xA2, false, 0); // Ctrl down, its key-up lost
        r.core.env.fg_hwnd.set(555); // another app came to the front; Ctrl is not really held
                                     // 'A' without Ctrl: not select-all, so no hint and a right-click shows nothing.
        r.key(true, 0x41, false, 100);
        assert!(r.right(100, 100, 200).is_empty());
        // With Ctrl really held, Ctrl+A arms the chip.
        r.key(true, 0xA2, false, 300);
        r.key(true, 0x41, false, 310);
        assert_eq!(r.right(100, 100, 400), vec![show(100, 100, ChipMode::Context)]);
    }

    #[test]
    fn keydowns_are_ignored_while_a_capture_is_busy_but_keyups_still_count() {
        let mut r = Rig::new(true, false);
        r.drag(100, 100, 50, 0);
        r.right(150, 100, 1000);
        r.busy = true;
        assert!(r.key(true, 0x43, false, 1100).is_empty(), "our own Ctrl+C must not hide the chip");
        r.key(true, 0xA2, false, 1110);
        r.key(false, 0xA2, false, 1120);
        r.busy = false;
        assert_eq!(r.key(true, 0x58, false, 1200), vec![HIDE]);
        // Mouse events still flow during a capture (quirk 13).
        r.right(150, 100, 1300);
        r.busy = true;
        assert_eq!(r.mouse(WM_MOUSEWHEEL, 0, 0, 1400), vec![HIDE]);
    }

    #[test]
    fn the_injected_double_click_time_drives_multi_clicks() {
        // 700 ms apart: not a multi-click with the 500 ms default …
        let mut r = Rig::new(false, true);
        r.click(300, 300, 0);
        assert!(r.click(300, 300, 700).is_empty());
        // … but the hook's counter says 2 when the system double-click time is 900 ms.
        let mut r = Rig::new(false, true);
        r.core.env.dbl.set(900);
        r.click(300, 300, 0);
        assert_eq!(r.click(300, 300, 700), vec![show(300, 300, ChipMode::Selection)]);
        // A wheel in between resets the counter.
        let mut r = Rig::new(false, true);
        r.core.env.dbl.set(900);
        r.click(300, 300, 0);
        r.mouse(WM_MOUSEWHEEL, 0, 0, 300);
        assert!(r.click(300, 300, 700).is_empty());
    }

    #[test]
    fn moves_are_fed_only_while_the_machine_wants_them() {
        let mut r = Rig::new(false, true);
        // No press: moves are dropped before the machine.
        assert!(r.mouse(WM_MOUSEMOVE, 50, 50, 0).is_empty());
        assert!(!r.core.machine.wants_mouse_move());
        r.mouse(WM_LBUTTONDOWN, 0, 0, 100);
        assert!(r.core.machine.wants_mouse_move());
        r.mouse(WM_MOUSEMOVE, 20, 0, 120);
        assert!(!r.core.machine.wants_mouse_move());
        assert_eq!(r.mouse(WM_LBUTTONUP, 20, 0, 140), vec![show(20, 0, ChipMode::Selection)]);
    }

    #[test]
    fn excluded_apps_are_normalized_and_blocked() {
        let mut core = Core::new(FakeDesk::new());
        let mut out = Vec::new();
        let mut c = cfg(true, false);
        c.excluded_apps = vec![r"C:\Program Files\Google\Chrome".into()];
        core.configure(&c, 0, &mut out);
        assert_eq!(core.excluded, vec!["chrome.exe".to_string()]);
        let mut r = Rig { core, bounds: None, busy: false };
        r.drag(100, 100, 50, 0);
        assert!(r.right(100, 100, 1000).is_empty(), "chrome is excluded");
    }

    #[test]
    fn reset_hides_the_chip_and_forgets_held_modifiers() {
        let mut r = Rig::new(true, false);
        r.drag(100, 100, 50, 0);
        r.right(150, 100, 1000);
        r.key(true, 0xA0, false, 1100); // hides the chip? no: a modifier changes nothing
        let mut out = Vec::new();
        r.core.reset(1200, &mut out);
        assert_eq!(out, vec![HIDE]);
        assert_eq!(r.core.mods.mods(), Mods::default());
    }

    // ───────────── capture pipeline over a fake CaptureEnv ─────────────

    /// A clipboard write the simulated app (or user) makes.
    #[derive(Debug, Clone)]
    struct Write {
        text: Option<&'static str>,
        listed: bool,
        secret: bool,
        private: bool,
        readable_after: u64,
    }

    fn text(t: &'static str) -> Write {
        Write { text: Some(t), listed: true, secret: false, private: false, readable_after: 0 }
    }
    fn image() -> Write {
        Write { text: None, listed: false, secret: false, private: false, readable_after: 0 }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Trigger {
        Abort,
        RestoreOff,
    }

    #[derive(Debug, Default)]
    struct Clip {
        seq: u32,
        text: String,
        listed: bool,
        readable_at: u64,
        marked: bool,
        private: bool,
    }

    struct St {
        now: u64,
        clip: Clip,
        snapshot: ClipboardSnapshot,
        snap_privacy: Option<Privacy>,
        last_own_write: Option<(u32, Option<Privacy>)>,
        restores: Vec<u64>,
        clears: Vec<u64>,
        chords: Vec<(u64, CopyMethod)>,
        escapes: Vec<u64>,
        reactions: VecDeque<Option<(u64, Write)>>,
        scheduled: Vec<(u64, Write)>,
        triggers: Vec<(u64, Trigger)>,
        held_until: u64,
        released: Vec<Vec<u32>>,
        foreground: Option<WindowInfo>,
        uipi: Option<bool>,
        uipi_calls: u32,
        snapshots: u32,
        chord_ok: bool,
        escape_ok: bool,
        abort_on_privacy: bool,
    }

    struct FakeClip {
        st: RefCell<St>,
        shared: Arc<Shared>,
    }

    fn utf16z(s: &str) -> Vec<u8> {
        s.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect()
    }

    fn snap_text(s: &str) -> ClipboardSnapshot {
        ClipboardSnapshot { formats: vec![(13, utf16z(s))], incomplete: false, failed: false }
    }

    fn snapshot_text(s: &ClipboardSnapshot) -> String {
        s.formats
            .iter()
            .find(|(id, _)| *id == 13)
            .map(|(_, b)| {
                let units: Vec<u16> =
                    b.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).take_while(|&u| u != 0).collect();
                String::from_utf16_lossy(&units)
            })
            .unwrap_or_default()
    }

    impl FakeClip {
        fn new(shared: Arc<Shared>) -> Self {
            FakeClip {
                shared,
                st: RefCell::new(St {
                    now: 0,
                    clip: Clip { seq: 100, text: "old".into(), listed: true, ..Clip::default() },
                    snapshot: snap_text("old"),
                    snap_privacy: None,
                    last_own_write: None,
                    restores: Vec::new(),
                    clears: Vec::new(),
                    chords: Vec::new(),
                    escapes: Vec::new(),
                    reactions: VecDeque::new(),
                    scheduled: Vec::new(),
                    triggers: Vec::new(),
                    held_until: 0,
                    released: Vec::new(),
                    foreground: Some(win(100, 7, "chrome.exe", "Chrome_WidgetWin_1")),
                    uipi: Some(false),
                    uipi_calls: 0,
                    snapshots: 0,
                    chord_ok: true,
                    escape_ok: true,
                    abort_on_privacy: false,
                }),
            }
        }

        fn write_now(&self, w: &Write) {
            let mut st = self.st.borrow_mut();
            let now = st.now;
            let clip = &mut st.clip;
            clip.seq += 1;
            clip.text = w.text.unwrap_or("").to_string();
            clip.listed = w.listed;
            clip.readable_at = now + w.readable_after;
            clip.marked = w.secret;
            clip.private = w.private;
        }

        /// Moves the clock to `t`, applying scheduled writes and triggers in time order.
        fn advance_to(&self, t: u64) {
            loop {
                let next = {
                    let st = self.st.borrow();
                    let w = st.scheduled.iter().map(|(at, _)| *at).filter(|&at| at <= t).min();
                    let g = st.triggers.iter().map(|(at, _)| *at).filter(|&at| at <= t).min();
                    match (w, g) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (a, b) => a.or(b),
                    }
                };
                let Some(at) = next else { break };
                {
                    let mut st = self.st.borrow_mut();
                    st.now = st.now.max(at);
                }
                let due_writes: Vec<Write> = {
                    let mut st = self.st.borrow_mut();
                    let (due, rest): (Vec<_>, Vec<_>) = st.scheduled.drain(..).partition(|(a, _)| *a == at);
                    st.scheduled = rest;
                    due.into_iter().map(|(_, w)| w).collect()
                };
                for w in &due_writes {
                    self.write_now(w);
                }
                let due_triggers: Vec<Trigger> = {
                    let mut st = self.st.borrow_mut();
                    let (due, rest): (Vec<_>, Vec<_>) = st.triggers.drain(..).partition(|(a, _)| *a == at);
                    st.triggers = rest;
                    due.into_iter().map(|(_, g)| g).collect()
                };
                for g in due_triggers {
                    match g {
                        Trigger::Abort => self.shared.abort.store(true, Ordering::SeqCst),
                        Trigger::RestoreOff => self.shared.restore_clipboard.store(false, Ordering::SeqCst),
                    }
                }
            }
            let mut st = self.st.borrow_mut();
            st.now = st.now.max(t);
        }
    }

    impl CaptureEnv for FakeClip {
        fn seq(&self) -> u32 {
            self.st.borrow().clip.seq
        }
        fn text(&self) -> String {
            let st = self.st.borrow();
            if st.now < st.clip.readable_at {
                String::new()
            } else {
                st.clip.text.clone()
            }
        }
        fn has_text_format(&self) -> bool {
            self.st.borrow().clip.listed
        }
        fn privacy(&self) -> Option<Privacy> {
            let st = self.st.borrow();
            if st.abort_on_privacy {
                self.shared.abort.store(true, Ordering::SeqCst);
            }
            // The real module's D-SEL-8 rule, then the marker scan.
            if let Some(original) = crate::clipboard::own_write_privacy(st.last_own_write, st.clip.seq) {
                return original;
            }
            if st.clip.marked {
                Some(Privacy::Secret)
            } else if st.clip.private {
                Some(Privacy::Private)
            } else {
                None
            }
        }
        fn snapshot(&self) -> ClipboardSnapshot {
            let mut st = self.st.borrow_mut();
            st.snapshots += 1;
            // The real module (D-SEL-8): a snapshot of our own restore leaves out the markers that restore added, so
            // it carries the restored data's original privacy, not our "do not record" marker.
            if let Some(original) = crate::clipboard::own_write_privacy(st.last_own_write, st.clip.seq) {
                st.snap_privacy = original;
                return st.snapshot.clone();
            }
            st.snap_privacy = if st.clip.marked {
                Some(Privacy::Secret)
            } else if st.clip.private {
                Some(Privacy::Private)
            } else {
                None
            };
            st.snapshot.clone()
        }
        fn restore(&self, snapshot: &Arc<ClipboardSnapshot>) -> bool {
            let mut st = self.st.borrow_mut();
            let now = st.now;
            st.restores.push(now);
            let original = st.snap_privacy;
            let clip = &mut st.clip;
            clip.seq += 1;
            clip.text = snapshot_text(snapshot);
            clip.listed = !clip.text.is_empty();
            clip.readable_at = 0;
            // Our restores carry the "do not record" markers.
            clip.marked = true;
            clip.private = false;
            let seq = clip.seq;
            st.last_own_write = Some((seq, original));
            true
        }
        fn clear(&self) -> bool {
            let mut st = self.st.borrow_mut();
            let now = st.now;
            st.clears.push(now);
            st.clip = Clip { seq: st.clip.seq + 1, marked: true, ..Clip::default() };
            let seq = st.clip.seq;
            st.last_own_write = Some((seq, None));
            true
        }
        fn send_chord(&self, method: CopyMethod) -> bool {
            let mut st = self.st.borrow_mut();
            let now = st.now;
            st.chords.push((now, method));
            if !st.chord_ok {
                return false;
            }
            if let Some(Some((delay, w))) = st.reactions.pop_front() {
                st.scheduled.push((now + delay, w));
            }
            true
        }
        fn send_escape(&self) -> bool {
            let mut st = self.st.borrow_mut();
            let now = st.now;
            st.escapes.push(now);
            st.escape_ok
        }
        fn held_modifiers(&self) -> Vec<u32> {
            let st = self.st.borrow();
            if st.now < st.held_until {
                vec![0xA2]
            } else {
                Vec::new()
            }
        }
        fn release_keys(&self, vks: &[u32]) {
            self.st.borrow_mut().released.push(vks.to_vec());
        }
        fn foreground(&self) -> Option<WindowInfo> {
            self.st.borrow().foreground.clone()
        }
        fn uipi_blocked(&self, _hwnd: isize, _pid: u32) -> Option<bool> {
            let mut st = self.st.borrow_mut();
            st.uipi_calls += 1;
            st.uipi
        }
        fn now(&self) -> u64 {
            self.st.borrow().now
        }
        fn sleep(&self, ms: u64) {
            let t = self.st.borrow().now + ms;
            self.advance_to(t);
        }
    }

    struct Cap {
        w: Worker<FakeClip>,
        log: Log,
    }

    impl Cap {
        fn new() -> Self {
            let (shared, log) = shared();
            let env = FakeClip::new(shared.clone());
            Cap { w: Worker::new(env, shared), log }
        }
        fn st(&self) -> std::cell::RefMut<'_, St> {
            self.w.env.st.borrow_mut()
        }
        /// `(chords sent, escapes sent)`.
        fn counts(&self) -> (usize, usize) {
            let st = self.st();
            (st.chords.len(), st.escapes.len())
        }
        fn react(&self, reactions: Vec<Option<(u64, Write)>>) {
            self.st().reactions = reactions.into();
        }
        fn capture(&mut self, source: ReadSource) {
            self.w.shared.busy.store(true, Ordering::SeqCst);
            self.w.capture(source, source == ReadSource::ContextChip);
        }
        /// Lets `ms` of virtual time pass, running the worker's due jobs as the real loop would.
        fn run_for(&mut self, ms: u64) {
            let end = self.w.env.now() + ms;
            while let Some(due) = self.w.next_due() {
                if due > end {
                    break;
                }
                self.w.env.advance_to(due);
                self.w.run_due();
            }
            self.w.env.advance_to(end);
        }
        fn events(&self) -> Vec<SelectionEvent> {
            std::mem::take(&mut *self.log.lock())
        }
        fn clip_text(&self) -> String {
            self.st().clip.text.clone()
        }
    }

    fn chrome_fg() -> Option<ForegroundInfo> {
        Some(win(100, 7, "chrome.exe", "Chrome_WidgetWin_1").info)
    }

    fn read(t: &str, source: ReadSource) -> SelectionEvent {
        SelectionEvent::Read { text: t.into(), source, foreground: chrome_fg() }
    }

    #[test]
    fn hotkey_capture_reads_and_restores_the_clipboard_150_ms_later() {
        let mut c = Cap::new();
        c.react(vec![Some((30, text("  Hello world \n")))]);
        c.capture(ReadSource::Hotkey);
        assert_eq!(c.events(), vec![read("Hello world", ReadSource::Hotkey)]);
        {
            let st = c.st();
            assert_eq!(st.chords, vec![(40, CopyMethod::CtrlC { escape_retry: true })], "settle 40 ms, one chord");
            assert!(st.escapes.is_empty());
            assert_eq!(st.now, 70);
            assert_eq!(st.uipi_calls, 1);
        }
        assert!(!c.w.shared.busy.load(Ordering::SeqCst));
        assert!(c.w.shared.reseed_modifiers.load(Ordering::SeqCst), "the consumer re-seeds after every capture");
        c.run_for(149);
        assert!(c.st().restores.is_empty());
        c.run_for(1);
        assert_eq!(c.st().restores, vec![220]);
        assert_eq!(c.clip_text(), "old");
        assert!(c.w.pending.is_none());
    }

    #[test]
    fn the_restore_is_skipped_when_the_user_copied_something_else() {
        let mut c = Cap::new();
        c.react(vec![Some((30, text("Hello")))]);
        c.capture(ReadSource::Hotkey);
        c.w.env.write_now(&text("mine"));
        c.run_for(500);
        assert!(c.st().restores.is_empty());
        assert_eq!(c.clip_text(), "mine");
        // The same text re-written (sequence moved, text equal) is still our copy: restored.
        let mut c = Cap::new();
        c.react(vec![Some((30, text("Hello")))]);
        c.capture(ReadSource::Hotkey);
        c.w.env.write_now(&text("Hello"));
        c.run_for(500);
        assert_eq!(c.st().restores.len(), 1);
    }

    #[test]
    fn restore_clipboard_off_leaves_the_copied_text_and_is_read_live() {
        let mut c = Cap::new();
        c.w.shared.restore_clipboard.store(false, Ordering::SeqCst);
        c.react(vec![Some((30, text("Hello")))]);
        c.capture(ReadSource::Hotkey);
        c.run_for(1000);
        assert!(c.st().restores.is_empty());
        assert_eq!(c.clip_text(), "Hello");
        // Turned off mid-capture (during the poll): that capture already honours it.
        let mut c = Cap::new();
        c.react(vec![Some((100, text("Hello")))]);
        c.st().triggers.push((60, Trigger::RestoreOff));
        c.capture(ReadSource::Hotkey);
        assert_eq!(c.events(), vec![read("Hello", ReadSource::Hotkey)]);
        c.run_for(1000);
        assert!(c.st().restores.is_empty());
    }

    #[test]
    fn a_secret_is_refused_and_restored_at_once_regardless_of_restore_clipboard() {
        let mut c = Cap::new();
        c.w.shared.restore_clipboard.store(false, Ordering::SeqCst);
        c.react(vec![Some((30, Write { secret: true, ..text("hunter2") }))]);
        c.capture(ReadSource::Hotkey);
        assert_eq!(c.events(), vec![info(copy::secret_refused())]);
        c.run_for(0);
        assert_eq!(c.st().restores, vec![70]);
        assert_eq!(c.clip_text(), "old");
        // Private text (an InPrivate window's flag).
        let mut c = Cap::new();
        c.react(vec![Some((30, Write { private: true, ..text("private") }))]);
        c.capture(ReadSource::Hotkey);
        assert_eq!(c.events(), vec![info(copy::private_refused())]);
        c.run_for(0);
        assert_eq!(c.st().restores.len(), 1);
    }

    #[test]
    fn a_secret_with_a_failed_snapshot_clears_the_clipboard_without_a_loss_notice() {
        let mut c = Cap::new();
        c.st().snapshot = ClipboardSnapshot { formats: Vec::new(), incomplete: false, failed: true };
        c.react(vec![Some((30, Write { secret: true, ..text("hunter2") }))]);
        c.capture(ReadSource::Hotkey);
        assert_eq!(c.events(), vec![info(copy::secret_refused())]);
        c.run_for(0);
        assert!(c.st().restores.is_empty());
        assert_eq!(c.st().clears, vec![70]);
        assert_eq!(c.clip_text(), "");
    }

    #[test]
    fn privacy_wins_over_abort() {
        let mut c = Cap::new();
        c.st().abort_on_privacy = true;
        c.react(vec![Some((30, Write { secret: true, ..text("hunter2") }))]);
        c.capture(ReadSource::Hotkey);
        assert_eq!(c.events(), vec![info(copy::secret_refused())]);
        c.run_for(0);
        assert_eq!(c.st().restores.len(), 1);
    }

    #[test]
    fn whitespace_only_text_ends_the_poll() {
        let mut c = Cap::new();
        c.react(vec![Some((30, text(" \n\t ")))]);
        c.capture(ReadSource::Hotkey);
        assert_eq!(c.st().now, 70, "no wait for the 450 ms deadline");
        assert_eq!(c.events(), vec![SelectionEvent::NoSelection(ReadSource::Hotkey)]);
        // The copy replaced the clipboard: put back at once.
        c.run_for(0);
        assert_eq!(c.st().restores, vec![70]);
    }

    #[test]
    fn the_text_settle_extension_needs_a_moved_sequence_and_a_listed_text_format() {
        // Moved, text listed but rendered late (RDP): read at +1000.
        let mut c = Cap::new();
        c.react(vec![Some((30, Write { readable_after: 1000, ..text("Big") }))]);
        c.capture(ReadSource::Hotkey);
        assert_eq!(c.events(), vec![read("Big", ReadSource::Hotkey)]);
        let now = c.st().now;
        assert!((1070..1090).contains(&now), "{now}");
        // Moved but no text format (an image): gives up at the 450 ms deadline.
        let mut c = Cap::new();
        c.react(vec![Some((30, image()))]);
        c.capture(ReadSource::Hotkey);
        let now = c.st().now;
        assert!((490..510).contains(&now), "{now}");
        assert_eq!(c.events(), vec![SelectionEvent::NoSelection(ReadSource::Hotkey)]);
        c.run_for(0);
        assert_eq!(c.st().restores.len(), 1, "the image is replaced by the user's clipboard at once");
        // Not moved (the old text format is still listed): no extension either.
        let mut c = Cap::new();
        c.react(vec![None]);
        c.capture(ReadSource::Hotkey);
        let now = c.st().now;
        assert!((490..510).contains(&now), "{now}");
        // Beyond 450 + 1500 ms the extension ends too.
        let mut c = Cap::new();
        c.react(vec![Some((30, Write { readable_after: 5000, ..text("Huge") }))]);
        c.capture(ReadSource::Hotkey);
        let now = c.st().now;
        assert!((1990..2010).contains(&now), "{now}");
        assert_eq!(c.events(), vec![SelectionEvent::NoSelection(ReadSource::Hotkey)]);
    }

    #[test]
    fn the_escape_retry_is_only_for_context_chip_ctrl_c_captures() {
        // Context chip, Ctrl+C app: nothing on the first chord → Escape → second chord answers.
        let mut c = Cap::new();
        c.react(vec![None, Some((30, text("Retry text")))]);
        c.capture(ReadSource::ContextChip);
        assert_eq!(c.events(), vec![read("Retry text", ReadSource::ContextChip)]);
        {
            let st = c.st();
            assert_eq!(st.chords.len(), 2);
            assert_eq!(st.chords[0].0, 120, "context-menu settle is 120 ms");
            assert_eq!(st.escapes.len(), 1);
            assert_eq!(st.chords[1].0, st.escapes[0] + 80, "80 ms after the Escape");
        }
        // Hotkey: one attempt only.
        let mut c = Cap::new();
        c.react(vec![None, Some((30, text("never")))]);
        c.capture(ReadSource::Hotkey);
        assert_eq!(c.counts(), (1, 0));
        // Selection popup: plain path, one attempt.
        let mut c = Cap::new();
        c.react(vec![None]);
        c.capture(ReadSource::SelectionPopup);
        assert_eq!(c.counts(), (1, 0));
        assert_eq!(c.st().chords[0].0, 40);
        // A terminal (Ctrl+Insert): no Escape (it would reach the shell).
        let mut c = Cap::new();
        c.st().foreground = Some(win(300, 9, "WindowsTerminal.exe", "CASCADIA_HOSTING_WINDOW_CLASS"));
        c.react(vec![None]);
        c.capture(ReadSource::ContextChip);
        assert_eq!(c.st().chords.iter().map(|c| c.1).collect::<Vec<_>>(), vec![CopyMethod::CtrlInsert]);
        assert!(c.st().escapes.is_empty());
        // The first copy arrived without text (sequence moved): no retry.
        let mut c = Cap::new();
        c.react(vec![Some((30, image()))]);
        c.capture(ReadSource::ContextChip);
        assert_eq!(c.counts(), (1, 0));
        // A failed Escape ends the retry silently.
        let mut c = Cap::new();
        c.st().escape_ok = false;
        c.react(vec![None, Some((30, text("x")))]);
        c.capture(ReadSource::ContextChip);
        assert_eq!(c.counts(), (1, 1));
        assert_eq!(c.events(), vec![SelectionEvent::NoSelection(ReadSource::ContextChip)]);
    }

    #[test]
    fn loss_notices_follow_the_snapshot() {
        // Text captured with a partial snapshot: read + "too large".
        let mut c = Cap::new();
        c.st().snapshot = ClipboardSnapshot { formats: vec![(13, utf16z("old"))], incomplete: true, failed: false };
        c.react(vec![Some((30, text("Hello")))]);
        c.capture(ReadSource::Hotkey);
        assert_eq!(c.events(), vec![read("Hello", ReadSource::Hotkey), info(copy::CLIPBOARD_TOO_LARGE.into())]);
        // An image copy over an unreadable clipboard: no selection + "couldn't restore".
        let mut c = Cap::new();
        c.st().snapshot = ClipboardSnapshot { formats: Vec::new(), incomplete: false, failed: true };
        c.react(vec![Some((30, image()))]);
        c.capture(ReadSource::Hotkey);
        assert_eq!(
            c.events(),
            vec![SelectionEvent::NoSelection(ReadSource::Hotkey), info(copy::CLIPBOARD_UNREADABLE.into())]
        );
        c.run_for(0);
        assert!(c.st().restores.is_empty() && c.st().clears.is_empty(), "a failed snapshot is not put back");
        // Text captured with restoreClipboard off: no loss notice.
        let mut c = Cap::new();
        c.w.shared.restore_clipboard.store(false, Ordering::SeqCst);
        c.st().snapshot = ClipboardSnapshot { formats: Vec::new(), incomplete: true, failed: true };
        c.react(vec![Some((30, text("Hello")))]);
        c.capture(ReadSource::Hotkey);
        assert_eq!(c.events(), vec![read("Hello", ReadSource::Hotkey)]);
    }

    #[test]
    fn a_late_copy_emits_read_with_the_original_source() {
        let mut c = Cap::new();
        c.react(vec![None, Some((600, text("Late text")))]);
        c.capture(ReadSource::ContextChip);
        // Both attempts came up empty; the second chord's answer is on its way.
        assert_eq!(c.events(), vec![SelectionEvent::NoSelection(ReadSource::ContextChip)]);
        assert!(matches!(c.w.pending, Some(Pending { job: Job::LateCopy { .. }, .. })));
        c.run_for(2000);
        assert_eq!(c.events(), vec![read("Late text", ReadSource::ContextChip)]);
        // Restored 150 ms after the late read, then guarded against an echo (two chords were sent).
        assert_eq!(c.st().restores.len(), 1);
        assert_eq!(c.clip_text(), "old");
    }

    #[test]
    fn a_late_copy_that_never_comes_expires_silently() {
        let mut c = Cap::new();
        c.react(vec![None]);
        c.capture(ReadSource::Hotkey);
        assert_eq!(c.events(), vec![SelectionEvent::NoSelection(ReadSource::Hotkey)]);
        c.run_for(1600);
        assert!(c.w.pending.is_none());
        assert!(c.st().restores.is_empty());
        assert!(c.events().is_empty());
    }

    #[test]
    fn a_late_secret_is_refused_and_restored() {
        let mut c = Cap::new();
        c.react(vec![Some((700, Write { secret: true, ..text("hunter2") }))]);
        c.capture(ReadSource::Hotkey);
        c.events();
        c.run_for(2000);
        assert_eq!(c.events(), vec![info(copy::secret_refused())]);
        assert_eq!(c.st().restores.len(), 1);
    }

    #[test]
    fn the_next_capture_flushes_a_late_copy_by_undoing_it() {
        let mut c = Cap::new();
        c.react(vec![None]);
        c.capture(ReadSource::Hotkey);
        c.events();
        // The late copy lands, but the next capture starts before the watch saw it: undone, not read.
        c.w.env.write_now(&text("Late"));
        c.react(vec![Some((30, text("Second")))]);
        c.capture(ReadSource::Hotkey);
        assert_eq!(c.st().restores.len(), 1, "flushRestore put the old snapshot back first");
        assert_eq!(c.events(), vec![read("Second", ReadSource::Hotkey)]);
    }

    #[test]
    fn a_late_watch_superseded_by_a_queued_capture_only_puts_back() {
        let mut c = Cap::new();
        c.react(vec![None]);
        c.capture(ReadSource::Hotkey);
        c.events();
        c.w.env.write_now(&text("Late"));
        // A capture got queued (busy) while the watch notices the move.
        c.w.shared.busy.store(true, Ordering::SeqCst);
        c.run_for(100);
        assert!(c.events().is_empty(), "superseded: not read");
        assert_eq!(c.st().restores.len(), 1);
    }

    #[test]
    fn the_echo_guard_undoes_a_second_answer_but_not_the_users_own_copy() {
        let mut c = Cap::new();
        c.react(vec![None, Some((30, text("Echo me")))]);
        c.capture(ReadSource::ContextChip);
        c.events();
        c.run_for(150);
        assert_eq!(c.st().restores.len(), 1);
        assert!(matches!(c.w.pending, Some(Pending { job: Job::Echo { .. }, .. })));
        // The app answers again with the same text after the restore: undone.
        c.w.env.write_now(&text("Echo me"));
        c.run_for(100);
        assert_eq!(c.st().restores.len(), 2);
        assert_eq!(c.clip_text(), "old");
        assert!(c.w.pending.is_none());
        // A different text is the user's own copy: left alone.
        let mut c = Cap::new();
        c.react(vec![None, Some((30, text("Echo me")))]);
        c.capture(ReadSource::ContextChip);
        c.run_for(150);
        c.w.env.write_now(&text("my own copy"));
        c.run_for(100);
        assert_eq!(c.st().restores.len(), 1);
        assert_eq!(c.clip_text(), "my own copy");
        // No second answer within 1.5 s: the guard expires silently.
        let mut c = Cap::new();
        c.react(vec![None, Some((30, text("Echo me")))]);
        c.capture(ReadSource::ContextChip);
        c.run_for(2000);
        assert!(c.w.pending.is_none());
        assert_eq!(c.st().restores.len(), 1);
    }

    #[test]
    fn a_single_chord_capture_arms_no_echo_guard() {
        let mut c = Cap::new();
        c.react(vec![Some((30, text("Hello")))]);
        c.capture(ReadSource::ContextChip);
        c.run_for(150);
        assert_eq!(c.st().restores.len(), 1);
        assert!(c.w.pending.is_none());
    }

    #[test]
    fn flush_restore_runs_the_pending_restore_now() {
        let mut c = Cap::new();
        c.react(vec![Some((30, text("Hello")))]);
        c.capture(ReadSource::Hotkey);
        c.w.flush_restore();
        assert_eq!(c.st().restores, vec![70]);
        assert!(c.w.pending.is_none());
        // `Flush` replies after flushing.
        let (tx, rx) = crossbeam_channel::bounded(1);
        c.w.handle(WorkerMsg::Flush(tx));
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn quitting_mid_capture_waits_for_the_copy_in_flight_and_undoes_it() {
        let mut c = Cap::new();
        c.react(vec![Some((300, text("in flight")))]);
        c.st().triggers.push((100, Trigger::Abort));
        c.w.shared.stopped.store(false, Ordering::SeqCst);
        c.capture(ReadSource::Hotkey);
        assert!(c.events().is_empty(), "aborted: nothing emitted");
        assert_eq!(c.st().now, 340, "waited for the copy (chord at 40 + 300)");
        c.w.flush_restore();
        assert_eq!(c.st().restores.len(), 1);
        assert_eq!(c.clip_text(), "old");
        // Aborted with nothing in flight: waits out the poll window, then neither restores nor watches.
        let mut c = Cap::new();
        c.react(vec![None]);
        c.st().triggers.push((100, Trigger::Abort));
        c.capture(ReadSource::Hotkey);
        assert_eq!(c.st().now, 490);
        assert!(c.w.pending.is_none());
        assert!(c.events().is_empty());
    }

    #[test]
    fn an_abort_before_the_snapshot_sends_nothing() {
        let mut c = Cap::new();
        c.st().triggers.push((20, Trigger::Abort));
        c.capture(ReadSource::Hotkey);
        assert!(c.events().is_empty());
        assert_eq!(
            {
                let st = c.st();
                (st.snapshots, st.chords.len())
            },
            (0, 0)
        );
        assert!(!c.w.shared.abort.load(Ordering::SeqCst), "reset after the capture");
    }

    #[test]
    fn stopped_captures_emit_nothing_and_send_nothing() {
        let mut c = Cap::new();
        c.w.shared.stopped.store(true, Ordering::SeqCst);
        c.react(vec![Some((30, text("Hello")))]);
        c.capture(ReadSource::Hotkey);
        assert!(c.events().is_empty());
        assert!(c.st().chords.is_empty());
        assert!(!c.w.shared.busy.load(Ordering::SeqCst));
    }

    #[test]
    fn copy_on_select_sends_no_keys_and_skips_the_uipi_probe() {
        let mut c = Cap::new();
        c.st().foreground = Some(win(400, 11, "putty.exe", "PuTTY"));
        c.st().uipi = Some(true); // an elevated PuTTY
        c.st().held_until = 10_000; // and modifiers held: no wait on this path
        c.w.env.write_now(&text("  ls -la  "));
        c.capture(ReadSource::Hotkey);
        let putty = Some(win(400, 11, "putty.exe", "PuTTY").info);
        assert_eq!(
            c.events(),
            vec![SelectionEvent::Read { text: "ls -la".into(), source: ReadSource::Hotkey, foreground: putty }]
        );
        let st = c.st();
        assert_eq!((st.chords.len(), st.escapes.len(), st.uipi_calls, st.snapshots), (0, 0, 0, 0));
        assert!(st.released.is_empty());
        assert_eq!(st.now, 0, "no settle, no wait");
    }

    #[test]
    fn copy_on_select_empty_or_secret() {
        let mut c = Cap::new();
        c.st().foreground = Some(win(400, 11, "mobaxterm.exe", ""));
        c.w.env.write_now(&image());
        c.capture(ReadSource::ContextChip);
        assert_eq!(c.events(), vec![SelectionEvent::NoSelection(ReadSource::ContextChip)]);
        let mut c = Cap::new();
        c.st().foreground = Some(win(400, 11, "kitty.exe", ""));
        c.w.env.write_now(&Write { secret: true, ..text("hunter2") });
        c.capture(ReadSource::Hotkey);
        assert_eq!(c.events(), vec![info(copy::secret_refused())]);
    }

    #[test]
    fn our_own_restore_is_not_reported_as_a_secret() {
        let mut c = Cap::new();
        c.react(vec![Some((30, text("Hello")))]);
        c.capture(ReadSource::Hotkey);
        c.run_for(200); // restored "old" with the do-not-record markers
        assert_eq!(c.clip_text(), "old");
        assert!(c.st().clip.marked);
        c.events();
        // Copy-on-select right after: the user's own clipboard is read, not refused.
        c.st().foreground = Some(win(400, 11, "putty.exe", "PuTTY"));
        c.capture(ReadSource::Hotkey);
        let putty = Some(win(400, 11, "putty.exe", "PuTTY").info);
        assert_eq!(
            c.events(),
            vec![SelectionEvent::Read { text: "old".into(), source: ReadSource::Hotkey, foreground: putty }]
        );
        // Once anything else writes, the markers count again.
        c.w.env.write_now(&Write { secret: true, ..text("hunter2") });
        c.capture(ReadSource::Hotkey);
        assert_eq!(c.events(), vec![info(copy::secret_refused())]);
    }

    /// Two restoring captures in a row: the second snapshot is taken of our own restore and must not record our own
    /// "do not record" marker as the user's data being a secret.
    #[test]
    fn our_own_restores_stay_exempt_after_several_captures() {
        let mut c = Cap::new();
        c.react(vec![Some((30, text("Hello"))), Some((30, text("World")))]);
        c.capture(ReadSource::Hotkey);
        c.run_for(200);
        c.capture(ReadSource::Hotkey);
        c.run_for(200);
        assert_eq!(c.st().restores.len(), 2);
        assert_eq!(c.clip_text(), "old");
        assert_eq!(c.events(), vec![read("Hello", ReadSource::Hotkey), read("World", ReadSource::Hotkey)]);
        c.st().foreground = Some(win(400, 11, "putty.exe", "PuTTY"));
        c.capture(ReadSource::Hotkey);
        let putty = Some(win(400, 11, "putty.exe", "PuTTY").info);
        assert_eq!(
            c.events(),
            vec![SelectionEvent::Read { text: "old".into(), source: ReadSource::Hotkey, foreground: putty }]
        );
        // Private data stays private (not upgraded to a secret, not dropped) across the same two captures.
        let mut c = Cap::new();
        c.w.env.write_now(&Write { private: true, ..text("old") });
        c.react(vec![Some((30, text("Hello"))), Some((30, text("World")))]);
        c.capture(ReadSource::Hotkey);
        c.run_for(200);
        c.capture(ReadSource::Hotkey);
        c.run_for(200);
        c.events();
        c.st().foreground = Some(win(400, 11, "putty.exe", "PuTTY"));
        c.capture(ReadSource::Hotkey);
        assert_eq!(c.events(), vec![info(copy::private_refused())]);
    }

    #[test]
    fn a_restored_secret_stays_a_secret() {
        let mut c = Cap::new();
        // The user's clipboard held a password manager's copy before the capture.
        c.w.env.write_now(&Write { secret: true, ..text("old") });
        c.st().snapshot = snap_text("old");
        c.react(vec![Some((30, text("Hello")))]);
        c.capture(ReadSource::Hotkey);
        c.run_for(200);
        c.events();
        c.st().foreground = Some(win(400, 11, "putty.exe", "PuTTY"));
        c.capture(ReadSource::Hotkey);
        assert_eq!(c.events(), vec![info(copy::secret_refused())]);
    }

    #[test]
    fn password_managers_are_refused_before_any_key() {
        let mut c = Cap::new();
        // A pending restore from an earlier capture is flushed first, even for a refusal.
        c.react(vec![Some((30, text("Hello")))]);
        c.capture(ReadSource::Hotkey);
        c.events();
        c.st().foreground = Some(win(500, 12, "KeePassXC.exe", "Qt5QWindowIcon"));
        c.capture(ReadSource::ContextChip);
        assert_eq!(c.events(), vec![info(copy::password_manager_refused())]);
        let st = c.st();
        assert_eq!(st.chords.len(), 1, "only the earlier capture's chord");
        assert_eq!(st.restores.len(), 1, "the earlier restore was flushed");
        assert_eq!(st.snapshots, 1);
    }

    #[test]
    fn an_elevated_foreground_gets_the_uipi_notice_and_no_keys() {
        let mut c = Cap::new();
        c.st().uipi = Some(true);
        c.st().held_until = 10_000;
        c.capture(ReadSource::Hotkey);
        assert_eq!(c.events(), vec![info(copy::elevated_app_refused())]);
        let st = c.st();
        assert_eq!((st.chords.len(), st.snapshots), (0, 0));
        assert!(st.released.is_empty(), "no modifier was touched");
        // Unknown or not blocked: the capture proceeds.
        drop(st);
        let mut c = Cap::new();
        c.st().uipi = None;
        c.react(vec![Some((30, text("Hello")))]);
        c.capture(ReadSource::Hotkey);
        assert_eq!(c.events(), vec![read("Hello", ReadSource::Hotkey)]);
    }

    #[test]
    fn no_foreground_window_uses_ctrl_c_with_the_escape_retry() {
        let mut c = Cap::new();
        c.st().foreground = None;
        c.react(vec![None, None]);
        c.capture(ReadSource::ContextChip);
        let st = c.st();
        assert_eq!(st.chords.len(), 2);
        assert_eq!(st.escapes.len(), 1);
        assert_eq!(st.uipi_calls, 0);
    }

    #[test]
    fn a_zero_sequence_number_reports_no_selection_and_logs_once() {
        let mut c = Cap::new();
        c.st().clip.seq = 0;
        c.capture(ReadSource::Hotkey);
        assert_eq!(c.events(), vec![SelectionEvent::NoSelection(ReadSource::Hotkey)]);
        assert!(c.st().chords.is_empty());
        assert!(c.w.seq_warned);
        assert!(c.w.pending.is_none());
    }

    #[test]
    fn held_modifiers_are_waited_for_then_force_released() {
        let mut c = Cap::new();
        c.st().held_until = 300;
        c.react(vec![Some((30, text("Hello")))]);
        c.capture(ReadSource::Hotkey);
        assert_eq!(c.st().chords[0].0, 340);
        assert!(c.st().released.is_empty());
        let mut c = Cap::new();
        c.st().held_until = 10_000;
        c.react(vec![Some((30, text("Hello")))]);
        c.capture(ReadSource::Hotkey);
        assert_eq!(c.st().released, vec![vec![0xA2]]);
        assert_eq!(c.st().chords[0].0, 740, "700 ms wait + 40 ms settle");
    }

    #[test]
    fn a_failed_chord_reports_no_selection_and_still_watches_for_a_late_copy() {
        let mut c = Cap::new();
        c.st().chord_ok = false;
        c.capture(ReadSource::ContextChip);
        assert_eq!(c.events(), vec![SelectionEvent::NoSelection(ReadSource::ContextChip)]);
        assert_eq!(c.st().chords.len(), 1);
        assert!(c.st().escapes.is_empty());
        assert!(matches!(c.w.pending, Some(Pending { job: Job::LateCopy { .. }, .. })));
    }

    #[test]
    fn read_selection_is_ignored_while_busy_or_stopped() {
        let (shared, _log) = shared();
        let (tx, rx) = crossbeam_channel::unbounded();
        request_capture(&shared, &tx, ReadSource::Hotkey);
        request_capture(&shared, &tx, ReadSource::ContextChip);
        assert!(shared.busy.load(Ordering::SeqCst));
        let msgs: Vec<WorkerMsg> = rx.try_iter().collect();
        assert_eq!(msgs.len(), 1);
        assert!(matches!(msgs[0], WorkerMsg::Capture { source: ReadSource::Hotkey, via_context_menu: false }));
        shared.busy.store(false, Ordering::SeqCst);
        request_capture(&shared, &tx, ReadSource::ContextChip);
        assert!(matches!(
            rx.try_recv(),
            Ok(WorkerMsg::Capture { source: ReadSource::ContextChip, via_context_menu: true })
        ));
        shared.busy.store(false, Ordering::SeqCst);
        shared.stopped.store(true, Ordering::SeqCst);
        request_capture(&shared, &tx, ReadSource::Hotkey);
        assert!(rx.try_recv().is_err());
        assert!(!shared.busy.load(Ordering::SeqCst));
    }

    #[test]
    fn the_handle_is_shareable_across_threads() {
        fn assert_send_sync_clone<T: Send + Sync + Clone + 'static>() {}
        assert_send_sync_clone::<SelectionService>();
    }

    #[test]
    fn the_service_handle_never_blocks_and_flushes() {
        let log: Log = Arc::new(Mutex::new(Vec::new()));
        let sink = log.clone();
        let service = SelectionService::new(Arc::new(move |ev| sink.lock().push(ev)));
        assert!(service.hook_available());
        assert!(!service.is_busy());
        // Chips off: no hook attempt, so no HookStatus.
        service.start(cfg(false, false));
        service.set_chip_bounds(Some(PhysRect { x: 0, y: 0, width: 10, height: 10 }));
        service.set_chip_bounds(None);
        service.chip_dismissed();
        service.stop();
        service.flush(Duration::from_secs(5));
        // Stopped: a read request is ignored.
        service.read_selection(ReadSource::Hotkey);
        assert!(!service.is_busy());
        std::thread::sleep(Duration::from_millis(50));
        let events = log.lock().clone();
        assert!(events.iter().all(|e| matches!(e, SelectionEvent::ChipHide)), "{events:?}");
    }

    /// `cargo test -p vox-win live_selection_service -- --ignored --nocapture` on an interactive desktop: select
    /// some text in Notepad, then right-click it within 10 s; the chip events and a hotkey-style capture are printed
    /// (the capture runs 8 s in, so keep the selection focused).
    #[test]
    #[ignore = "needs an interactive desktop and a human"]
    fn live_selection_service() {
        let service = SelectionService::new(Arc::new(|ev| match &ev {
            SelectionEvent::Read { text, source, foreground } => {
                println!(
                    "read {} chars from {:?} via {source:?}",
                    text.chars().count(),
                    foreground.as_ref().map(|f| &f.app_name)
                )
            }
            other => println!("{other:?}"),
        }));
        service.start(cfg(true, true));
        std::thread::sleep(Duration::from_secs(8));
        service.read_selection(ReadSource::Hotkey);
        std::thread::sleep(Duration::from_secs(4));
        service.stop();
        service.flush(Duration::from_secs(3));
    }
}
