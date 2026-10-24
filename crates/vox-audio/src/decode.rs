//! Decoding chunk files to PCM: MP3 via symphonia (ElevenLabs output), WAV (mock), by MIME with content sniffing as a
//! fallback. A chunk that fails to decode is marked failed (it keeps its timeline slot and playback skips it).
//!
//! Gapless (ARCHITECTURE.md §5.3): symphonia's gapless mode trims the MP3 encoder delay and padding announced by the
//! Xing/`Info` + LAME tag. Verified on 2026-10-17 against 464 real ElevenLabs `mp3_44100_128` chunks from an Electron
//! profile's audio cache: every file starts with an ID3v2 tag followed by a CBR `Info` frame whose LAME extension is
//! written by `Lavf` with encoder delay 576 and a padding of 576–1656 samples; symphonia therefore trims
//! 528 + 1 + 576 = 1105 frames at the start and `padding − 529` at the end (see the `elevenlabs_style_info_tag_*`
//! tests, which rebuild that exact tag layout). MP3s without the tag fall back to dropping the decoder + LAME delay:
//! 1105 frames at MPEG-1 rates (32–48 kHz), 529 at the lower MPEG-2/2.5 rates.

use std::io::Cursor;

use symphonia::core::codecs::audio::well_known::CODEC_ID_MP3;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;

/// Interleaved f32 PCM.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedAudio {
    pub sample_rate: u32,
    pub channels: u16,
    pub samples: Vec<f32>,
}

