//! Quit, session end and overlay close requests (shell.md §3.5–§3.7; persistence.md §10; ARCHITECTURE.md §4.17).
//!
//! `shutdown()` (first `Command::Quit`; later requests are ignored while it runs):
//! 1. Arm the **watchdog**: a detached thread that sleeps `SHUTDOWN_WATCHDOG_MS` (= `SHUTDOWN_BUDGET +
//!    FORCE_EXIT_AFTER`, 7 s), then logs `forced exit after shutdown timeout` and exits with
//!    `shots::hard_exit_code()` (0 outside the shots harness) — so a hung service (a cpal/WASAPI stream on a vanished
//!    Bluetooth device, the SMTC STA thread, the hook join) can never leave a process with a dead core and a live tray.
//! 2. Log `shutting down`; cancel the retention / suspension / preload timers; unregister hotkeys;
//!    `selection.stop()` (flags only).
//! 3. `persistPlayback`: `sessions.persist_all(now, Some(&player.snapshot()))` (the player's exact current values,
//!    D-RP-6) → history patches.
//! 4. `engine.stop()` — which emits `Stopped{billed_so_far}` synchronously onto the core channel — then **drain** the
//!    commands already queued (`rx.try_iter()`), dispatching only `Command::Engine(..)` and dropping the rest, so the
//!    billing known at stop time reaches the record (quit mid-synthesis keeps `charactersBilled`).
//! 5. In parallel, bounded by `SHUTDOWN_BUDGET` (3 s): `settings.flush()`, `history.flush()`, `selection.flush()`,
//!    `cache.flush()`, `bridge.stop()` (never waits for in-flight `/read`s).
//! 6. Publish `UiUpdate::Quit` (the UI saves `window-state.json` itself, hides the windows, destroys the tray, arms its
//!    4 s hard exit and leaves the event loop). The core no longer saves the window state (it has no HWND; a
//!    `SaveWindowState` sent after the quit started would be dropped).
//! 7. `player.shutdown()` ×2 and `media.shutdown()`, each on a helper thread joined for at most
//!    `SERVICE_SHUTDOWN_JOIN_MS` (500 ms); then the core loop exits.
//!
//! Tests: a fake service whose `stop()` never returns — the quit path still reaches `UiUpdate::Quit` ([`run_bounded`]
//! here; `Core::shutdown` with an injected hung flush step in `core_tests`); listening values persisted on quit and on
//! session end (`core_tests`).
//!
//! `session_end()` (`WM_QUERYENDSESSION`/`WM_ENDSESSION` via `Command::SessionEnd`, external `WM_CLOSE` on the system
//! window, and an overlay close request): synchronously `persistPlayback` (with the current snapshot),
//! `settings.flush_sync()`, `history.flush_sync()`; never vetoes; idempotent. If the core is busy, the system
//! watcher's handler flushes settings + history itself after 2.5 s (`SessionEndHandler` docs).
//!
//! Overlay close (`WindowEvent::CloseRequested(Widget | Chip)`): `tracing::info!(target: "windows", "{kind} window got
//! a close request; quitting")` → `session_end()` → the quit path above.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::time::{Duration, Instant};

use super::reader::Reader;
use super::timers::TimerId;
use super::Core;
use crate::command::Command;
use crate::update::UiUpdate;

/// Flush budget.
pub const SHUTDOWN_BUDGET: Duration = Duration::from_millis(vox_core::constants::SHUTDOWN_TIMEOUT_MS);
/// Hard exit after cleanup (UI side, armed on `UiUpdate::Quit`; the UI may use this or the vox-core constant).
#[allow(dead_code)]
pub const FORCE_EXIT_AFTER: Duration = Duration::from_millis(vox_core::constants::FORCE_EXIT_MS);
/// The core-side watchdog armed by the first quit request.
pub const WATCHDOG_AFTER: Duration = Duration::from_millis(vox_core::constants::SHUTDOWN_WATCHDOG_MS);
/// Join limit for each of the players' and SMTC's shutdown helpers.
pub const SERVICE_JOIN: Duration = Duration::from_millis(vox_core::constants::SERVICE_SHUTDOWN_JOIN_MS);

