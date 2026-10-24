//! SMTC glue (reader-playback.md §9): `MediaState` from player reports (active = mediaKeys && session && status ≠
//! error && status ≠ idle — the second condition is defensive: after an explicit stop the player also sends a
//! sessionless `idle` report; title = session title || `Vox`; artist = voice; album = `Vox`; position refreshed ≤ once
//! per second unless status/rate changed). Actions: play/pause → `Player`; stop → `reader::command(Stop)` (which hides
//! the widget when a session existed, like every other stop); previous/next → sentence skip; seek-to → `Seek`; seek-by →
//! `Skip{seconds}` — the OS-supplied `f64`s go through `vox_core::player::sanitize_player_command` (NaN dropped,
//! negative clamped) before mapping.

use vox_core::constants::APP_NAME;
use vox_core::player::{PlayerCommand, PlayerSnapshot, PlayerStatus, SkipDirection, SkipUnit};
use vox_win::media::{MediaAction, MediaState};

/// Pure mapping from a snapshot (tested).
pub fn media_state(snapshot: &PlayerSnapshot, media_keys: bool) -> MediaState {
    let status = snapshot.state.status;
    let session = snapshot.session.as_deref();
    let active = media_keys && session.is_some() && status != PlayerStatus::Error && status != PlayerStatus::Idle;
    let title = session.map(|s| s.title.trim()).filter(|t| !t.is_empty()).unwrap_or(APP_NAME).to_string();
    let finite = |x: f64| if x.is_finite() { x.max(0.0) } else { 0.0 };
    MediaState {
        active,
        playing: active && status == PlayerStatus::Playing,
        title,
        artist: session.map(|s| s.voice_name.clone()).unwrap_or_default(),
        album: APP_NAME.to_string(),
        position_sec: finite(snapshot.state.position_sec),
        duration_sec: finite(snapshot.state.duration_sec),
        rate: if snapshot.state.rate.is_finite() && snapshot.state.rate > 0.0 { snapshot.state.rate } else { 1.0 },
    }
}

/// The transport command for an SMTC action (before `sanitize_player_command`; the OS-supplied numbers are
/// untrusted: NaN seeks are dropped by the sanitizer, a NaN skip falls back to the 10 s default).
pub fn media_command(action: MediaAction) -> PlayerCommand {
    match action {
        MediaAction::Play => PlayerCommand::Play,
        MediaAction::Pause => PlayerCommand::Pause,
        MediaAction::Stop => PlayerCommand::Stop,
        MediaAction::Previous => PlayerCommand::previous_sentence(),
        MediaAction::Next => PlayerCommand::next_sentence(),
        MediaAction::SeekTo(position_sec) => PlayerCommand::Seek { position_sec },
        MediaAction::SeekBy(seconds) => PlayerCommand::Skip {
            direction: if seconds < 0.0 { SkipDirection::Back } else { SkipDirection::Forward },
            unit: SkipUnit::Seconds,
            amount: Some(seconds.abs()),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use vox_core::player::{sanitize_player_command, SessionInfo};

    use super::*;

    fn snap(status: PlayerStatus, with_session: bool) -> PlayerSnapshot {
        let mut s = PlayerSnapshot::default();
        s.state.status = status;
        s.state.position_sec = 3.0;
        s.state.duration_sec = 9.0;
        s.state.rate = 1.25;
        if with_session {
            s.session = Some(Arc::new(SessionInfo {
                id: "s".into(),
                read_id: "r".into(),
                text: Arc::from("Hello"),
                title: "Hello there".into(),
                voice_name: "George".into(),
                voice_id: "v".into(),
                source_app: None,
                from_cache: false,
                highlight_words: true,
                sentences: Arc::from(vec![0u32]),
            }));
        }
        s
    }

    #[test]
    fn active_needs_keys_a_session_and_a_live_status() {
        let m = media_state(&snap(PlayerStatus::Playing, true), true);
        assert!(m.active && m.playing);
        assert_eq!((m.title.as_str(), m.artist.as_str(), m.album.as_str()), ("Hello there", "George", "Vox"));
        assert_eq!((m.position_sec, m.duration_sec, m.rate), (3.0, 9.0, 1.25));
        assert!(!media_state(&snap(PlayerStatus::Playing, true), false).active, "media keys off");
        assert!(!media_state(&snap(PlayerStatus::Error, true), true).active);
        assert!(!media_state(&snap(PlayerStatus::Idle, true), true).active, "idle (the stopped session) is inactive");
        assert!(!media_state(&snap(PlayerStatus::Idle, false), true).active);
        let ended = media_state(&snap(PlayerStatus::Ended, true), true);
        assert!(ended.active && !ended.playing, "an ended read stays listed (paused)");
        let loading = media_state(&snap(PlayerStatus::Loading, true), true);
        assert!(loading.active && !loading.playing);
        assert_eq!(media_state(&snap(PlayerStatus::Idle, false), true).title, "Vox");
    }

    #[test]
    fn smtc_seeks_are_sanitized() {
        assert_eq!(sanitize_player_command(media_command(MediaAction::SeekTo(f64::NAN))), None);
        assert_eq!(
            sanitize_player_command(media_command(MediaAction::SeekTo(-4.0))),
            Some(PlayerCommand::Seek { position_sec: 0.0 })
        );
        assert_eq!(
            sanitize_player_command(media_command(MediaAction::SeekBy(-15.0))),
            Some(PlayerCommand::Skip { direction: SkipDirection::Back, unit: SkipUnit::Seconds, amount: Some(15.0) })
        );
        assert_eq!(
            sanitize_player_command(media_command(MediaAction::SeekBy(f64::NAN))),
            Some(PlayerCommand::Skip { direction: SkipDirection::Forward, unit: SkipUnit::Seconds, amount: None })
        );
        assert_eq!(
            sanitize_player_command(media_command(MediaAction::SeekBy(1e9))),
            Some(PlayerCommand::Skip { direction: SkipDirection::Forward, unit: SkipUnit::Seconds, amount: Some(3600.0) })
        );
        assert_eq!(media_command(MediaAction::Next), PlayerCommand::next_sentence());
        assert_eq!(media_command(MediaAction::Stop), PlayerCommand::Stop);
    }
}
