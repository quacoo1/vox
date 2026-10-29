//! The voices built into Windows (WinRT `Windows.Media.SpeechSynthesis`, the OneCore voices). Free, offline, always
//! available. Synthesis returns a WAV stream with word-boundary cues (`SpeechCue`: start time and the UTF-16 position
//! of the word in the input), so highlighting is exact. Voice ids are `windows:<VoiceInformation.Id>`.

use std::sync::Arc;

use vox_core::providers::Provider;
use vox_core::tts::{Alignment, MIME_WAV};
use vox_core::voices::Voice;

use crate::cancel::CancelToken;
use crate::client::SynthesisResult;
use crate::error::TtsError;
use crate::mock::even_alignment;
#[cfg_attr(not(windows), allow(unused_imports))]
use crate::providers::{voice, SpeechProvider};
use crate::wav::read_wav_info;

const NAME: &str = "Windows";

#[derive(Default)]
pub struct WindowsVoices;

impl WindowsVoices {
    pub fn new() -> Self {
        Self
    }
}

/// A word cue: start (s) and the word's UTF-16 range in the input.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WordCue {
    pub start_sec: f64,
    pub from: usize,
    pub to: usize,
}

/// Per-UTF-16-unit alignment from word cues: a word's units span from its start to the next word's start (the last
/// to `duration`); units between words take the previous end.
pub fn alignment_from_cues(text: &str, cues: &[WordCue], duration: f64) -> Option<Alignment> {
    let base = even_alignment(text, duration)?;
    if cues.is_empty() {
        return Some(base);
    }
    let total = base.chars.len();
    let mut cues: Vec<WordCue> = cues.iter().copied().filter(|c| c.from < c.to && c.to <= total).collect();
    cues.sort_by(|a, b| a.start_sec.partial_cmp(&b.start_sec).unwrap_or(std::cmp::Ordering::Equal));
    if cues.is_empty() {
        return Some(base);
    }
    let mut starts = vec![f64::NAN; total];
    let mut ends = vec![f64::NAN; total];
    for (i, c) in cues.iter().enumerate() {
        let t0 = c.start_sec.clamp(0.0, duration);
        let t1 = cues.get(i + 1).map_or(duration, |n| n.start_sec.clamp(0.0, duration)).max(t0);
        let n = (c.to - c.from) as f64;
        for (k, u) in (c.from..c.to).enumerate() {
            starts[u] = round3(t0 + (t1 - t0) * k as f64 / n);
            ends[u] = round3(t0 + (t1 - t0) * (k + 1) as f64 / n);
        }
    }
    let mut last = 0.0;
    for u in 0..total {
        if starts[u].is_nan() {
            starts[u] = last;
            ends[u] = last;
        } else {
            last = ends[u];
        }
    }
    if let Some(e) = ends.last_mut() {
        *e = round3(duration).max(*e);
    }
    Some(Alignment { chars: base.chars, starts, ends })
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

#[cfg(windows)]
mod imp {
    use windows::core::{Interface, HSTRING};
    use windows::Media::Core::SpeechCue;
    use windows::Media::SpeechSynthesis::{SpeechSynthesizer, VoiceGender};
    use windows::Storage::Streams::DataReader;
    use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

    use super::*;

    fn init() {
        // Already-initialised threads (any apartment) answer with an error we can ignore.
        unsafe {
            let _ = RoInitialize(RO_INIT_MULTITHREADED);
        }
    }

    fn fail(e: windows::core::Error) -> TtsError {
        tracing::warn!(target: "tts", "Windows speech: {e}");
        TtsError::new(vox_core::read::ReadErrorCode::Unknown, "Windows couldn’t read this text with that voice.")
    }

    pub fn list() -> Result<Vec<Voice>, TtsError> {
        init();
        let all = SpeechSynthesizer::AllVoices().map_err(fail)?;
        let mut out = Vec::new();
        for info in all {
            let id = info.Id().map_err(fail)?.to_string();
            let name = info.DisplayName().map_err(fail)?.to_string();
            let language = info.Language().map_err(fail)?.to_string();
            let gender = match info.Gender() {
                Ok(VoiceGender::Female) => "female",
                Ok(VoiceGender::Male) => "male",
                _ => "",
            };
            let short = name.trim_start_matches("Microsoft ").split(" - ").next().unwrap_or(&name).to_string();
            out.push(voice(Provider::Windows, &id, short, vec![("gender", gender.to_string()), ("accent", language.clone()), ("language", language), ("use_case", "Offline".into())]));
        }
        out.sort_by_key(|v| v.name.to_lowercase());
        Ok(out)
    }

    pub fn synthesize(text: &str, id: &str) -> Result<(Vec<u8>, Vec<WordCue>), TtsError> {
        init();
        let synth = SpeechSynthesizer::new().map_err(fail)?;
        let all = SpeechSynthesizer::AllVoices().map_err(fail)?;
        let chosen = all.into_iter().find(|v| v.Id().map(|i| i == id).unwrap_or(false));
        let Some(chosen) = chosen else {
            return Err(TtsError::new(vox_core::read::ReadErrorCode::Unknown, "This Windows voice is no longer installed. Pick another voice."));
        };
        synth.SetVoice(&chosen).map_err(fail)?;
        synth.Options().and_then(|o| o.SetIncludeWordBoundaryMetadata(true)).map_err(fail)?;
        let stream = synth.SynthesizeTextToStreamAsync(&HSTRING::from(text)).map_err(fail)?.join().map_err(fail)?;
        let size = u32::try_from(stream.Size().map_err(fail)?).map_err(|_| TtsError::no_audio())?;
        let reader = DataReader::CreateDataReader(&stream.GetInputStreamAt(0).map_err(fail)?).map_err(fail)?;
        reader.LoadAsync(size).map_err(fail)?.join().map_err(fail)?;
        let mut audio = vec![0u8; size as usize];
        reader.ReadBytes(&mut audio).map_err(fail)?;
        let mut cues = Vec::new();
        if let Ok(tracks) = stream.TimedMetadataTracks() {
            for track in tracks {
                let Ok(list) = track.Cues() else { continue };
                for cue in list {
                    let Ok(sc) = cue.cast::<SpeechCue>() else { continue };
                    let start = sc.StartTime().map(|t| t.Duration as f64 / 10_000_000.0).unwrap_or(0.0);
                    let from = sc.StartPositionInInput().and_then(|r| r.Value()).ok();
                    let to = sc.EndPositionInInput().and_then(|r| r.Value()).ok();
                    if let (Some(from), Some(to)) = (from, to) {
                        // End positions are inclusive.
                        cues.push(WordCue { start_sec: start, from: from.max(0) as usize, to: (to.max(0) as usize) + 1 });
                    }
                }
            }
        }
        Ok((audio, cues))
    }
}

impl SpeechProvider for WindowsVoices {
    fn provider(&self) -> Provider {
        Provider::Windows
    }

    fn synthesize(&self, text: &str, rest: &str, _word_timing: bool, cancel: &CancelToken) -> Result<SynthesisResult, TtsError> {
        if cancel.is_cancelled() {
            return Err(TtsError::cancelled());
        }
        #[cfg(windows)]
        {
            let (audio, cues) = imp::synthesize(text, rest)?;
            if audio.is_empty() {
                return Err(TtsError::no_audio());
            }
            let duration = read_wav_info(&audio).map(|w| w.duration_sec).unwrap_or(0.0);
            let alignment = alignment_from_cues(text, &cues, duration);
            Ok(SynthesisResult { audio: Arc::from(audio), mime: MIME_WAV.to_string(), alignment, request_id: None, character_cost: Some(0.0) })
        }
        #[cfg(not(windows))]
        {
            let _ = (text, rest, read_wav_info, Arc::<[u8]>::from(Vec::new()), MIME_WAV);
            Err(TtsError::new(vox_core::read::ReadErrorCode::Unknown, "Windows voices need Windows."))
        }
    }

    fn list_voices(&self) -> Result<Vec<Voice>, TtsError> {
        #[cfg(windows)]
        {
            imp::list()
        }
        #[cfg(not(windows))]
        {
            Ok(Vec::new())
        }
    }

    fn summary(&self) -> String {
        match self.list_voices() {
            Ok(v) if v.len() == 1 => "1 voice installed".into(),
            Ok(v) => format!("{} voices installed", v.len()),
            Err(_) => format!("{NAME} speech is unavailable"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cues_become_alignment() {
        let text = "Hello big world";
        let cues = [WordCue { start_sec: 0.0, from: 0, to: 5 }, WordCue { start_sec: 0.5, from: 6, to: 9 }, WordCue { start_sec: 0.9, from: 10, to: 15 }];
        let a = alignment_from_cues(text, &cues, 1.4).unwrap();
        assert!(a.is_consistent());
        assert_eq!(a.starts[6], 0.5);
        assert_eq!(a.starts[10], 0.9);
        assert_eq!(*a.ends.last().unwrap(), 1.4);
        assert!(a.starts.windows(2).all(|w| w[0] <= w[1]));
    }

    /// Lists the installed voices and reads a sentence with the first one.
    /// `cargo test -p vox-tts windows_live -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn windows_live_reads_with_an_installed_voice() {
        let w = WindowsVoices::new();
        let voices = w.list_voices().unwrap();
        println!("{} voices: {:?}", voices.len(), voices.iter().map(|v| v.name.as_str()).collect::<Vec<_>>());
        let first = voices.first().expect("at least one voice");
        let (_, rest) = vox_core::providers::split_voice_id(&first.voice_id).unwrap();
        let r = w.synthesize("Hello from the voices built into Windows.", rest, true, &CancelToken::new()).unwrap();
        let a = r.alignment.unwrap();
        println!("{} bytes, ends at {:.2}s", r.audio.len(), a.last_end());
        assert!(r.audio.len() > 1000 && a.last_end() > 0.5);
    }
}
