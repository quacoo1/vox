//! Browser-bridge glue (shell.md §4.2, integrations.md §1.3).
//!
//! [`BridgeGlue`] implements `vox_bridge::BridgeHandler` on bridge connection threads: `read` sends
//! `Command::Bridge(Read{request, reply})` and waits with `reply.recv_deadline(deadline)`; a timeout or a disconnected
//! channel (core gone during quit) returns `None` at once (→ 504; the core still runs the read if it later dequeues
//! it). The core maps the request with `BridgeReadRequest::into_request(foreground)` (foreground kept only for known
//! browsers).
//! `command` sends `Command::Player`. `status` reads a shared atomic "playing" flag the core updates on every player
//! report. `sync_server(settings)`: want = `browserExtension && !shots`; not wanted → stop; same port running →
//! nothing; else (re)start; failure → warn + error toast `bridge_port_failed(port)` with "Open settings" →
//! settings/integrations (not in demo); always followed by a fresh `UiUpdate::AppInfo` (ends the Settings re-check
//! window). Calls are serialized by running them on the core thread; `BridgeServer::stop` never waits for in-flight
//! `read` calls, so calling it from the core cannot deadlock.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use vox_bridge::{BridgeHandler, BridgeStatus};
use vox_core::app::Toast;
use vox_core::player::PlayerCommand;
use vox_core::read::{BridgeReadRequest, ReadResult};

use super::Core;
use crate::command::{BridgeCall, Command, CoreHandle};

/// The handler given to `BridgeServer::new`.
pub struct BridgeGlue {
    pub core: CoreHandle,
    pub playing: Arc<AtomicBool>,
}

impl BridgeHandler for BridgeGlue {
    fn read(&self, req: BridgeReadRequest, deadline: Instant) -> Option<ReadResult> {
        let (reply, answer) = crossbeam_channel::bounded::<ReadResult>(1);
        self.core.send(Command::Bridge(BridgeCall::Read { request: req, reply }));
        // Disconnected (the core is gone and dropped the command, or dropped it during the quit drain) → `None` at once.
        answer.recv_deadline(deadline).ok()
    }
    fn command(&self, cmd: PlayerCommand) {
        self.core.send(Command::Player(cmd));
    }
    fn status(&self) -> BridgeStatus {
        BridgeStatus {
            app_name: vox_core::constants::APP_NAME.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            playing: self.playing.load(Ordering::Relaxed),
        }
    }
}

/// What `sync_server` must do (pure; tested).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeSync {
    /// Not wanted: stop (a no-op when stopped).
    Stop,
    /// Wanted and already listening on that port.
    Keep,
    /// Wanted: (re)start on this port.
    Start(u16),
}

/// `syncServer` decision: want = `browserExtension && !shots`.
pub fn bridge_sync(browser_extension: bool, shots: bool, port: u16, running: bool, running_port: u16) -> BridgeSync {
    if !browser_extension || shots {
        BridgeSync::Stop
    } else if running && running_port == port {
        BridgeSync::Keep
    } else {
        BridgeSync::Start(port)
    }
}

impl Core {
    /// `syncServer(settings)` at startup and when `triggers.browserExtension` or `server.port` changed; always
    /// followed by a fresh `AppInfo`.
    pub(super) fn sync_bridge(&mut self) {
        let settings = self.settings();
        let bridge = self.services.bridge.clone();
        let decision = bridge_sync(settings.triggers.browser_extension, self.shots(), settings.server.port, bridge.running(), bridge.port());
        let outcome = catch_unwind(AssertUnwindSafe(|| match decision {
            BridgeSync::Stop => {
                if bridge.running() {
                    bridge.stop();
                    tracing::info!(target: "bridge", "bridge stopped");
                }
                None
            }
            BridgeSync::Keep => None,
            BridgeSync::Start(port) => {
                let started = bridge.start(port);
                if started.ok {
                    tracing::info!(target: "bridge", "bridge listening on 127.0.0.1:{}", started.port);
                    None
                } else {
                    let detail = started.error.unwrap_or_else(|| "unknown error".to_string());
                    tracing::warn!(target: "bridge", "bridge failed on port {port}: {detail}");
                    Some(port)
                }
            }
        }));
        match outcome {
            Ok(Some(port)) if !self.demo() => {
                self.toast(Toast::error(vox_core::copy::bridge_port_failed(port)).with_settings_action("integrations"));
            }
            Ok(_) => {}
            Err(_) => tracing::error!(target: "bridge", "bridge sync failed"),
        }
        self.publish_app_info();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn sync_decisions() {
        assert_eq!(bridge_sync(false, false, 47823, true, 47823), BridgeSync::Stop);
        assert_eq!(bridge_sync(true, true, 47823, false, 0), BridgeSync::Stop, "never in shots mode");
        assert_eq!(bridge_sync(true, false, 47823, true, 47823), BridgeSync::Keep);
        assert_eq!(bridge_sync(true, false, 47823, true, 50000), BridgeSync::Start(47823), "port changed");
        assert_eq!(bridge_sync(true, false, 47823, false, 47823), BridgeSync::Start(47823), "a dead listener restarts");
    }

    fn glue() -> (BridgeGlue, crossbeam_channel::Receiver<Command>) {
        let (tx, rx) = crossbeam_channel::unbounded();
        (BridgeGlue { core: CoreHandle::new(tx), playing: Arc::new(AtomicBool::new(false)) }, rx)
    }

    fn request() -> BridgeReadRequest {
        BridgeReadRequest { text: "hello".into(), url: None, title: None }
    }

    #[test]
    fn read_waits_for_the_core_answer() {
        let (glue, rx) = glue();
        let core = std::thread::spawn(move || match rx.recv().unwrap() {
            Command::Bridge(BridgeCall::Read { reply, .. }) => reply.send(ReadResult::started("r1")).unwrap(),
            other => panic!("unexpected {other:?}"),
        });
        let answer = glue.read(request(), Instant::now() + Duration::from_secs(5));
        assert_eq!(answer, Some(ReadResult::started("r1")));
        core.join().unwrap();
    }

    #[test]
    fn read_times_out_without_an_answer() {
        let (glue, rx) = glue();
        let started = Instant::now();
        assert_eq!(glue.read(request(), Instant::now() + Duration::from_millis(80)), None);
        assert!(started.elapsed() >= Duration::from_millis(70));
        assert!(matches!(rx.try_recv(), Ok(Command::Bridge(_))), "the read is still queued for the core");
    }

    #[test]
    fn read_returns_at_once_when_the_core_is_gone() {
        let (glue, rx) = glue();
        drop(rx);
        let started = Instant::now();
        assert_eq!(glue.read(request(), Instant::now() + Duration::from_secs(10)), None);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn status_and_commands() {
        let (glue, rx) = glue();
        glue.playing.store(true, Ordering::Relaxed);
        let s = glue.status();
        assert_eq!((s.app_name.as_str(), s.playing), ("Vox", true));
        assert_eq!(s.version, env!("CARGO_PKG_VERSION"));
        glue.command(PlayerCommand::Pause);
        assert!(matches!(rx.try_recv(), Ok(Command::Player(PlayerCommand::Pause))));
    }
}
