//! Player state, commands and snapshot (contracts.md §3.5, reader-playback.md §1.1, §4.10, §8).
//!
//! In the Rust app the core owns the player (vox-audio). [`PlayerState`] is what the core folds into history, the
//! tray and the bridge's `/health`; [`PlayerSnapshot`] adds the session info and UI flags the widget needs.

use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;

use crate::constants::{PLAYBACK_RATES, RATE_MAX, RATE_MIN};
use crate::read::ReadErrorCode;

/// Player status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PlayerStatus {
    #[default]
    Idle,
    Loading,
    Playing,
    Paused,
    Ended,
    Error,
}

impl PlayerStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Loading => "loading",
            Self::Playing => "playing",
            Self::Paused => "paused",
            Self::Ended => "ended",
            Self::Error => "error",
        }
    }
    /// loading / playing / paused: a read is in progress (tray "Stop" enabled, credits refresh on settle).
    pub fn is_active(self) -> bool {
        matches!(self, Self::Loading | Self::Playing | Self::Paused)
    }
    /// idle / ended / error.
    pub fn is_settled(self) -> bool {
        matches!(self, Self::Idle | Self::Ended | Self::Error)
    }
}

/// The player's state (serialized order of `main/ipc.ts:150-171`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerState {
    pub status: PlayerStatus,
    pub session_id: Option<String>,
    pub read_id: Option<String>,
    pub title: String,
    pub voice_name: String,
    /// Playhead in audio seconds (finished chunk durations + current chunk time).
    pub position_sec: f64,
    /// Known chunk durations + estimate for the unsynthesized rest.
    pub duration_sec: f64,
    pub duration_is_estimate: bool,
    /// 0..1 share already synthesized.
    pub buffered_fraction: f64,
    pub rate: f64,
    pub volume: f64,
    /// Current UTF-16 offset into the session text.
    pub char_index: u32,
    /// Session text length (UTF-16).
    pub total_chars: u32,
    /// Wall-clock seconds audible in this session.
    pub listened_sec: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_app: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<ReadErrorCode>,
}

impl PlayerState {
    /// `IDLE_PLAYER_STATE`.
    pub fn idle() -> Self {
        Self {
            status: PlayerStatus::Idle,
            session_id: None,
            read_id: None,
            title: String::new(),
            voice_name: String::new(),
            position_sec: 0.0,
            duration_sec: 0.0,
            duration_is_estimate: true,
            buffered_fraction: 0.0,
            rate: 1.0,
            volume: 1.0,
            char_index: 0,
            total_chars: 0,
            listened_sec: 0.0,
            source_app: None,
            error: None,
            error_code: None,
        }
    }
}

impl Default for PlayerState {
    fn default() -> Self {
        Self::idle()
    }
}

/// Session info that is stable for the session (cheap to clone through `Arc`).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionInfo {
    pub id: String,
    pub read_id: String,
    pub text: Arc<str>,
    /// `title || text[0..120]`.
    pub title: String,
    pub voice_name: String,
    pub voice_id: String,
    pub source_app: Option<String>,
    pub from_cache: bool,
    pub highlight_words: bool,
    /// Sentence starts (UTF-16).
    pub sentences: Arc<[u32]>,
}

/// A non-fatal session error: the read will stop before the end.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionWarning {
    pub message: String,
    pub code: ReadErrorCode,
}

/// Everything the UI renders about playback.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlayerSnapshot {
    pub state: PlayerState,
    pub session: Option<Arc<SessionInfo>>,
    /// User intent: playing, or resumes as soon as audio arrives (drives the play/pause icon).
    pub wants_play: bool,
    /// Caught up with synthesis, waiting for the next chunk.
    pub buffering: bool,
    /// Audio ready, waiting for the output device to wake.
    pub starting: bool,
    pub warning: Option<SessionWarning>,
    /// Some chunk's text range lay beyond the session text (word positions unknown).
    pub partial_text: bool,
}

/// `skip.direction`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipDirection {
    Back,
    Forward,
}

