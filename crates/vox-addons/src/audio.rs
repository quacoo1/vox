//! Turning a helper's `audio` reply into the engine's [`SynthesisResult`] (docs/addons-design.md "Result building").
//! Nothing the helper says about the audio is trusted: the WAV is checked and measured here, and every word cue is
//! checked against the chunk text before it is used.

use base64::Engine as _;
use vox_core::text16::{utf16_len, utf16_offset};
use vox_core::tts::MIME_WAV;
use vox_tts::mock::even_alignment;
use vox_tts::wav::read_wav_info;
use vox_tts::windows_voices::{alignment_from_cues, WordCue};
use vox_tts::SynthesisResult;

use crate::protocol::{AudioReply, Cue, OffsetUnit};

/// The longest audio accepted for one chunk.
const MAX_SECONDS: f64 = 600.0;

/// Why audio was refused (for the log).
#[derive(Debug, Clone, PartialEq)]
pub struct BadAudio(pub String);

/// Builds the result for `text` from a helper's reply. `unit` is the offset unit the helper announced in `hello`
/// (`None` when it named one this Vox does not know); the reply's own `offsets` overrides it.
pub fn build_result(text: &str, reply: AudioReply, unit: Option<OffsetUnit>, word_timing: bool) -> Result<SynthesisResult, BadAudio> {
    if reply.format != "wav" {
        return Err(BadAudio(format!("format {:?}", reply.format)));
    }
    let wav = base64::engine::general_purpose::STANDARD.decode(reply.wav.trim()).map_err(|e| BadAudio(format!("base64: {e}")))?;
    let duration = check_wav(&wav, utf16_len(text))?;
    let unit = match reply.offsets.as_deref() {
        Some(s) => OffsetUnit::parse(s),
        None => unit,
    };
    let cues = match (word_timing, unit, &reply.words) {
        (true, Some(unit), Some(words)) => word_cues(text, words, unit, duration),
        _ => Vec::new(),
    };
    let alignment = if cues.is_empty() { even_alignment(text, duration) } else { alignment_from_cues(text, &cues, duration) };
    Ok(SynthesisResult {
        audio: wav.into(),
        mime: MIME_WAV.to_string(),
        alignment,
        request_id: None,
        // Never `None`: the engine would bill the chunk's length.
        character_cost: Some(0.0),
    })
}

/// Checks one WAV and returns its duration: PCM16 or float32 (plain or `WAVE_FORMAT_EXTENSIBLE`), 1–2 channels,
/// 8–192 kHz, a non-empty `data` chunk that fits in the file (no streaming size), at most 600 s and at most
/// 0.5 s per UTF-16 unit of text plus 10 s.
pub fn check_wav(wav: &[u8], len16: usize) -> Result<f64, BadAudio> {
    let info = read_wav_info(wav).ok_or_else(|| BadAudio("not a RIFF/WAVE file with fmt and data".into()))?;
    let pcm_ok = match (info.format, info.bits_per_sample) {
        (1, 16) | (3, 32) => true,
        // WAVE_FORMAT_EXTENSIBLE: the sub-format GUID's first two bytes are the format code.
        (0xFFFE, 16 | 32) => extensible_subformat(wav).is_some_and(|sub| (sub, info.bits_per_sample) == (1, 16) || (sub, info.bits_per_sample) == (3, 32)),
        _ => false,
    };
    if !pcm_ok {
        return Err(BadAudio(format!("format {} with {} bits", info.format, info.bits_per_sample)));
    }
    if !(1..=2).contains(&info.channels) || !(8_000..=192_000).contains(&info.sample_rate) {
        return Err(BadAudio(format!("{} channels at {} Hz", info.channels, info.sample_rate)));
    }
    // The declared data size, which read_wav_info clamps to the file: it must really fit.
    let declared = info.data_offset.checked_sub(4).and_then(|at| wav.get(at..at + 4)).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    match declared {
        Some(size) if size != u32::MAX && size as usize <= wav.len() - info.data_offset && size > 0 => {}
        _ => return Err(BadAudio("bad data size".into())),
    }
    let duration = info.duration_sec;
    if !duration.is_finite() || duration <= 0.0 || duration > MAX_SECONDS || duration > 0.5 * len16 as f64 + 10.0 {
        return Err(BadAudio(format!("{duration:.1} s of audio for {len16} units of text")));
    }
    Ok(duration)
}

