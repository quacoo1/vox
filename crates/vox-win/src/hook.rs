//! The global input hook thread (selection.md §4, §14): installs `WH_MOUSE_LL` + `WH_KEYBOARD_LL`, pumps
//! `GetMessageW`, and from the hook procedures only copies `(msg, pt, mouseData, time, vk, scan, flags)` into a
//! channel and calls `CallNextHookEx` (never consumes input). Mouse moves are forwarded only while
//! [`InputHook::set_wants_mouse_move`] is true. Panics never cross the FFI boundary (`catch_unwind`, errors logged at
//! most once per 10 s). A start failure is **not** sticky (BRIEF.md Product rulings "Hook"): the selection service
//! retries `start` whenever the trigger settings change, and reports availability through
//! `SelectionEvent::HookStatus` (the Settings → Triggers notice).
//!
//! Key events Vox injected itself (tagged with `keys::VOX_INPUT_TAG` in `dwExtraInfo`, D-SEL-3) are dropped here.
//! Every procedure call, filtered or not, bumps an event counter the selection consumer's watchdog reads
//! (`InputHook::event_count`) to notice a hook that Windows removed silently.

#![warn(dead_code, unused)]

use std::cell::{Cell, RefCell};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    GetCurrentThread, GetCurrentThreadId, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, PeekMessageW, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, HC_ACTION,
    HHOOK, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED, MSG, MSLLHOOKSTRUCT, PM_NOREMOVE, WH_KEYBOARD_LL,
    WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_MOUSEMOVE, WM_QUIT, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use crate::keys::VOX_INPUT_TAG;

/// A raw hook event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawInput {
    Mouse { msg: u32, x: i32, y: i32, mouse_data: u32, time: u32 },
    Key { down: bool, vk: u32, scan: u32, extended: bool, injected: bool, time: u32 },
}

/// The running hook.
pub struct InputHook {
    thread: Option<JoinHandle<()>>,
    thread_id: u32,
    wants_move: Arc<AtomicBool>,
    events: Arc<AtomicU64>,
}

/// What the hook procedures of one hook thread share with its owner.
struct ProcContext {
    tx: Sender<RawInput>,
    wants_move: Arc<AtomicBool>,
    events: Arc<AtomicU64>,
    last_warn: Cell<Option<Instant>>,
}

thread_local! {
    /// Set on the hook thread before the hooks are installed; the procedures run on that thread.
    static CONTEXT: RefCell<Option<ProcContext>> = const { RefCell::new(None) };
}

impl InputHook {
    /// Starts the hook thread; fails if `SetWindowsHookExW` fails. The hook thread calls
    /// `SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL)` before `SetWindowsHookExW` (parity with
    /// libuiohook) so the procedures meet `LowLevelHooksTimeout` under CPU load; the procedures allocate nothing beyond
    /// one crossbeam send. Windows can still remove a hook silently; the selection consumer detects that (no event for
    /// 10 s while `GetLastInputInfo` advances) and re-installs (selection module docs).
    pub fn start(events: Sender<RawInput>) -> Result<InputHook, crate::WinError> {
        let wants_move = Arc::new(AtomicBool::new(false));
        let counter = Arc::new(AtomicU64::new(0));
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<u32, crate::WinError>>(1);
        let ctx_wants = wants_move.clone();
        let ctx_events = counter.clone();
        let thread = std::thread::Builder::new()
            .name("vox-input-hook".into())
            .spawn(move || {
                let ctx =
                    ProcContext { tx: events, wants_move: ctx_wants, events: ctx_events, last_warn: Cell::new(None) };
                hook_thread(ctx, ready_tx);
            })
            .map_err(|e| crate::WinError(format!("cannot start the input hook thread: {e}")))?;
        match ready_rx.recv() {
            Ok(Ok(thread_id)) => Ok(InputHook { thread: Some(thread), thread_id, wants_move, events: counter }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(crate::WinError("the input hook thread ended before installing the hooks".into()))
            }
        }
    }

    pub fn set_wants_mouse_move(&self, wants: bool) {
        self.wants_move.store(wants, Ordering::Relaxed);
    }

    /// Unhooks and ends the thread.
    pub fn stop(self) {
        // Drop does the work.
    }

    /// How many times the hook procedures ran (every event, including filtered moves and our own injected keys).
    pub(crate) fn event_count(&self) -> u64 {
        self.events.load(Ordering::Relaxed)
    }

    fn shutdown(&mut self) {
        if let Some(thread) = self.thread.take() {
            // SAFETY: plain value arguments; the thread created its message queue before reporting ready.
            if let Err(e) = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) } {
                tracing::warn!(target: "selection", "global hook failed to stop: {e}");
                // The thread cannot be woken; leave it detached rather than block forever.
                return;
            }
            if thread.join().is_err() {
                tracing::warn!(target: "selection", "global hook thread panicked while stopping");
            }
        }
    }
}