impl SkipDirection {
    pub fn sign(self) -> f64 {
        match self {
            Self::Back => -1.0,
            Self::Forward => 1.0,
        }
    }
}

/// `skip.unit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipUnit {
    Sentence,
    Seconds,
}

/// A transport command (from the UI, tray, hotkeys, media keys or the bridge).
#[derive(Debug, Clone, PartialEq)]
pub enum PlayerCommand {
    Toggle,
    Play,
    Pause,
    Stop,
    /// Audio seconds (≥ 0).
    Seek { position_sec: f64 },
    /// 0..1 of the estimated total.
    SeekFraction { fraction: f64 },
    /// `amount`: seconds (0..3600) for `Seconds`; `None` = 10 s.
    Skip { direction: SkipDirection, unit: SkipUnit, amount: Option<f64> },
    /// 0.5..3, session only (does not persist settings).
    Rate { rate: f64 },
    /// 0..1, session only.
    Volume { volume: f64 },
}

impl PlayerCommand {
    pub fn next_sentence() -> Self {
        Self::Skip { direction: SkipDirection::Forward, unit: SkipUnit::Sentence, amount: None }
    }
    pub fn previous_sentence() -> Self {
        Self::Skip { direction: SkipDirection::Back, unit: SkipUnit::Sentence, amount: None }
    }
}

fn finite_field(o: &serde_json::Map<String, Value>, k: &str) -> Option<f64> {
    match o.get(k) {
        Some(Value::Number(n)) => n.as_f64().filter(|x| x.is_finite()),
        _ => None,
    }
}

/// `parsePlayerCommand` (main/ipc.ts:118-144): lenient — anything invalid is dropped (`None`); a bad `amount` is
/// ignored. Clamps: seek ≥ 0, fraction 0..1, amount 0..3600, rate 0.5..3, volume 0..1.
pub fn parse_player_command(v: &Value) -> Option<PlayerCommand> {
    let o = v.as_object()?;
    let ty = o.get("type")?.as_str()?;
    Some(match ty {
        "toggle" => PlayerCommand::Toggle,
        "play" => PlayerCommand::Play,
        "pause" => PlayerCommand::Pause,
        "stop" => PlayerCommand::Stop,
        "seek" => PlayerCommand::Seek { position_sec: finite_field(o, "positionSec")?.max(0.0) },
        "seekFraction" => PlayerCommand::SeekFraction { fraction: finite_field(o, "fraction")?.clamp(0.0, 1.0) },
        "skip" => {
            let direction = match o.get("direction").and_then(Value::as_f64) {
                Some(1.0) => SkipDirection::Forward,
                Some(-1.0) => SkipDirection::Back,
                _ => return None,
            };
            let unit = match o.get("unit").and_then(Value::as_str) {
                Some("sentence") => SkipUnit::Sentence,
                Some("seconds") => SkipUnit::Seconds,
                _ => return None,
            };
            PlayerCommand::Skip { direction, unit, amount: finite_field(o, "amount").map(|a| a.clamp(0.0, 3600.0)) }
        }
        "rate" => PlayerCommand::Rate { rate: finite_field(o, "rate")?.clamp(RATE_MIN, RATE_MAX) },
        "volume" => PlayerCommand::Volume { volume: finite_field(o, "volume")?.clamp(0.0, 1.0) },
        _ => return None,
    })
}