/// A named shutdown step for [`run_bounded`].
pub type ShutdownTask = (&'static str, Box<dyn FnOnce() + Send>);

/// Runs every task on its own thread and waits until all finished or `budget` elapsed (a task that never returns is
/// left running; the process exits soon after). Panics inside a task are caught and logged. Returns the names of the
/// tasks that did not finish in time.
pub fn run_bounded(tasks: Vec<ShutdownTask>, budget: Duration) -> Vec<&'static str> {
    let deadline = Instant::now() + budget;
    let (done_tx, done_rx) = crossbeam_channel::unbounded::<&'static str>();
    let mut pending: Vec<&'static str> = Vec::with_capacity(tasks.len());
    for (name, task) in tasks {
        let tx = done_tx.clone();
        let spawned = std::thread::Builder::new().name(format!("vox-quit-{name}")).spawn(move || {
            if catch_unwind(AssertUnwindSafe(task)).is_err() {
                tracing::warn!(target: "app", "shutdown step {name} panicked");
            }
            let _ = tx.send(name);
        });
        match spawned {
            Ok(_) => pending.push(name),
            Err(e) => tracing::warn!(target: "app", "shutdown step {name} could not start: {e}"),
        }
    }
    drop(done_tx);
    while !pending.is_empty() {
        match done_rx.recv_deadline(deadline) {
            Ok(name) => pending.retain(|n| *n != name),
            Err(_) => break,
        }
    }
    pending
}

/// Arms the 7 s watchdog (never in unit tests: it would end the test process).
fn arm_watchdog() {
    if cfg!(test) {
        return;
    }
    let spawned = std::thread::Builder::new().name("vox-quit-watchdog".into()).spawn(|| {
        std::thread::sleep(WATCHDOG_AFTER);
        tracing::error!(target: "app", "forced exit after shutdown timeout");
        std::process::exit(crate::shots::hard_exit_code());
    });
    if let Err(e) = spawned {
        tracing::warn!(target: "app", "quit watchdog could not start: {e}");
    }
}

impl Core {
    /// The quit sequence (module docs). Later quit requests are ignored.
    pub(super) fn shutdown(&mut self) {
        if self.quitting {
            return;
        }
        self.quitting = true;
        // 1.
        arm_watchdog();
        // 2.
        tracing::info!(target: "app", "shutting down");
        self.retention.cancel();
        for id in [
            TimerId::RetentionInterval,
            TimerId::RetentionChange,
            TimerId::HotkeySuspend,
            TimerId::PreloadOutput,
            TimerId::CreditsAfterRead,
            TimerId::CreditsReset,
            TimerId::MediaTick,
            TimerId::Midnight,
            TimerId::HistoryChangedThrottle,
        ] {
            self.timers.cancel(id);
        }
        if let Some(hotkeys) = &self.services.hotkeys {
            if catch_unwind(AssertUnwindSafe(|| hotkeys.unregister_all())).is_err() {
                tracing::warn!(target: "shortcuts", "hotkey unregister failed");
            }
        }
        if catch_unwind(AssertUnwindSafe(|| self.services.selection.stop())).is_err() {
            tracing::warn!(target: "selection", "selection stop failed");
        }
        // 3.
        if catch_unwind(AssertUnwindSafe(|| self.persist_playback())).is_err() {
            tracing::error!(target: "reader", "persisting playback failed");
        }
        // 4. Billing known at stop time reaches the record.
        if catch_unwind(AssertUnwindSafe(|| self.services.engine.stop())).is_err() {
            tracing::error!(target: "reader", "stopping synthesis failed");
        }
        self.drain_engine_events();
        // 5.
        let settings = self.services.settings.clone();
        let history = self.services.history.clone();
        let selection = self.services.selection.clone();
        let cache = self.services.cache.clone();
        let bridge = self.services.bridge.clone();
        #[allow(unused_mut)] // tests add a hung step
        let mut tasks: Vec<ShutdownTask> = vec![
            ("settings", Box::new(move || settings.flush())),
            ("history", Box::new(move || history.flush())),
            ("selection", Box::new(move || selection.flush(SHUTDOWN_BUDGET))),
            ("cache", Box::new(move || {
                if !cache.flush(SHUTDOWN_BUDGET) {
                    tracing::debug!(target: "cache", "audio cache writes still queued at quit");
                }
            })),
            ("bridge", Box::new(move || bridge.stop())),
        ];
        #[cfg(test)]
        tasks.extend(super::core_tests::take_extra_shutdown_tasks());
        let late = run_bounded(tasks, SHUTDOWN_BUDGET);
        if !late.is_empty() {
            tracing::warn!(target: "app", "shutdown flush timed out ({})", late.join(", "));
        }
        // 6.
        self.publish(UiUpdate::Quit);
        // 7.
        let player = self.services.player.clone();
        let preview = self.services.preview_player.clone();
        let mut tasks: Vec<ShutdownTask> =
            vec![("player", Box::new(move || player.shutdown())), ("preview", Box::new(move || preview.shutdown()))];
        // The core is about to exit: these handles move to their shutdown helpers.
        if let Some(media) = self.services.media.take() {
            tasks.push(("media", Box::new(move || media.shutdown())));
        }
        if let Some(hotkeys) = self.services.hotkeys.take() {
            tasks.push(("hotkeys", Box::new(move || hotkeys.shutdown())));
        }
        let late = run_bounded(tasks, SERVICE_JOIN);
        if !late.is_empty() {
            tracing::warn!(target: "app", "services still stopping at exit ({})", late.join(", "));
        }
        self.exit = true;
    }

