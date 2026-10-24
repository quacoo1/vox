//! The offline synthesiser (tts.md §5): a quiet 220 Hz tone, 16-bit mono WAV at 22 050 Hz, `max(0.6, chars/15/speed)`
//! seconds, fades of 662 samples, evenly spaced alignment, `characterCost = UTF-16 length`.

use std::sync::Arc;

use vox_core::constants::{ESTIMATED_CHARS_PER_SECOND, MOCK_SAMPLE_RATE};
use vox_core::js::{js_round, round3};
use vox_core::text16::utf16_len;
use vox_core::tts::{Alignment, MIME_WAV};

use crate::client::SynthesisResult;
use crate::wav::encode_wav;

const TONE_HZ: f64 = 220.0;
const AMPLITUDE: f64 = 0.015;
const FADE_SEC: f64 = 0.03;
const MIN_DURATION_SEC: f64 = 0.6;

/// `speed` when finite and > 0, else 1 (`positive()` in the original).
pub(crate) fn positive_speed(speed: f64) -> f64 {
    if speed.is_finite() && speed > 0.0 {
        speed
    } else {
        1.0
    }
}

/// `mockSynthesize(text, speed)`.
pub fn mock_synthesize(text: &str, speed: f64) -> SynthesisResult {
    let rate = positive_speed(speed);
    let len16 = utf16_len(text);
    let sample_rate = f64::from(MOCK_SAMPLE_RATE);
    let target_sec = (len16 as f64 / ESTIMATED_CHARS_PER_SECOND / rate).max(MIN_DURATION_SEC);
    // Finite and ≥ 13 230 (0.6 s); the cast saturates for absurd lengths.
    let frames = js_round(target_sec * sample_rate) as usize;
    let fade = (js_round(FADE_SEC * sample_rate) as usize).min(frames / 2).max(1);
    let step = 2.0 * std::f64::consts::PI * TONE_HZ / sample_rate;
    let fade_f = fade as f64;
    let last = frames as f64 - 1.0;
    let samples: Vec<f32> = (0..frames)
        .map(|i| {
            let i = i as f64;
            let envelope = 1f64.min(i / fade_f).min((last - i) / fade_f);
            (AMPLITUDE * envelope * (step * i).sin()) as f32
        })
        .collect();
    let duration_sec = frames as f64 / sample_rate;
    SynthesisResult {
        audio: Arc::from(encode_wav(&samples, MOCK_SAMPLE_RATE)),
        mime: MIME_WAV.to_string(),
        alignment: even_alignment(text, duration_sec),
        request_id: None,
        character_cost: Some(len16 as f64),
    }
}

/// `evenAlignment(text, durationSec)`: one entry per **UTF-16 unit** (so it maps 1:1 onto the UTF-16 offsets); for
/// an astral character the first unit's entry holds the character and the second `""`. `round3` times; the last
/// end is `round3(duration)`. Empty text → `None`.
pub fn even_alignment(text: &str, duration_sec: f64) -> Option<Alignment> {
    if text.is_empty() {
        return None;
    }
    let mut chars: Vec<String> = Vec::with_capacity(text.len());
    for c in text.chars() {
        chars.push(c.to_string());
        if c.len_utf16() == 2 {
            chars.push(String::new());
        }
    }
    let n = chars.len();
    let per = duration_sec / n as f64;
    let starts = (0..n).map(|i| round3(i as f64 * per)).collect();
    let ends = (0..n).map(|i| if i == n - 1 { round3(duration_sec) } else { round3((i + 1) as f64 * per) }).collect();
    Some(Alignment { chars, starts, ends })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wav::read_wav_info;

    #[test]
    fn hi_example() {
        let r = mock_synthesize("Hi.", 1.0);
        assert_eq!(r.audio.len(), 26_504);
        assert_eq!(r.mime, "audio/wav");
        assert_eq!(r.character_cost, Some(3.0));
        assert_eq!(r.request_id, None);
        let info = read_wav_info(&r.audio).unwrap();
        assert_eq!(info.data_bytes / 2, 13_230);
        let a = r.alignment.unwrap();
        assert_eq!(a.chars, ["H", "i", "."]);
        assert_eq!(a.starts, [0.0, 0.2, 0.4]);
        assert_eq!(a.ends, [0.2, 0.4, 0.6]);
    }

    #[test]
    fn length_follows_speed() {
        let r = mock_synthesize(&"x".repeat(30), 2.0);
        assert_eq!(r.audio.len(), 44_144);
        assert_eq!(read_wav_info(&r.audio).unwrap().duration_sec, 1.0);
        assert_eq!(r.alignment.as_ref().unwrap().ends.last(), Some(&1.0));
        // Non-finite / non-positive speeds count as 1.
        for speed in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(mock_synthesize(&"x".repeat(30), speed).audio.len(), 44 + 2 * 44_100, "{speed}");
        }
        // Empty text: minimum length, no alignment, cost 0.
        let r = mock_synthesize("", 1.0);
        assert_eq!(r.audio.len(), 26_504);
        assert!(r.alignment.is_none());
        assert_eq!(r.character_cost, Some(0.0));
    }

    #[test]
    fn samples_are_quiet_and_faded() {
        let r = mock_synthesize("Hi.", 1.0);
        let samples: Vec<i16> = r.audio[44..].chunks(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(samples[0], 0);
        assert_eq!(*samples.last().unwrap(), 0);
        let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!(peak > 400 && peak <= 492, "{peak}"); // 0.015 × 32767 ≈ 491.5
        // During the 662-sample fade-in the envelope is below 1.
        let early_peak = samples[..100].iter().map(|s| s.unsigned_abs()).max().unwrap();
        assert!(early_peak < 100, "{early_peak}");
    }

    #[test]
    fn alignment_is_per_utf16_unit() {
        let a = even_alignment("a😀b", 0.8).unwrap();
        assert_eq!(a.chars, ["a", "😀", "", "b"]);
        assert_eq!(a.starts, [0.0, 0.2, 0.4, 0.6]);
        assert_eq!(a.ends, [0.2, 0.4, 0.6, 0.8]);
        assert!(even_alignment("", 1.0).is_none());
        // round3 with JS rounding; the last end is the exact (rounded) duration.
        let a = even_alignment("abc", 1.0).unwrap();
        assert_eq!(a.starts, [0.0, 0.333, 0.667]);
        assert_eq!(a.ends, [0.333, 0.667, 1.0]);
        let r = mock_synthesize("a😀b", 1.0);
        assert_eq!(r.character_cost, Some(4.0));
        assert_eq!(r.alignment.unwrap().chars.len(), 4);
    }
}