/// The bridge's stricter parser (local-server.ts:320-352): an invalid command is an error with the exact HTTP-400
/// message; an `amount` that is present but not a finite number (including `null`) is an error.
pub fn parse_bridge_command(v: &Value) -> Result<PlayerCommand, String> {
    let unknown = || "Unknown command type".to_string();
    let o = v.as_object().ok_or_else(unknown)?;
    let ty = o.get("type").and_then(Value::as_str).ok_or_else(unknown)?;
    let need = |k: &str| finite_field(o, k).ok_or_else(|| format!("\"{k}\" must be a finite number"));
    Ok(match ty {
        "toggle" => PlayerCommand::Toggle,
        "play" => PlayerCommand::Play,
        "pause" => PlayerCommand::Pause,
        "stop" => PlayerCommand::Stop,
        "seek" => PlayerCommand::Seek { position_sec: need("positionSec")?.max(0.0) },
        "seekFraction" => PlayerCommand::SeekFraction { fraction: need("fraction")?.clamp(0.0, 1.0) },
        "skip" => {
            let direction = match o.get("direction") {
                Some(Value::Number(n)) if n.as_f64() == Some(1.0) => SkipDirection::Forward,
                Some(Value::Number(n)) if n.as_f64() == Some(-1.0) => SkipDirection::Back,
                _ => return Err("\"direction\" must be 1 or -1".into()),
            };
            let unit = match o.get("unit").and_then(Value::as_str) {
                Some("sentence") => SkipUnit::Sentence,
                Some("seconds") => SkipUnit::Seconds,
                _ => return Err("\"unit\" must be sentence|seconds".into()),
            };
            let amount = match o.get("amount") {
                None => None,
                Some(_) => Some(need("amount")?.clamp(0.0, 3600.0)),
            };
            PlayerCommand::Skip { direction, unit, amount }
        }
        "rate" => PlayerCommand::Rate { rate: need("rate")?.clamp(RATE_MIN, RATE_MAX) },
        "volume" => PlayerCommand::Volume { volume: need("volume")?.clamp(0.0, 1.0) },
        _ => return Err(unknown()),
    })
}

/// Validates an already-typed command from any in-process source (contracts.md §5.1: "keep these validators in Rust
/// even in-process"; ARCHITECTURE.md §3.2). Same rules as [`parse_player_command`]: `Seek` non-finite → `None`, else
/// `max(0)`; `SeekFraction` non-finite → `None`, else clamped 0..1; `Skip` with a non-finite `amount` keeps the
/// command with `amount: None` (the 10 s default), else the amount is clamped 0..3600; `Rate` / `Volume` non-finite →
/// `None`, else clamped [`RATE_MIN`]..[`RATE_MAX`] / 0..1. Toggle/Play/Pause/Stop pass through. The core calls it
/// first in `reader::command` (UI sliders, tray, hotkeys, the bridge) and in `media` before mapping SMTC seeks.
pub fn sanitize_player_command(cmd: PlayerCommand) -> Option<PlayerCommand> {
    let finite = |x: f64| x.is_finite().then_some(x);
    Some(match cmd {
        PlayerCommand::Seek { position_sec } => PlayerCommand::Seek { position_sec: finite(position_sec)?.max(0.0) },
        PlayerCommand::SeekFraction { fraction } => PlayerCommand::SeekFraction { fraction: finite(fraction)?.clamp(0.0, 1.0) },
        PlayerCommand::Skip { direction, unit, amount } => {
            PlayerCommand::Skip { direction, unit, amount: amount.and_then(finite).map(|a| a.clamp(0.0, 3600.0)) }
        }
        PlayerCommand::Rate { rate } => PlayerCommand::Rate { rate: finite(rate)?.clamp(RATE_MIN, RATE_MAX) },
        PlayerCommand::Volume { volume } => PlayerCommand::Volume { volume: finite(volume)?.clamp(0.0, 1.0) },
        other => other,
    })
}