impl Drop for InputHook {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// `GetLastInputInfo().dwTime` (tick count of the session's last input), for the silent-removal watchdog.
pub(crate) fn last_input_tick() -> Option<u32> {
    let mut info = LASTINPUTINFO { cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32, dwTime: 0 };
    // SAFETY: `info` is a properly sized LASTINPUTINFO.
    if unsafe { GetLastInputInfo(&mut info) }.as_bool() {
        Some(info.dwTime)
    } else {
        None
    }
}

/// Unhooks on drop (on the hook thread, which owns the hooks).
struct Installed(HHOOK);

impl Drop for Installed {
    fn drop(&mut self) {
        // SAFETY: the handle came from SetWindowsHookExW on this thread and is unhooked once.
        if let Err(e) = unsafe { UnhookWindowsHookEx(self.0) } {
            tracing::warn!(target: "selection", "UnhookWindowsHookEx failed: {e}");
        }
    }
}

fn hook_thread(ctx: ProcContext, ready: crossbeam_channel::Sender<Result<u32, crate::WinError>>) {
    // SAFETY: the pseudo handle of the current thread.
    if let Err(e) = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) } {
        tracing::debug!(target: "selection", "hook thread priority not raised: {e}");
    }
    // Create the message queue before anyone can post WM_QUIT to this thread.
    let mut msg = MSG::default();
    // SAFETY: `msg` outlives the call.
    unsafe {
        let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
    }
    let _ = CONTEXT.try_with(|c| *c.borrow_mut() = Some(ctx));
    let installed = match install() {
        Ok(hooks) => hooks,
        Err(e) => {
            let _ = CONTEXT.try_with(|c| c.borrow_mut().take());
            let _ = ready.send(Err(e));
            return;
        }
    };
    // SAFETY: no arguments.
    let _ = ready.send(Ok(unsafe { GetCurrentThreadId() }));
    loop {
        // SAFETY: `msg` outlives the call; the LL hook procedures are called from inside GetMessageW.
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if r.0 == 0 || r.0 == -1 {
            break;
        }
    }
    drop(installed);
    let _ = CONTEXT.try_with(|c| c.borrow_mut().take());
}

fn install() -> Result<(Installed, Installed), crate::WinError> {
    // SAFETY: the module handle of the executable (never freed).
    let module = unsafe { GetModuleHandleW(None) }.ok().map(|m| m.into());
    // SAFETY: the procedures are `extern "system"` functions that live for the whole process.
    let mouse = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), module, 0) }
        .map_err(|e| crate::WinError(format!("Failed to register low level windows hook: {e}")))?;
    let mouse = Installed(mouse);
    // SAFETY: as above.
    let keyboard = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), module, 0) }
        .map_err(|e| crate::WinError(format!("Failed to register low level windows hook: {e}")))?;
    Ok((mouse, Installed(keyboard)))
}

/// Logs a caught panic at most once per 10 s.
fn warn_throttled(ctx: &ProcContext, what: &str) {
    let now = Instant::now();
    if ctx.last_warn.get().is_some_and(|t| now.duration_since(t) < Duration::from_secs(10)) {
        return;
    }
    ctx.last_warn.set(Some(now));
    tracing::warn!(target: "selection", "{what}");
}

fn with_context(f: impl FnOnce(&ProcContext)) {
    let _ = CONTEXT.try_with(|c| {
        if let Ok(guard) = c.try_borrow() {
            if let Some(ctx) = guard.as_ref() {
                f(ctx);
            }
        }
    });
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && lparam.0 != 0 {
        let caught = catch_unwind(AssertUnwindSafe(|| {
            // SAFETY: for HC_ACTION, lparam points at the system's MSLLHOOKSTRUCT for this call.
            let info = unsafe { *(lparam.0 as *const MSLLHOOKSTRUCT) };
            with_context(|ctx| on_mouse(ctx, wparam.0 as u32, &info));
        }));
        if caught.is_err() {
            with_context(|ctx| warn_throttled(ctx, "input hook: mouse procedure panicked"));
        }
    }
    // SAFETY: passes the call on unchanged (the hook never consumes input).
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && lparam.0 != 0 {
        let caught = catch_unwind(AssertUnwindSafe(|| {
            // SAFETY: for HC_ACTION, lparam points at the system's KBDLLHOOKSTRUCT for this call.
            let info = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
            with_context(|ctx| on_key(ctx, wparam.0 as u32, &info));
        }));
        if caught.is_err() {
            with_context(|ctx| warn_throttled(ctx, "input hook: keyboard procedure panicked"));
        }
    }
    // SAFETY: passes the call on unchanged.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn on_mouse(ctx: &ProcContext, msg: u32, info: &MSLLHOOKSTRUCT) {
    ctx.events.fetch_add(1, Ordering::Relaxed);
    if let Some(raw) = mouse_event(msg, info, ctx.wants_move.load(Ordering::Relaxed)) {
        let _ = ctx.tx.send(raw);
    }
}

