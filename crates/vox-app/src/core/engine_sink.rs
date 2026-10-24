//! Adapters between the engine, the players and the core channel.
//!
//! - [`PlayerSink`] implements `vox_tts::TtsSink` by forwarding to the reading `vox_audio::Player` (each call only
//!   enqueues on the player thread, so it never blocks the engine).
//! - Engine events and player events become `Command::Engine` / `Command::PlayerReport` / `Command::PreviewReport`.
//!   Ordering: the engine emits `Stopped{error}` before `sink.done` (vox_tts::engine docs), so the `Command::Engine`
//!   is queued before any report the player produces after `done`.

use std::sync::Arc;

use vox_audio::{Player, PlayerEvent};
use vox_core::tts::{TtsChunk, TtsSessionDone, TtsSessionError, TtsSessionStart};
use vox_tts::{EngineEvent, TtsSink};

use crate::command::{Command, CoreHandle};

/// `TtsSink` → reading player.
pub struct PlayerSink {
    pub player: Player,
}

impl TtsSink for PlayerSink {
    fn session_start(&self, start: TtsSessionStart) {
        self.player.start_session(start);
    }
    fn chunk(&self, chunk: TtsChunk) {
        self.player.add_chunk(chunk);
    }
    fn done(&self, done: TtsSessionDone) {
        self.player.finish_session(done);
    }
    fn error(&self, error: TtsSessionError) {
        self.player.fail_session(error);
    }
}

/// Engine events → the core.
pub fn engine_events(core: CoreHandle) -> Arc<dyn Fn(EngineEvent) + Send + Sync> {
    Arc::new(move |ev| core.send(Command::Engine(ev)))
}

/// Reading-player events → the core.
pub fn player_events(core: CoreHandle) -> Arc<dyn Fn(PlayerEvent) + Send + Sync> {
    Arc::new(move |ev| match ev {
        PlayerEvent::State(s) => core.send(Command::PlayerReport(s)),
        PlayerEvent::Failed { session_id, message } => core.send(Command::PlayerFailed { session_id, message }),
    })
}

/// Preview-player events → the core (a preview-player failure ends the sample with `PreviewUpdate::Failed`, which the
/// Voices / onboarding controller turns into its `voice-preview` toast).
pub fn preview_events(core: CoreHandle) -> Arc<dyn Fn(PlayerEvent) + Send + Sync> {
    Arc::new(move |ev| match ev {
        PlayerEvent::State(s) => core.send(Command::PreviewReport(s)),
        PlayerEvent::Failed { message, .. } => core.send(Command::PreviewFailed { message }),
    })
}
