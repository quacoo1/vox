//! Media keys / system media controls: not wired on Linux yet (an MPRIS service would go here). Same API as the
//! Windows SMTC session; every call is a no-op.

use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MediaAction {
    Play,
    Pause,
    Stop,
    Previous,
    Next,
    SeekTo(f64),
    SeekBy(f64),
}

#[derive(Debug, Clone, PartialEq)]
pub struct MediaState {
    pub active: bool,
    pub playing: bool,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub position_sec: f64,
    pub duration_sec: f64,
    pub rate: f64,
}

pub type MediaHandler = Arc<dyn Fn(MediaAction) + Send + Sync>;

pub struct MediaSession;

impl MediaSession {
    pub fn spawn(_on_action: MediaHandler) -> Result<MediaSession, crate::WinError> {
        Err(crate::WinError("system media controls are not available on Linux yet".into()))
    }
    pub fn set_enabled(&self, _enabled: bool) {}
    pub fn update(&self, _state: MediaState) {}
    pub fn shutdown(&self) {}
}