fn on_key(ctx: &ProcContext, msg: u32, info: &KBDLLHOOKSTRUCT) {
    ctx.events.fetch_add(1, Ordering::Relaxed);
    if let Some(raw) = key_event(msg, info) {
        let _ = ctx.tx.send(raw);
    }
}

/// The mouse event to forward (`None` for a move while nobody wants moves).
fn mouse_event(msg: u32, info: &MSLLHOOKSTRUCT, wants_move: bool) -> Option<RawInput> {
    if msg == WM_MOUSEMOVE && !wants_move {
        return None;
    }
    Some(RawInput::Mouse { msg, x: info.pt.x, y: info.pt.y, mouse_data: info.mouseData, time: info.time })
}

/// The key event to forward (`None` for our own tagged injections and unknown messages).
fn key_event(msg: u32, info: &KBDLLHOOKSTRUCT) -> Option<RawInput> {
    if info.dwExtraInfo == VOX_INPUT_TAG {
        return None;
    }
    let down = match msg {
        WM_KEYDOWN | WM_SYSKEYDOWN => true,
        WM_KEYUP | WM_SYSKEYUP => false,
        _ => return None,
    };
    Some(RawInput::Key {
        down,
        vk: info.vkCode,
        scan: info.scanCode,
        extended: info.flags.contains(LLKHF_EXTENDED),
        injected: info.flags.contains(LLKHF_INJECTED),
        time: info.time,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::{KBDLLHOOKSTRUCT_FLAGS, WM_LBUTTONDOWN, WM_MOUSEWHEEL};

    fn kb(vk: u32, flags: u32, extra: usize) -> KBDLLHOOKSTRUCT {
        KBDLLHOOKSTRUCT {
            vkCode: vk,
            scanCode: 0x2E,
            flags: KBDLLHOOKSTRUCT_FLAGS(flags),
            time: 77,
            dwExtraInfo: extra,
        }
    }

    #[test]
    fn our_own_injected_keys_are_dropped_but_other_injections_pass() {
        assert_eq!(key_event(WM_KEYDOWN, &kb(0x43, 0x10, VOX_INPUT_TAG)), None);
        // AutoHotkey / on-screen keyboards inject too: those still count.
        assert_eq!(
            key_event(WM_KEYDOWN, &kb(0x43, 0x10, 0)),
            Some(RawInput::Key { down: true, vk: 0x43, scan: 0x2E, extended: false, injected: true, time: 77 })
        );
    }

    #[test]
    fn key_messages_map_to_down_and_up_with_the_extended_flag() {
        assert!(matches!(key_event(WM_SYSKEYDOWN, &kb(0x12, 0, 0)), Some(RawInput::Key { down: true, .. })));
        assert!(matches!(key_event(WM_SYSKEYUP, &kb(0x12, 0, 0)), Some(RawInput::Key { down: false, .. })));
        assert!(matches!(
            key_event(WM_KEYUP, &kb(0x2D, 1, 0)),
            Some(RawInput::Key { down: false, extended: true, .. })
        ));
        assert_eq!(key_event(0x0123, &kb(0x2D, 1, 0)), None);
    }

    #[test]
    fn moves_are_forwarded_only_when_wanted() {
        let info =
            MSLLHOOKSTRUCT { pt: POINT { x: -5, y: 7 }, mouseData: 120 << 16, flags: 0, time: 9, dwExtraInfo: 0 };
        assert_eq!(mouse_event(WM_MOUSEMOVE, &info, false), None);
        assert_eq!(
            mouse_event(WM_MOUSEMOVE, &info, true),
            Some(RawInput::Mouse { msg: WM_MOUSEMOVE, x: -5, y: 7, mouse_data: 120 << 16, time: 9 })
        );
        assert!(mouse_event(WM_LBUTTONDOWN, &info, false).is_some());
        assert!(mouse_event(WM_MOUSEWHEEL, &info, false).is_some());
    }

    /// `cargo test -p vox-win hook_starts_and_stops -- --ignored --nocapture` on an interactive desktop; move the
    /// mouse or press a key during the 3 s window to see events arrive.
    #[test]
    #[ignore = "installs a global input hook on the interactive desktop"]
    fn hook_starts_and_stops() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let hook = InputHook::start(tx).expect("SetWindowsHookExW");
        hook.set_wants_mouse_move(true);
        let deadline = Instant::now() + Duration::from_secs(3);
        let (mut keys, mut mice) = (0u64, 0u64);
        while let Ok(ev) = rx.recv_deadline(deadline) {
            // Counts only: key codes are never printed.
            match ev {
                RawInput::Key { .. } => keys += 1,
                RawInput::Mouse { .. } => mice += 1,
            }
        }
        println!("{keys} key and {mice} mouse events forwarded, {} procedure calls", hook.event_count());
        assert!(hook.event_count() >= keys + mice);
        hook.stop();
        // A second hook can be installed after the first one stopped.
        let (tx, _rx) = crossbeam_channel::unbounded();
        InputHook::start(tx).expect("restart").stop();
        assert!(last_input_tick().is_some());
    }
}