/// The widget's speed cycle: first preset `> rate + 0.001`, else wraps to 0.75.
pub fn next_rate(rate: f64) -> f64 {
    PLAYBACK_RATES.iter().copied().find(|r| *r > rate + 0.001).unwrap_or(PLAYBACK_RATES[0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lenient_parser() {
        assert_eq!(parse_player_command(&json!({"type": "rate", "rate": 10})), Some(PlayerCommand::Rate { rate: 3.0 }));
        assert_eq!(parse_player_command(&json!({"type": "seek", "positionSec": -5})), Some(PlayerCommand::Seek { position_sec: 0.0 }));
        assert_eq!(
            parse_player_command(&json!({"type": "skip", "direction": -1, "unit": "seconds", "amount": "x"})),
            Some(PlayerCommand::Skip { direction: SkipDirection::Back, unit: SkipUnit::Seconds, amount: None })
        );
        assert_eq!(parse_player_command(&json!({"type": "skip", "direction": 2, "unit": "seconds"})), None);
        assert_eq!(parse_player_command(&json!({"type": "nope"})), None);
        assert_eq!(parse_player_command(&json!([])), None);
    }

    #[test]
    fn strict_parser() {
        assert_eq!(
            parse_bridge_command(&json!({"type": "skip", "direction": -1, "unit": "seconds", "amount": 99999})).unwrap(),
            PlayerCommand::Skip { direction: SkipDirection::Back, unit: SkipUnit::Seconds, amount: Some(3600.0) }
        );
        assert_eq!(
            parse_bridge_command(&json!({"type": "skip", "direction": 1, "unit": "seconds", "amount": null})).unwrap_err(),
            "\"amount\" must be a finite number"
        );
        assert_eq!(parse_bridge_command(&json!({"type": "skip", "direction": "1", "unit": "x"})).unwrap_err(), "\"direction\" must be 1 or -1");
        assert_eq!(parse_bridge_command(&json!({"type": "skip", "direction": 1, "unit": "x"})).unwrap_err(), "\"unit\" must be sentence|seconds");
        assert_eq!(parse_bridge_command(&json!({"type": "rate", "rate": "2"})).unwrap_err(), "\"rate\" must be a finite number");
        assert_eq!(parse_bridge_command(&json!({})).unwrap_err(), "Unknown command type");
    }

    #[test]
    fn typed_commands_are_sanitized_like_the_lenient_parser() {
        use PlayerCommand as C;
        assert_eq!(sanitize_player_command(C::Rate { rate: 10.0 }), Some(C::Rate { rate: 3.0 }));
        assert_eq!(sanitize_player_command(C::Rate { rate: 0.1 }), Some(C::Rate { rate: 0.5 }));
        assert_eq!(sanitize_player_command(C::Rate { rate: f64::NAN }), None);
        assert_eq!(sanitize_player_command(C::Seek { position_sec: -5.0 }), Some(C::Seek { position_sec: 0.0 }));
        assert_eq!(sanitize_player_command(C::Seek { position_sec: f64::INFINITY }), None);
        assert_eq!(sanitize_player_command(C::SeekFraction { fraction: 1.5 }), Some(C::SeekFraction { fraction: 1.0 }));
        assert_eq!(sanitize_player_command(C::SeekFraction { fraction: f64::NAN }), None);
        assert_eq!(sanitize_player_command(C::Volume { volume: -0.2 }), Some(C::Volume { volume: 0.0 }));
        assert_eq!(sanitize_player_command(C::Volume { volume: f64::NEG_INFINITY }), None);
        let skip = |amount| C::Skip { direction: SkipDirection::Back, unit: SkipUnit::Seconds, amount };
        assert_eq!(sanitize_player_command(skip(Some(f64::NAN))), Some(skip(None)));
        assert_eq!(sanitize_player_command(skip(Some(99_999.0))), Some(skip(Some(3600.0))));
        assert_eq!(sanitize_player_command(skip(Some(-1.0))), Some(skip(Some(0.0))));
        assert_eq!(sanitize_player_command(C::Stop), Some(C::Stop));
        // Same outcome as the lenient JSON parser for the same inputs.
        assert_eq!(
            parse_player_command(&json!({"type": "rate", "rate": 10})),
            sanitize_player_command(C::Rate { rate: 10.0 })
        );
    }

    #[test]
    fn idle_state_and_rates() {
        let s = PlayerState::idle();
        assert_eq!(
            crate::json::to_js_compact(&s),
            r#"{"status":"idle","sessionId":null,"readId":null,"title":"","voiceName":"","positionSec":0.0,"durationSec":0.0,"durationIsEstimate":true,"bufferedFraction":0.0,"rate":1.0,"volume":1.0,"charIndex":0,"totalChars":0,"listenedSec":0.0}"#
        );
        assert_eq!(next_rate(1.0), 1.25);
        assert_eq!(next_rate(2.0), 0.75);
        assert_eq!(next_rate(2.5), 0.75);
        assert_eq!(next_rate(0.5), 0.75);
    }
}
