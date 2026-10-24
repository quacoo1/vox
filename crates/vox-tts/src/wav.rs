//! WAV helper (tts.md §6): 16-bit PCM mono writer and a RIFF walker.

const HEADER_BYTES: usize = 44;

/// `encodeWav(samples, sampleRate)`: 44-byte header, little-endian; `v = clamp(s, -1, 1)`,
/// `round(v < 0 ? v × 32768 : v × 32767)` with JS rounding.
pub fn encode_wav(samples: &[f32], sample_rate: u32) -> Vec<u8> {
    let data_bytes = samples.len() * 2;
    let mut buf = Vec::with_capacity(HEADER_BYTES + data_bytes);
    // RIFF sizes are u32; a mono 16-bit file this large (> 2 GiB of samples) is never produced.
    let data_u32 = u32::try_from(data_bytes).unwrap_or(u32::MAX);
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&data_u32.saturating_add(36).to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    buf.extend_from_slice(&1u16.to_le_bytes()); // PCM
    buf.extend_from_slice(&1u16.to_le_bytes()); // mono
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.extend_from_slice(&sample_rate.wrapping_mul(2).to_le_bytes()); // byte rate
    buf.extend_from_slice(&2u16.to_le_bytes()); // block align
    buf.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_u32.to_le_bytes());
    for &s in samples {
        buf.extend_from_slice(&sample_to_i16(s).to_le_bytes());
    }
    buf
}

/// JS `Math.round(v < 0 ? v * 0x8000 : v * 0x7fff)` of the clamped sample (halves toward +∞, so −2⁻¹⁶ → 0; NaN → 0).
fn sample_to_i16(s: f32) -> i16 {
    let v = f64::from(s).clamp(-1.0, 1.0);
    let scaled = if v < 0.0 { v * 32768.0 } else { v * 32767.0 };
    // `as` saturates and maps NaN to 0, which is what Node writes for NaN.
    (scaled + 0.5).floor() as i16
}

