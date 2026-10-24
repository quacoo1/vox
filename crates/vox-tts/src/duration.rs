//! `chunkDuration(result, chunk, settings)` (tts.md §4.8): the max alignment end; for CBR MP3 the byte-size duration
//! `bytes × 8 / (kbps × 1000)` when it lies within `[alignEnd, alignEnd + 2]`; for WAV the header duration; without
//! alignment the measured duration; else `chunkLen16 / 15 / speed`.

use vox_core::constants::ESTIMATED_CHARS_PER_SECOND;
use vox_core::settings::Settings;
use vox_core::tts::{MIME_MPEG, MIME_WAV};

use crate::client::SynthesisResult;
use crate::mapping::mp3_kbps;
use crate::mock::positive_speed;
use crate::wav::read_wav_info;

/// Seconds of trailing silence the byte-size duration may add after the last aligned character.
const MEASURED_TOLERANCE_SEC: f64 = 2.0;

/// Best known duration of one synthesized chunk. `chunk_len16` = the chunk's UTF-16 length.
pub fn chunk_duration(result: &SynthesisResult, chunk_len16: usize, settings: &Settings) -> f64 {
    let align_end = result.alignment.as_ref().map_or(0.0, |a| a.last_end());
    let measured = if result.mime == MIME_MPEG {
        mp3_kbps(settings.output_format.as_str()).map(|kbps| result.audio.len() as f64 * 8.0 / (f64::from(kbps) * 1000.0))
    } else if result.mime == MIME_WAV {
        read_wav_info(&result.audio).map(|w| w.duration_sec)
    } else {
        None
    };
    if align_end > 0.0 {
        return match measured {
            Some(m) if (align_end..=align_end + MEASURED_TOLERANCE_SEC).contains(&m) => m,
            _ => align_end,
        };
    }
    if let Some(m) = measured.filter(|m| *m > 0.0) {
        return m;
    }
    chunk_len16 as f64 / ESTIMATED_CHARS_PER_SECOND / positive_speed(settings.voice.speed)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use vox_core::settings::OutputFormat;
    use vox_core::tts::Alignment;

    use super::*;

    fn result(mime: &str, bytes: usize, ends: Option<Vec<f64>>) -> SynthesisResult {
        SynthesisResult {
            audio: Arc::from(vec![0u8; bytes]),
            mime: mime.into(),
            alignment: ends.map(|ends| Alignment { chars: vec![String::new(); ends.len()], starts: vec![0.0; ends.len()], ends }),
            request_id: None,
            character_cost: None,
        }
    }

    fn settings(format: OutputFormat, speed: f64) -> Settings {
        Settings {
            output_format: format,
            voice: vox_core::settings::VoiceSettings { speed, ..Default::default() },
            ..Settings::default()
        }
    }

    #[test]
    fn mp3_byte_size_is_used_within_two_seconds_after_the_alignment() {
        let s = settings(OutputFormat::Mp3_44100_128, 1.0);
        // 160 000 bytes at 128 kbps = 10 s.
        assert_eq!(chunk_duration(&result(MIME_MPEG, 160_000, Some(vec![1.0, 9.0])), 50, &s), 10.0);
        assert_eq!(chunk_duration(&result(MIME_MPEG, 160_000, Some(vec![8.0])), 50, &s), 10.0); // exactly +2
        assert_eq!(chunk_duration(&result(MIME_MPEG, 160_000, Some(vec![7.9])), 50, &s), 7.9); // too long
        assert_eq!(chunk_duration(&result(MIME_MPEG, 160_000, Some(vec![10.5])), 50, &s), 10.5); // too short
        assert_eq!(chunk_duration(&result(MIME_MPEG, 160_000, Some(vec![10.0])), 50, &s), 10.0);
        // The bitrate comes from the configured output format.
        let s32 = settings(OutputFormat::Mp3_22050_32, 1.0);
        assert_eq!(chunk_duration(&result(MIME_MPEG, 40_000, None), 50, &s32), 10.0);
        let s64 = settings(OutputFormat::Mp3_44100_64, 1.0);
        assert_eq!(chunk_duration(&result(MIME_MPEG, 80_000, None), 50, &s64), 10.0);
    }

    #[test]
    fn wav_uses_the_header_and_alignment_wins_otherwise() {
        let s = settings(OutputFormat::Mp3_44100_128, 1.0);
        let mock = crate::mock::mock_synthesize("Hi.", 1.0);
        assert_eq!(chunk_duration(&mock, 3, &s), 0.6);
        let mut no_align = mock.clone();
        no_align.alignment = None;
        assert_eq!(chunk_duration(&no_align, 3, &s), 0.6);
        // A broken WAV with alignment → the alignment end.
        assert_eq!(chunk_duration(&result(MIME_WAV, 10, Some(vec![0.25])), 3, &s), 0.25);
        // Other MIME types never measure.
        assert_eq!(chunk_duration(&result("audio/ogg", 160_000, Some(vec![1.5])), 3, &s), 1.5);
    }

    #[test]
    fn estimate_when_nothing_is_known() {
        let s = settings(OutputFormat::Mp3_44100_128, 1.0);
        assert_eq!(chunk_duration(&result("audio/ogg", 10, None), 30, &s), 2.0);
        assert_eq!(chunk_duration(&result(MIME_MPEG, 0, None), 30, &s), 2.0); // measured 0 is not used
        assert_eq!(chunk_duration(&result(MIME_MPEG, 0, Some(vec![0.0, -1.0])), 30, &s), 2.0); // alignEnd 0
        assert_eq!(chunk_duration(&result(MIME_WAV, 3, None), 30, &settings(OutputFormat::Mp3_44100_128, 2.0)), 1.0);
        // The unclamped speed is used (a hand-edited 2.0 halves the estimate); invalid → 1.
        assert_eq!(chunk_duration(&result("x", 0, None), 30, &settings(OutputFormat::Mp3_44100_128, 0.0)), 2.0);
        assert_eq!(chunk_duration(&result("x", 0, None), 30, &settings(OutputFormat::Mp3_44100_128, f64::NAN)), 2.0);
    }
}