impl DecodedAudio {
    pub fn frames(&self) -> usize {
        if self.channels == 0 {
            0
        } else {
            self.samples.len() / self.channels as usize
        }
    }
    /// Decoded duration (adopted when it differs from the engine's by > 0.01 s, §4.7 `onChunkMeta`).
    pub fn duration_sec(&self) -> f64 {
        if self.sample_rate == 0 {
            0.0
        } else {
            self.frames() as f64 / self.sample_rate as f64
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("unsupported audio: {0}")]
    Unsupported(String),
    #[error("corrupt audio: {0}")]
    Corrupt(String),
}

/// Frames to drop from the start of an MP3 without a LAME/Info tag (decoder delay 529 + LAME's 576 at MPEG-1
/// rates; the architecture's 529 at the lower rates).
pub fn untagged_mp3_delay(sample_rate: u32) -> usize {
    if sample_rate >= 32_000 {
        1105
    } else {
        529
    }
}

/// The container hint for a MIME (case-insensitive substring, like `ext_for_mime`).
fn hint_for(mime: &str) -> Option<Hint> {
    let m = mime.to_ascii_lowercase();
    let mut hint = Hint::new();
    if m.contains("mpeg") || m.contains("mp3") {
        hint.with_extension("mp3").mime_type("audio/mpeg");
        Some(hint)
    } else if m.contains("wav") {
        hint.with_extension("wav").mime_type("audio/wav");
        Some(hint)
    } else {
        None
    }
}

/// Decodes one complete file.
pub fn decode(bytes: &[u8], mime: &str) -> Result<DecodedAudio, DecodeError> {
    if bytes.is_empty() {
        return Err(DecodeError::Corrupt("empty file".into()));
    }
    match hint_for(mime) {
        Some(hint) => match decode_with(bytes, &hint) {
            Ok(audio) => Ok(audio),
            // The MIME may be wrong (e.g. a WAV labelled audio/mpeg): sniff the content instead.
            Err(first) => decode_with(bytes, &Hint::new()).map_err(|_| first),
        },
        None => decode_with(bytes, &Hint::new()),
    }
}

fn decode_with(bytes: &[u8], hint: &Hint) -> Result<DecodedAudio, DecodeError> {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(bytes)), MediaSourceStreamOptions::default());
    let mut format = symphonia::default::get_probe()
        .probe(hint, mss, FormatOptions::default(), MetadataOptions::default())
        .map_err(|e| match e {
            SymError::Unsupported(what) => DecodeError::Unsupported(what.to_string()),
            other => DecodeError::Corrupt(other.to_string()),
        })?;
    let track =
        format.default_track(TrackType::Audio).ok_or_else(|| DecodeError::Unsupported("no audio track".into()))?;
    let track_id = track.id;
    let delay_known = track.delay.is_some();
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .cloned()
        .ok_or_else(|| DecodeError::Unsupported("not an audio track".into()))?;
    let is_mp3 = params.codec == CODEC_ID_MP3;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &AudioDecoderOptions::default().gapless(true))
        .map_err(|e| DecodeError::Unsupported(e.to_string()))?;

    let mut samples: Vec<f32> = Vec::new();
    let mut scratch: Vec<f32> = Vec::new();
    let mut spec: Option<(u32, u16)> = None;
    let mut bad_packets = 0usize;
    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(SymError::IoError(_)) => break, // truncated file: keep what decoded
            Err(SymError::ResetRequired) => break,
            Err(e) => {
                if samples.is_empty() {
                    return Err(DecodeError::Corrupt(e.to_string()));
                }
                break;
            }
        };
        if packet.track_id != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(buf) => {
                let channels = buf.spec().channels().count().max(1) as u16;
                let rate = buf.spec().rate();
                match spec {
                    None => spec = Some((rate, channels)),
                    // A mid-stream format change: not expected from TTS output; keep the first format.
                    Some((r, c)) if r != rate || c != channels => {
                        bad_packets += 1;
                        continue;
                    }
                    Some(_) => {}
                }
                let n = buf.samples_interleaved();
                if n == 0 {
                    continue;
                }
                scratch.resize(n, 0.0);
                buf.copy_to_slice_interleaved(&mut scratch[..]);
                samples.extend_from_slice(&scratch);
            }
            Err(SymError::DecodeError(_)) | Err(SymError::IoError(_)) => bad_packets += 1,
            Err(SymError::ResetRequired) => break,
            Err(e) => {
                if samples.is_empty() {
                    return Err(DecodeError::Corrupt(e.to_string()));
                }
                break;
            }
        }
    }
    let Some((sample_rate, channels)) = spec else {
        return Err(DecodeError::Corrupt(format!("no decodable audio ({bad_packets} bad packets)")));
    };
    if sample_rate == 0 {
        return Err(DecodeError::Corrupt("zero sample rate".into()));
    }
    if is_mp3 && !delay_known {
        let drop = untagged_mp3_delay(sample_rate) * channels as usize;
        samples.drain(..drop.min(samples.len()));
    }
    // Decoders never produce non-finite samples for valid input; make sure nothing odd reaches the device.
    samples.iter_mut().filter(|s| !s.is_finite()).for_each(|s| *s = 0.0);
    if samples.is_empty() {
        return Err(DecodeError::Corrupt("no audio frames".into()));
    }
    Ok(DecodedAudio { sample_rate, channels, samples })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::f64::consts::PI;

    /// A 16-bit PCM WAV like vox-tts's mock (`tts.md` §5): 220 Hz sine, amplitude 0.015, 30 ms linear fades.
    pub(crate) fn mock_wav(sample_rate: u32, frames: usize) -> Vec<u8> {
        let fade = (sample_rate as f64 * 0.03).round() as usize;
        let mut pcm = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let mut a = 0.015 * (2.0 * PI * 220.0 * i as f64 / sample_rate as f64).sin();
            if i < fade {
                a *= i as f64 / fade as f64;
            }
            if frames - i <= fade {
                a *= (frames - i) as f64 / fade as f64;
            }
            let s = (a * 32767.0).round().clamp(-32768.0, 32767.0) as i16;
            pcm.extend_from_slice(&s.to_le_bytes());
        }
        wav_bytes(sample_rate, 1, &pcm)
    }

    pub(crate) fn wav_bytes(sample_rate: u32, channels: u16, pcm16: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(44 + pcm16.len());
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + pcm16.len() as u32).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&channels.to_le_bytes());
        v.extend_from_slice(&sample_rate.to_le_bytes());
        v.extend_from_slice(&(sample_rate * channels as u32 * 2).to_le_bytes());
        v.extend_from_slice(&(channels * 2).to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&(pcm16.len() as u32).to_le_bytes());
        v.extend_from_slice(pcm16);
        v
    }

    /// A procedurally generated MPEG-1 Layer III stream (44.1 kHz mono, 32 kbps CBR): every granule carries one
    /// spectral line (index 5 of the long-block spectrum, ≈ 210 Hz). Optionally preceded by an ID3v2 tag and an
    /// ElevenLabs-style `Info` frame with a `Lavf` LAME extension (encoder delay 576, padding `pad`).
    pub(crate) mod mp3gen {
        pub const FRAME_BYTES: usize = 104; // 144 × 32000 / 44100, no padding slot
        const HEADER: [u8; 4] = [0xFF, 0xFB, 0x10, 0xC4];
        const SIDE_INFO_BYTES: usize = 17;

        struct Bits {
            out: Vec<u8>,
            acc: u64,
            n: u32,
        }

        impl Bits {
            fn new() -> Self {
                Self { out: Vec::new(), acc: 0, n: 0 }
            }
            fn put(&mut self, value: u32, bits: u32) {
                for i in (0..bits).rev() {
                    self.acc = (self.acc << 1) | ((value >> i) & 1) as u64;
                    self.n += 1;
                    if self.n == 8 {
                        self.out.push(self.acc as u8);
                        self.acc = 0;
                        self.n = 0;
                    }
                }
            }
            fn finish(mut self) -> Vec<u8> {
                if self.n > 0 {
                    let pad = 8 - self.n;
                    self.put(0, pad);
                }
                self.out
            }
        }

        /// Huffman table 1 codes for the three pairs (0,0) (0,0) (0,1)+sign: lines 0..5, line 5 = +1.
        fn granule_main_data(bits: &mut Bits) {
            bits.put(0b1, 1);
            bits.put(0b1, 1);
            bits.put(0b001, 3);
            bits.put(0, 1); // sign: positive
        }
        const GRANULE_BITS: u32 = 6;

        fn data_frame(global_gain: u32) -> Vec<u8> {
            let mut side = Bits::new();
            side.put(0, 9); // main_data_begin
            side.put(0, 5); // private bits (mono)
            side.put(0, 4); // scfsi
            for _ in 0..2 {
                side.put(GRANULE_BITS, 12); // part2_3_length
                side.put(3, 9); // big_values (3 pairs)
                side.put(global_gain, 8);
                side.put(0, 4); // scalefac_compress (no scale factors)
                side.put(0, 1); // window_switching_flag
                for _ in 0..3 {
                    side.put(1, 5); // table_select = Huffman table 1
                }
                side.put(0, 4); // region0_count
                side.put(0, 3); // region1_count
                side.put(0, 1); // preflag
                side.put(0, 1); // scalefac_scale
                side.put(0, 1); // count1table_select
            }
            let side = side.finish();
            assert_eq!(side.len(), SIDE_INFO_BYTES);
            let mut main = Bits::new();
            granule_main_data(&mut main);
            granule_main_data(&mut main);
            let main = main.finish();
            let mut frame = Vec::with_capacity(FRAME_BYTES);
            frame.extend_from_slice(&HEADER);
            frame.extend_from_slice(&side);
            frame.extend_from_slice(&main);
            frame.resize(FRAME_BYTES, 0);
            frame
        }

        fn info_frame(data_frames: u32, delay: u32, padding: u32) -> Vec<u8> {
            let mut f = Vec::with_capacity(FRAME_BYTES);
            f.extend_from_slice(&HEADER);
            f.extend_from_slice(&[0; SIDE_INFO_BYTES]);
            f.extend_from_slice(b"Info");
            f.extend_from_slice(&1u32.to_be_bytes()); // flags: frame count present
            f.extend_from_slice(&data_frames.to_be_bytes());
            f.extend_from_slice(b"Lavf58.76"); // encoder (as in ElevenLabs files)
            f.push(0); // revision / VBR method
            f.push(0); // lowpass
            f.extend_from_slice(&[0; 4]); // peak
            f.extend_from_slice(&[0; 2]); // radio replay gain
            f.extend_from_slice(&[0; 2]); // audiophile replay gain
            f.push(0); // encoding flags
            f.push(0); // ABR bitrate
            let trim = (delay << 12) | (padding & 0xFFF);
            f.extend_from_slice(&trim.to_be_bytes()[1..]);
            f.resize(FRAME_BYTES, 0);
            f
        }

        fn id3v2() -> Vec<u8> {
            // ID3v2.4 header with 16 bytes of padding (ElevenLabs files start with an ID3 tag).
            let mut v = b"ID3\x04\x00\x00".to_vec();
            v.extend_from_slice(&[0, 0, 0, 16]);
            v.extend_from_slice(&[0; 16]);
            v
        }

        /// The fixture: ID3 + Info (delay 576, padding 1000) + `frames` data frames.
        pub fn build(frames: u32, with_info: bool) -> Vec<u8> {
            let mut out = id3v2();
            if with_info {
                out.extend_from_slice(&info_frame(frames, 576, 1000));
            }
            let frame = data_frame(GLOBAL_GAIN);
            for _ in 0..frames {
                out.extend_from_slice(&frame);
            }
            out
        }

        pub const GLOBAL_GAIN: u32 = 202;
        pub const FIXTURE_FRAMES: u32 = 40;
    }

    /// The committed fixture (`tests/fixtures/tone-44k-mono.mp3`, CC0, see `tests/fixtures/README.md`).
    const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/tone-44k-mono.mp3");

    /// Dominant frequency by a coarse DFT scan (10 Hz steps over 50..1000 Hz).
    fn dominant_freq(x: &[f32], sr: u32) -> f64 {
        let mut best = (0.0, 0.0);
        let mut f = 50.0;
        while f <= 1000.0 {
            let (mut re, mut im) = (0.0, 0.0);
            for (i, s) in x.iter().enumerate() {
                let ph = 2.0 * PI * f * i as f64 / sr as f64;
                re += *s as f64 * ph.cos();
                im += *s as f64 * ph.sin();
            }
            let p = re * re + im * im;
            if p > best.1 {
                best = (f, p);
            }
            f += 10.0;
        }
        best.0
    }

    #[test]
    fn fixture_matches_its_generator() {
        assert_eq!(FIXTURE, &mp3gen::build(mp3gen::FIXTURE_FRAMES, true)[..]);
    }

    /// Regenerates the committed fixture: `cargo test -p vox-audio -- --ignored regenerate_mp3_fixture`.
    #[test]
    #[ignore]
    fn regenerate_mp3_fixture() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tone-44k-mono.mp3");
        std::fs::write(path, mp3gen::build(mp3gen::FIXTURE_FRAMES, true)).expect("write fixture");
    }

    #[test]
    fn decodes_the_mp3_fixture_with_gapless_trimming() {
        let audio = decode(FIXTURE, "audio/mpeg").expect("decode");
        assert_eq!(audio.sample_rate, 44_100);
        assert_eq!(audio.channels, 1);
        // 40 × 1152 decoded frames − (529 + 576) delay − (1000 − 529) padding.
        assert_eq!(audio.frames(), 40 * 1152 - 1105 - 471);
        assert!((audio.duration_sec() - 44_504.0 / 44_100.0).abs() < 1e-9);
        let mid = &audio.samples[4410..audio.frames() - 4410];
        let rms = (mid.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / mid.len() as f64).sqrt();
        let peak = mid.iter().fold(0f32, |m, s| m.max(s.abs())) as f64;
        // A steady sinusoid at about −12 dBFS: peak ≈ 0.25, rms ≈ peak / √2.
        assert!((0.2..0.3).contains(&peak), "peak {peak}");
        assert!((rms / peak - std::f64::consts::FRAC_1_SQRT_2).abs() < 0.05, "rms {rms} peak {peak}");
        let f = dominant_freq(&mid[..8192], 44_100);
        assert!((180.0..=240.0).contains(&f), "dominant {f} Hz");
    }

    #[test]
    fn elevenlabs_style_info_tag_trims_1105_frames_at_the_start() {
        // The same audio with and without the Info/LAME tag: the tagged decode equals the untagged decode minus
        // the 1105-frame delay (and the end padding).
        let tagged = decode(&mp3gen::build(12, true), "audio/mpeg").expect("tagged");
        let untagged = decode(&mp3gen::build(12, false), "audio/mpeg").expect("untagged");
        // Without the tag nothing tells symphonia about the delay: the fallback drops 1105 frames, no end padding.
        assert_eq!(untagged.frames(), 12 * 1152 - untagged_mp3_delay(44_100));
        assert_eq!(tagged.frames(), 12 * 1152 - 1105 - 471);
        // Both start at the same audio position: the fallback and the tag agree.
        let n = tagged.frames().min(untagged.frames());
        for i in 0..n {
            assert!((tagged.samples[i] - untagged.samples[i]).abs() < 1e-6, "frame {i}");
        }
    }

    #[test]
    fn decodes_mock_wav() {
        // vox-tts mock for "Hi.": 0.6 s at 22 050 Hz = 13 230 frames, 26 504 bytes.
        let wav = mock_wav(22_050, 13_230);
        assert_eq!(wav.len(), 26_504);
        let audio = decode(&wav, "audio/wav").expect("decode");
        assert_eq!(audio.sample_rate, 22_050);
        assert_eq!(audio.channels, 1);
        assert_eq!(audio.frames(), 13_230);
        assert!((audio.duration_sec() - 0.6).abs() < 1e-9);
        let peak = audio.samples.iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!((0.0145..0.0155).contains(&peak), "{peak}");
    }

    #[test]
    fn sniffs_when_the_mime_is_wrong_or_unknown() {
        let wav = mock_wav(22_050, 2205);
        assert_eq!(decode(&wav, "audio/mpeg").expect("mislabelled").frames(), 2205);
        assert_eq!(decode(&wav, "").expect("no mime").frames(), 2205);
        assert_eq!(decode(FIXTURE, "application/octet-stream").expect("sniffed mp3").sample_rate, 44_100);
    }

    #[test]
    fn stereo_wav_is_interleaved() {
        let mut pcm = Vec::new();
        for i in 0..100i16 {
            pcm.extend_from_slice(&(i * 100).to_le_bytes());
            pcm.extend_from_slice(&(-i * 100).to_le_bytes());
        }
        let audio = decode(&wav_bytes(44_100, 2, &pcm), "audio/wav").expect("decode");
        assert_eq!(audio.channels, 2);
        assert_eq!(audio.frames(), 100);
        assert!((audio.samples[2 * 50] + audio.samples[2 * 50 + 1]).abs() < 1e-6);
        assert!(audio.samples[2 * 50] > 0.1);
    }

    #[test]
    fn rejects_garbage() {
        assert!(matches!(decode(&[], "audio/mpeg"), Err(DecodeError::Corrupt(_))));
        assert!(decode(b"not audio at all, just some text", "audio/mpeg").is_err());
        assert!(decode(&[0u8; 4096], "audio/wav").is_err());
        // A truncated MP3 still yields what decoded.
        let truncated = &FIXTURE[..FIXTURE.len() / 2];
        assert!(decode(truncated, "audio/mpeg").map(|a| a.frames() > 0).unwrap_or(false));
        // Random bytes never panic.
        let mut x = 0x1234_5678u32;
        for len in [1usize, 7, 100, 1000, 5000] {
            let bytes: Vec<u8> = (0..len)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 17;
                    x ^= x << 5;
                    x as u8
                })
                .collect();
            let _ = decode(&bytes, "audio/mpeg");
            let _ = decode(&bytes, "audio/wav");
        }
    }

    #[test]
    fn decoded_duration_helpers() {
        let a = DecodedAudio { sample_rate: 0, channels: 1, samples: vec![0.0; 10] };
        assert_eq!(a.duration_sec(), 0.0);
        let b = DecodedAudio { sample_rate: 10, channels: 0, samples: vec![0.0; 10] };
        assert_eq!(b.frames(), 0);
        assert_eq!(untagged_mp3_delay(22_050), 529);
        assert_eq!(untagged_mp3_delay(48_000), 1105);
    }
}