/// The sub-format code of the `fmt ` chunk `read_wav_info` uses: like it, the last `fmt ` before the first `data`
/// that follows one. `None` when that chunk is too short to hold the extensible fields.
fn extensible_subformat(wav: &[u8]) -> Option<u16> {
    let mut at = 12usize;
    // Some(sub-format of the latest fmt chunk) once one was seen.
    let mut fmt: Option<Option<u16>> = None;
    while at.checked_add(8)? <= wav.len() {
        let size = u32::from_le_bytes(wav[at + 4..at + 8].try_into().ok()?) as usize;
        match &wav[at..at + 4] {
            // fmt body: 16 bytes of WAVEFORMAT, cbSize, valid bits, channel mask, then the 16-byte GUID.
            b"fmt " => fmt = Some(if size >= 40 { wav.get(at + 32..at + 34).map(|b| u16::from_le_bytes([b[0], b[1]])) } else { None }),
            b"data" if fmt.is_some() => return fmt.flatten(),
            _ => {}
        }
        at = at.checked_add(8)?.checked_add(size)?.checked_add(size % 2)?;
    }
    None
}

/// The usable cues of a `words` list as half-open UTF-16 ranges of `text`. A cue is dropped when it lacks a number,
/// its offsets are not whole numbers in range on a character boundary, `from >= to`, or its time is not finite and
/// non-negative. Times are clamped to the audio.
pub fn word_cues(text: &str, items: &[Cue], unit: OffsetUnit, duration: f64) -> Vec<WordCue> {
    // UTF-16 offset of every code point boundary, for the code point unit.
    let codepoints: Vec<usize> = match unit {
        OffsetUnit::Codepoint => {
            let mut v = Vec::with_capacity(text.len() + 1);
            let mut at = 0;
            v.push(0);
            for c in text.chars() {
                at += c.len_utf16();
                v.push(at);
            }
            v
        }
        _ => Vec::new(),
    };
    let len16 = utf16_len(text);
    let to16 = |n: f64| -> Option<usize> {
        if !(n.is_finite() && n >= 0.0 && n.fract() == 0.0 && n <= usize::MAX as f64) {
            return None;
        }
        let n = n as usize;
        match unit {
            OffsetUnit::Utf8 => text.is_char_boundary(n).then(|| utf16_offset(text, n)),
            OffsetUnit::Codepoint => codepoints.get(n).copied(),
            OffsetUnit::Utf16 => (n <= len16).then_some(n),
        }
    };
    let mut out = Vec::with_capacity(items.len().min(len16));
    for w in items.iter().take(len16.max(1) * 2) {
        let (Some(from), Some(to), Some(start)) = (w.from, w.to, w.start) else { continue };
        let (Some(from), Some(to)) = (to16(from), to16(to)) else { continue };
        if from >= to || !start.is_finite() || start < 0.0 {
            continue;
        }
        out.push(WordCue { start_sec: start.min(duration), from, to });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use vox_core::tts::Alignment;
    use vox_tts::wav::encode_wav;

    use crate::protocol::words_from_json;

    /// One entry per UTF-16 unit, which is what the engine expects.
    fn is_per_unit(text: &str, a: &Alignment) -> bool {
        a.chars.len() == utf16_len(text) && a.starts.len() == a.chars.len() && a.ends.len() == a.chars.len()
    }

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn reply(wav: &[u8], words: Option<Value>) -> AudioReply {
        AudioReply { format: "wav".into(), wav: b64(wav), offsets: None, words: words.map(words_from_json) }
    }

    fn tone(seconds: f64) -> Vec<u8> {
        encode_wav(&vec![0.1; (24_000.0 * seconds) as usize], 24_000)
    }

    #[test]
    fn a_good_reply_becomes_a_free_wav_chunk_with_word_timing() {
        let text = "héllo 👋 wörld";
        // UTF-8 byte offsets: "héllo" 0..6, "👋" 7..11, "wörld" 12..18.
        let words = serde_json::json!([{"from":0,"to":6,"start":0.0},{"from":7,"to":11,"start":0.4},{"from":12,"to":18,"start":0.8}]);
        let r = build_result(text, reply(&tone(1.2), Some(words)), Some(OffsetUnit::Utf8), true).unwrap();
        assert_eq!(r.mime, MIME_WAV);
        assert_eq!(r.character_cost, Some(0.0));
        let a = r.alignment.unwrap();
        assert!(is_per_unit(text, &a));
        assert_eq!(a.starts[0], 0.0);
        // "👋" starts at UTF-16 unit 6 (h é l l o space), and its second unit is the empty entry.
        assert_eq!(a.chars[6], "👋");
        assert_eq!(a.chars[7], "");
        assert_eq!(a.starts[6], 0.4);
        assert_eq!(a.starts[9], 0.8);
        assert_eq!(*a.ends.last().unwrap(), 1.2);
    }

    #[test]
    fn the_three_offset_units_agree() {
        let text = "héllo 👋 wörld";
        let cues = |unit, words: Value| word_cues(text, &words_from_json(words), unit, 2.0);
        let utf8 = cues(OffsetUnit::Utf8, serde_json::json!([{"from":0,"to":6,"start":0},{"from":7,"to":11,"start":0.5},{"from":12,"to":18,"start":1}]));
        let cp = cues(OffsetUnit::Codepoint, serde_json::json!([{"from":0,"to":5,"start":0},{"from":6,"to":7,"start":0.5},{"from":8,"to":13,"start":1}]));
        let u16 = cues(OffsetUnit::Utf16, serde_json::json!([{"from":0,"to":5,"start":0},{"from":6,"to":8,"start":0.5},{"from":9,"to":14,"start":1}]));
        assert_eq!(utf8.len(), 3);
        assert_eq!(utf8, cp);
        assert_eq!(utf8, u16);
        assert_eq!((utf8[1].from, utf8[1].to), (6, 8));
    }

    #[test]
    fn bad_cues_are_dropped_and_the_timing_falls_back_to_even() {
        let text = "héllo wörld";
        let bad = serde_json::json!([
            {"from": 2, "to": 4, "start": 0.1},       // mid-character (inside é)
            {"from": 6, "to": 3, "start": 0.2},       // reversed
            {"from": 0, "to": 99, "start": 0.3},      // out of range
            {"from": 0.5, "to": 3, "start": 0.3},     // not a whole number
            {"from": 0, "to": 3, "start": -1},        // negative time
            {"from": 0, "to": 3},                     // no time
            "nonsense"
        ]);
        assert!(word_cues(text, &words_from_json(bad.clone()), OffsetUnit::Utf8, 1.0).is_empty());
        let r = build_result(text, reply(&tone(1.0), Some(bad)), Some(OffsetUnit::Utf8), true).unwrap();
        assert_eq!(r.alignment, even_alignment(text, 1.0));
        // No words, word timing off, an unknown unit, or words that are not a list: even timing too.
        for (words, timing, unit) in [
            (None, true, Some(OffsetUnit::Utf8)),
            (Some(serde_json::json!([{"from":0,"to":5,"start":0}])), false, Some(OffsetUnit::Utf8)),
            (Some(serde_json::json!([{"from":0,"to":5,"start":0}])), true, None),
            (Some(serde_json::json!({"from":0})), true, Some(OffsetUnit::Utf8)),
        ] {
            let r = build_result(text, reply(&tone(1.0), words), unit, timing).unwrap();
            assert_eq!(r.alignment, even_alignment(text, 1.0));
        }
        // A time past the end is clamped to the audio.
        let late = word_cues("hi", &words_from_json(serde_json::json!([{"from":0,"to":2,"start":9}])), OffsetUnit::Utf8, 1.0);
        assert_eq!(late[0].start_sec, 1.0);
    }

    #[test]
    fn the_reply_unit_overrides_hello() {
        let text = "é a";
        let mut r = reply(&tone(1.0), Some(serde_json::json!([{"from":2,"to":3,"start":0.5}])));
        r.offsets = Some("utf16".into());
        // As UTF-8 bytes 2..3 would be " "; as UTF-16 units 2..3 it is "a".
        let a = build_result(text, r, Some(OffsetUnit::Utf8), true).unwrap().alignment.unwrap();
        assert_eq!(a.starts[2], 0.5);
    }

    fn with_u32(mut wav: Vec<u8>, at: usize, v: u32) -> Vec<u8> {
        wav[at..at + 4].copy_from_slice(&v.to_le_bytes());
        wav
    }

    #[test]
    fn bad_audio_is_refused() {
        let good = tone(1.0);
        assert!(check_wav(&good, 10).is_ok());
        let mut not_riff = good.clone();
        not_riff[0] = b'X';
        let mut eight_bit = good.clone();
        eight_bit[34] = 8; // bits per sample
        let mut mono_rate = good.clone();
        mono_rate[24..28].copy_from_slice(&4_000u32.to_le_bytes());
        let mut three_channels = good.clone();
        three_channels[22] = 3;
        for (name, wav) in [
            ("not RIFF", not_riff),
            ("streaming size", with_u32(good.clone(), 40, u32::MAX)),
            ("size past the end", with_u32(good.clone(), 40, good.len() as u32)),
            ("empty data", encode_wav(&[], 24_000)),
            ("8-bit", eight_bit),
            ("4 kHz", mono_rate),
            ("3 channels", three_channels),
            ("truncated", good[..30].to_vec()),
        ] {
            assert!(check_wav(&wav, 10).is_err(), "{name}");
        }
        // Too long for the text: 0.5 s per unit + 10 s.
        assert!(check_wav(&tone(12.0), 2).is_err());
        assert!(check_wav(&tone(10.5), 2).is_ok());
        // Not base64, not wav.
        assert!(build_result("hi", AudioReply { format: "wav".into(), wav: "%%%".into(), offsets: None, words: None }, None, false).is_err());
        assert!(build_result("hi", AudioReply { format: "mp3".into(), wav: b64(&good), offsets: None, words: None }, None, false).is_err());
    }

    #[test]
    fn float_and_extensible_wavs_are_accepted() {
        let rate = 24_000u32;
        let samples = 2_400usize;
        let fmt = |format: u16, bits: u16, extensible: Option<u16>| {
            let mut f = Vec::new();
            f.extend_from_slice(&format.to_le_bytes());
            f.extend_from_slice(&1u16.to_le_bytes());
            f.extend_from_slice(&rate.to_le_bytes());
            f.extend_from_slice(&(rate * u32::from(bits) / 8).to_le_bytes());
            f.extend_from_slice(&(bits / 8).to_le_bytes());
            f.extend_from_slice(&bits.to_le_bytes());
            if let Some(sub) = extensible {
                f.extend_from_slice(&22u16.to_le_bytes());
                f.extend_from_slice(&bits.to_le_bytes());
                f.extend_from_slice(&4u32.to_le_bytes());
                f.extend_from_slice(&sub.to_le_bytes());
                f.extend_from_slice(&[0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xAA, 0, 0x38, 0x9B, 0x71]);
            }
            f
        };
        let file = |fmt: Vec<u8>, bits: u16| {
            let data = vec![0u8; samples * usize::from(bits / 8)];
            let mut w = b"RIFF".to_vec();
            w.extend_from_slice(&((4 + 8 + fmt.len() + 8 + data.len()) as u32).to_le_bytes());
            w.extend_from_slice(b"WAVEfmt ");
            w.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
            w.extend_from_slice(&fmt);
            w.extend_from_slice(b"data");
            w.extend_from_slice(&(data.len() as u32).to_le_bytes());
            w.extend_from_slice(&data);
            w
        };
        assert!(check_wav(&file(fmt(3, 32, None), 32), 10).is_ok());
        assert!(check_wav(&file(fmt(0xFFFE, 16, Some(1)), 16), 10).is_ok());
        assert!(check_wav(&file(fmt(0xFFFE, 32, Some(3)), 32), 10).is_ok());
        // Extensible with a sub-format that is not PCM16 / float32.
        assert!(check_wav(&file(fmt(0xFFFE, 32, Some(1)), 32), 10).is_err());
        // Two fmt chunks: the last one counts (as in read_wav_info), so a PCM GUID first cannot vouch for ADPCM after.
        let two = |first: Vec<u8>, second: Vec<u8>| {
            let mut w = file(first, 16);
            let data_at = w.windows(4).position(|c| c == b"data").unwrap();
            let mut chunk = b"fmt ".to_vec();
            chunk.extend_from_slice(&(second.len() as u32).to_le_bytes());
            chunk.extend_from_slice(&second);
            w.splice(data_at..data_at, chunk);
            let riff = (w.len() - 8) as u32;
            w[4..8].copy_from_slice(&riff.to_le_bytes());
            w
        };
        assert!(check_wav(&two(fmt(0xFFFE, 16, Some(1)), fmt(0xFFFE, 16, Some(2))), 10).is_err());
        assert!(check_wav(&two(fmt(0xFFFE, 16, Some(2)), fmt(0xFFFE, 16, Some(1))), 10).is_ok());
        // An extensible fmt chunk too short to hold its GUID.
        let mut short = fmt(0xFFFE, 16, Some(1));
        short.truncate(24);
        assert!(check_wav(&file(short, 16), 10).is_err());
        assert!(check_wav(&file(fmt(1, 32, None), 32), 10).is_err());
    }
}