/// Parsed `fmt ` + `data` facts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WavInfo {
    pub format: u16,
    pub channels: u16,
    pub sample_rate: u32,
    pub bits_per_sample: u16,
    pub data_offset: usize,
    pub data_bytes: usize,
    pub duration_sec: f64,
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// `readWavInfo(bytes)` (walks RIFF chunks; 64-bit arithmetic; a later `fmt ` overrides; `data` before `fmt ` is
/// skipped).
pub fn read_wav_info(bytes: &[u8]) -> Option<WavInfo> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let len = bytes.len() as u64;
    // (format, channels, sample_rate, bits_per_sample)
    let mut fmt: Option<(u16, u16, u32, u16)> = None;
    let mut at: u64 = 12;
    while at + 8 <= len {
        let a = usize::try_from(at).ok()?;
        let id = &bytes[a..a + 4];
        let size = u64::from(u32_at(bytes, a + 4)?);
        let body = at + 8;
        if id == b"fmt " && body + 16 <= len {
            let b = usize::try_from(body).ok()?;
            fmt = Some((u16_at(bytes, b)?, u16_at(bytes, b + 2)?, u32_at(bytes, b + 4)?, u16_at(bytes, b + 14)?));
        } else if id == b"data" {
            if let Some((format, channels, sample_rate, bits_per_sample)) = fmt {
                let data_bytes = size.min(len - body);
                let frame_bytes = f64::from(channels) * (f64::from(bits_per_sample) / 8.0);
                let duration_sec = if frame_bytes > 0.0 && sample_rate > 0 {
                    data_bytes as f64 / frame_bytes / f64::from(sample_rate)
                } else {
                    0.0
                };
                return Some(WavInfo {
                    format,
                    channels,
                    sample_rate,
                    bits_per_sample,
                    data_offset: usize::try_from(body).ok()?,
                    data_bytes: usize::try_from(data_bytes).ok()?,
                    duration_sec,
                });
            }
        }
        at = body + size + (size % 2); // chunks are word aligned
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = id.to_vec();
        v.extend_from_slice(&(body.len() as u32).to_le_bytes());
        v.extend_from_slice(body);
        if body.len() % 2 == 1 {
            v.push(0);
        }
        v
    }

    fn fmt_body(channels: u16, rate: u32, bits: u16) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&channels.to_le_bytes());
        b.extend_from_slice(&rate.to_le_bytes());
        b.extend_from_slice(&(rate * u32::from(channels) * u32::from(bits) / 8).to_le_bytes());
        b.extend_from_slice(&(channels * bits / 8).to_le_bytes());
        b.extend_from_slice(&bits.to_le_bytes());
        b
    }

    fn riff(chunks: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = chunks.concat();
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&(4 + body.len() as u32).to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(&body);
        v
    }

    #[test]
    fn header_layout() {
        let w = encode_wav(&[0.0, 1.0, -1.0, 0.5], 22_050);
        assert_eq!(w.len(), 44 + 8);
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(u32_at(&w, 4), Some(36 + 8));
        assert_eq!(&w[8..16], b"WAVEfmt ");
        assert_eq!(u32_at(&w, 16), Some(16));
        assert_eq!(u16_at(&w, 20), Some(1));
        assert_eq!(u16_at(&w, 22), Some(1));
        assert_eq!(u32_at(&w, 24), Some(22_050));
        assert_eq!(u32_at(&w, 28), Some(44_100));
        assert_eq!(u16_at(&w, 32), Some(2));
        assert_eq!(u16_at(&w, 34), Some(16));
        assert_eq!(&w[36..40], b"data");
        assert_eq!(u32_at(&w, 40), Some(8));
        let samples: Vec<i16> = w[44..].chunks(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(samples, [0, 32767, -32768, 16384]); // 0.5 × 32767 = 16383.5 → 16384
    }

    #[test]
    fn js_rounding_and_clamping() {
        assert_eq!(sample_to_i16(-(2f32.powi(-16))), 0); // −0.5 → −0 in JS, not −1
        assert_eq!(sample_to_i16(-(2f32.powi(-15))), -1);
        assert_eq!(sample_to_i16(2.0), 32767);
        assert_eq!(sample_to_i16(-2.0), -32768);
        assert_eq!(sample_to_i16(f32::NAN), 0);
        let w = encode_wav(&[-(2f32.powi(-16))], 8_000);
        assert_eq!(&w[44..46], &[0, 0]);
        assert_eq!(encode_wav(&[], 8_000).len(), 44);
    }

    #[test]
    fn reads_the_mock_file() {
        let w = crate::mock::mock_synthesize("Hi.", 1.0).audio;
        let info = read_wav_info(&w).unwrap();
        assert_eq!(
            info,
            WavInfo {
                format: 1,
                channels: 1,
                sample_rate: 22_050,
                bits_per_sample: 16,
                data_offset: 44,
                data_bytes: 26_460,
                duration_sec: 0.6
            }
        );
    }

    #[test]
    fn walks_extra_and_odd_chunks() {
        let data = vec![0u8; 8_000];
        let w = riff(&[chunk(b"LIST", b"odd"), chunk(b"fmt ", &fmt_body(1, 8_000, 16)), chunk(b"fact", &[1, 2, 3, 4]), chunk(b"data", &data)]);
        let info = read_wav_info(&w).unwrap();
        assert_eq!(info.data_offset, 12 + 12 + 24 + 12 + 8);
        assert_eq!(info.data_bytes, 8_000);
        assert_eq!(info.duration_sec, 0.5);
        // Stereo 24-bit.
        let w = riff(&[chunk(b"fmt ", &fmt_body(2, 48_000, 24)), chunk(b"data", &vec![0u8; 288_000])]);
        assert_eq!(read_wav_info(&w).unwrap().duration_sec, 1.0);
    }

    #[test]
    fn data_before_fmt_is_skipped_and_later_fmt_overrides() {
        let w = riff(&[chunk(b"data", &[0u8; 4]), chunk(b"fmt ", &fmt_body(1, 8_000, 16))]);
        assert_eq!(read_wav_info(&w), None);
        let w = riff(&[chunk(b"fmt ", &fmt_body(1, 8_000, 16)), chunk(b"fmt ", &fmt_body(2, 16_000, 16)), chunk(b"data", &[0u8; 64_000])]);
        let info = read_wav_info(&w).unwrap();
        assert_eq!((info.channels, info.sample_rate, info.duration_sec), (2, 16_000, 1.0));
    }

    #[test]
    fn truncated_and_streaming_headers() {
        // Declared size 0xFFFFFFFF (streaming) → clamped to what is there.
        let mut w = riff(&[chunk(b"fmt ", &fmt_body(1, 8_000, 16))]);
        w.extend_from_slice(b"data");
        w.extend_from_slice(&u32::MAX.to_le_bytes());
        w.extend_from_slice(&[0u8; 1_600]);
        let info = read_wav_info(&w).unwrap();
        assert_eq!(info.data_bytes, 1_600);
        assert_eq!(info.duration_sec, 0.1);
        // Truncated mock file.
        let full = crate::mock::mock_synthesize("Hi.", 1.0).audio;
        let info = read_wav_info(&full[..1_044]).unwrap();
        assert_eq!(info.data_bytes, 1_000);
        // A fmt chunk without 16 bytes of body in the buffer is skipped.
        let mut w = b"RIFF\0\0\0\0WAVEfmt \x10\0\0\0".to_vec();
        w.extend_from_slice(&[1, 0, 1, 0]);
        assert_eq!(read_wav_info(&w), None);
        // Bad magic and short input.
        assert_eq!(read_wav_info(b"RIFF\0\0\0\0WAVX"), None);
        assert_eq!(read_wav_info(b"RIFF"), None);
        assert_eq!(read_wav_info(&[]), None);
        // Zero channels / bits → duration 0.
        let w = riff(&[chunk(b"fmt ", &fmt_body(0, 8_000, 16)), chunk(b"data", &[0u8; 10])]);
        assert_eq!(read_wav_info(&w).unwrap().duration_sec, 0.0);
    }

    #[test]
    fn huge_chunk_sizes_do_not_overflow() {
        let mut w = b"RIFF\0\0\0\0WAVE".to_vec();
        w.extend_from_slice(b"JUNK");
        w.extend_from_slice(&u32::MAX.to_le_bytes());
        w.extend_from_slice(&[0u8; 16]);
        assert_eq!(read_wav_info(&w), None);
    }
}