    /// Dispatches only the engine events already queued (step 4); everything else queued is dropped.
    fn drain_engine_events(&mut self) {
        let queued: Vec<Command> = self.rx.try_iter().collect();
        let mut dropped = 0usize;
        for cmd in queued {
            match cmd {
                Command::Engine(ev) => {
                    if catch_unwind(AssertUnwindSafe(|| self.on_engine_event(ev))).is_err() {
                        tracing::error!(target: "panic", "engine event panicked during shutdown");
                    }
                }
                _ => dropped += 1,
            }
        }
        if dropped > 0 {
            tracing::debug!(target: "app", "dropped {dropped} queued commands at quit");
        }
    }

    /// Log-off / shutdown / overlay close: synchronous save (never vetoes; idempotent).
    pub(super) fn session_end(&mut self) {
        if catch_unwind(AssertUnwindSafe(|| self.persist_playback())).is_err() {
            tracing::error!(target: "reader", "persisting playback failed");
        }
        let settings = &self.services.settings;
        let history = &self.services.history;
        if catch_unwind(AssertUnwindSafe(|| {
            settings.flush_sync();
            history.flush_sync();
        }))
        .is_err()
        {
            tracing::error!(target: "app", "session-end save failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::*;

    #[test]
    fn a_hung_step_cannot_hold_up_the_quit() {
        let ran = Arc::new(AtomicUsize::new(0));
        let r1 = ran.clone();
        let r2 = ran.clone();
        let started = Instant::now();
        let late = run_bounded(
            vec![
                ("hung", Box::new(|| std::thread::sleep(Duration::from_secs(30)))),
                ("quick", Box::new(move || {
                    r1.fetch_add(1, Ordering::SeqCst);
                })),
                ("panics", Box::new(move || {
                    r2.fetch_add(1, Ordering::SeqCst);
                    panic!("boom");
                })),
            ],
            Duration::from_millis(200),
        );
        assert_eq!(late, ["hung"]);
        assert_eq!(ran.load(Ordering::SeqCst), 2);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn all_steps_finishing_returns_early() {
        let started = Instant::now();
        let late = run_bounded(vec![("a", Box::new(|| {})), ("b", Box::new(|| {}))], Duration::from_secs(10));
        assert!(late.is_empty());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn budgets_match_the_constants() {
        assert_eq!(WATCHDOG_AFTER, SHUTDOWN_BUDGET + FORCE_EXIT_AFTER);
        assert_eq!(SERVICE_JOIN, Duration::from_millis(500));
    }
}
