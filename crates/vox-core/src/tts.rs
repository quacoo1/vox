//! The TTS stream contract between the engine (vox-tts), the cache (vox-store) and the player (vox-audio)
//! (contracts.md §3.4, tts.md §1.3, reader-playback.md §1.1).
//!
//! In-process, so audio is shared (`Arc<[u8]>`) instead of copied, and two fields were added that the Electron
//! payloads did not carry: [`TtsSessionStart::voice_id`] (the widget's art is seeded by the voice id) and
//! [`TtsSessionStart::sentences`] (sentence starts computed once by the engine with vox-text, so the player does
//! not depend on vox-text). All offsets are UTF-16 code units into [`TtsSessionStart::text`].

use std::sync::Arc;

use serde::Serialize;

use crate::json::js_f64_vec;
use crate::read::ReadErrorCode;

/// Encoded audio bytes (a complete MP3 or WAV file for one chunk), shared between the sink and the cache.
pub type AudioBytes = Arc<[u8]>;

pub const MIME_MPEG: &str = "audio/mpeg";
pub const MIME_WAV: &str = "audio/wav";

/// Character timing for one chunk; seconds relative to the chunk's audio start. ElevenLabs counts code points, so
/// `chars.len()` may differ from the chunk's UTF-16 range; the player maps proportionally ([`crate::timing`]).
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct Alignment {
    pub chars: Vec<String>,
    #[serde(serialize_with = "js_f64_vec")]
    pub starts: Vec<f64>,
    #[serde(serialize_with = "js_f64_vec")]
    pub ends: Vec<f64>,
}

impl Alignment {
    /// All three arrays have equal length.
    pub fn is_consistent(&self) -> bool {
        self.chars.len() == self.starts.len() && self.starts.len() == self.ends.len()
    }
    /// `max(0, max(ends))`.
    pub fn last_end(&self) -> f64 {
        self.ends.iter().copied().filter(|x| x.is_finite()).fold(0.0, f64::max)
    }
    /// Same timing with every char replaced by `""` (preview-only records keep no text next to the audio).
    pub fn redacted(&self) -> Alignment {
        Alignment { chars: vec![String::new(); self.chars.len()], starts: self.starts.clone(), ends: self.ends.clone() }
    }
    /// Some char is non-empty.
    pub fn has_text(&self) -> bool {
        self.chars.iter().any(|c| !c.is_empty())
    }
    /// The player's validity rule: non-empty starts, all finite.
    pub fn usable_for_timing(&self) -> bool {
        !self.starts.is_empty() && self.starts.iter().all(|x| x.is_finite())
    }
}

/// A `[start, end)` range in UTF-16 units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct TextSpan {
    pub start: u32,
    pub end: u32,
}

impl TextSpan {
    pub fn new(start: u32, end: u32) -> Self {
        Self { start, end }
    }
    pub fn len(&self) -> u32 {
        self.end.saturating_sub(self.start)
    }
    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }
}

/// A synthesis chunk: `text == input[start..end)` in UTF-16 units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextChunk {
    pub text: String,
    pub start: u32,
    pub end: u32,
}

/// A new session for the player; any previous session is superseded.
#[derive(Debug, Clone, PartialEq)]
pub struct TtsSessionStart {
    /// UUID v4 per session (not per read).
    pub session_id: String,
    pub read_id: String,
    /// Full cleaned text being read (preview + `…` placeholders for preview-only cached replays).
    pub text: Arc<str>,
    /// `previewTitle(text)`.
    pub title: String,
    pub voice_name: String,
    /// Voice id (art seed). May be empty for old records.
    pub voice_id: String,
    pub source_app: Option<String>,
    /// Fresh read: `utf16_len(text) / 15 / speed`; cached: Σ chunk durations.
    pub estimated_duration_sec: f64,
    pub chunk_count: u32,
    /// `settings.playbackRate` at start.
    pub rate: f64,
    /// `settings.volume` at start.
    pub volume: f64,
    pub highlight_words: bool,
    pub from_cache: bool,
    /// Sentence starts (UTF-16, ascending, never empty) from `vox_text::sentence_starts(text)`.
    pub sentences: Arc<[u32]>,
}

/// One synthesized chunk, delivered in index order 0, 1, 2…
#[derive(Debug, Clone, PartialEq)]
pub struct TtsChunk {
    pub session_id: String,
    pub index: u32,
    pub text_start: u32,
    pub text_end: u32,
    pub audio: AudioBytes,
    /// `audio/mpeg` or `audio/wav`.
    pub mime: String,
    pub alignment: Option<Arc<Alignment>>,
    /// Best known duration (alignment / byte size / estimate).
    pub duration_sec: f64,
}

/// Every chunk delivered (also after a non-fatal failure, with the number actually delivered).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TtsSessionDone {
    pub session_id: String,
    pub total_chunks: u32,
}

/// `fatal`: session aborted before any audio. Non-fatal: a later chunk failed; what arrived still plays.
#[derive(Debug, Clone, PartialEq)]
pub struct TtsSessionError {
    pub session_id: String,
    pub message: String,
    pub code: ReadErrorCode,
    pub fatal: bool,
}

/// A chunk as kept in the audio cache and replayed for free.
#[derive(Debug, Clone, PartialEq)]
pub struct CachedChunk {
    pub index: u32,
    pub text_start: u32,
    pub text_end: u32,
    pub mime: String,
    pub alignment: Option<Alignment>,
    pub duration_sec: f64,
    pub audio: AudioBytes,
}

/// Chunk file extension from a MIME (case-insensitive substring): mpeg|mp3 → mp3, wav|wave → wav, ogg|opus → ogg,
/// else bin.
pub fn ext_for_mime(mime: &str) -> &'static str {
    let m = mime.to_ascii_lowercase();
    if m.contains("mpeg") || m.contains("mp3") {
        "mp3"
    } else if m.contains("wav") {
        "wav"
    } else if m.contains("ogg") || m.contains("opus") {
        "ogg"
    } else {
        "bin"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alignment_helpers() {
        let a = Alignment { chars: vec!["H".into(), "i".into()], starts: vec![0.0, 0.2], ends: vec![0.2, 0.4] };
        assert!(a.is_consistent());
        assert_eq!(a.last_end(), 0.4);
        assert!(!a.redacted().has_text());
        assert_eq!(crate::json::to_js_compact(&a), r#"{"chars":["H","i"],"starts":[0,0.2],"ends":[0.2,0.4]}"#);
        assert_eq!(ext_for_mime("audio/MPEG"), "mp3");
        assert_eq!(ext_for_mime("audio/wave"), "wav");
        assert_eq!(ext_for_mime("audio/opus"), "ogg");
        assert_eq!(ext_for_mime("x"), "bin");
    }
}
