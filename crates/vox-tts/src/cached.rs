//! Cached-replay helpers (reader-playback.md §2.7): rebuilding the session text of a preview-only record and
//! redacting alignment text before caching.

use vox_core::js::js_round;
use vox_core::text16::utf16_len;
use vox_core::tts::{Alignment, CachedChunk};

/// `previewReplay(preview, cached)`: the session text = preview + one `…` per chunk past it (a straddling chunk gets
/// `textEnd = len + 1` over an appended `…` and a clipped alignment; a chunk entirely past it gets `"\n…"` and
/// `alignment: None`). Offsets are UTF-16. Chunks are processed sorted by index.
pub fn preview_replay(preview: &str, chunks: Vec<CachedChunk>) -> (String, Vec<CachedChunk>) {
    let preview_len = u32::try_from(utf16_len(preview)).unwrap_or(u32::MAX);
    let mut text = preview.to_string();
    let mut text_len = preview_len; // UTF-16 length of `text`, kept in step with it
    let mut sorted = chunks;
    sorted.sort_by_key(|c| c.index); // stable
    let rebuilt = sorted
        .into_iter()
        .map(|c| {
            let start = c.text_start;
            let end = c.text_end.max(start);
            if end <= preview_len {
                c
            } else if start < preview_len {
                // Straddles the end of the preview (only ever the first chunk past it).
                text.push('\u{2026}');
                text_len += 1;
                let alignment = clip_alignment(&c, end - start, preview_len - start);
                CachedChunk { text_start: start, text_end: preview_len + 1, alignment, ..c }
            } else {
                // Entirely past the preview: its own `…` line (a UAX #29 sentence of its own).
                text.push_str("\n\u{2026}");
                text_len += 2;
                CachedChunk { text_start: text_len - 1, text_end: text_len, alignment: None, ..c }
            }
        })
        .collect();
    (text, rebuilt)
}

/// JS `Math.max(a, b)`: NaN if either is NaN.
fn js_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::NAN
    } else {
        a.max(b)
    }
}

/// `clipAlignment(c, len, keep)`: timing for the first `keep` chars plus one entry for the rest (chars all `""`).
pub fn clip_alignment(chunk: &CachedChunk, len: u32, keep: u32) -> Option<Alignment> {
    let a = chunk.alignment.as_ref().filter(|a| !a.starts.is_empty() && a.ends.len() == a.starts.len());
    let n = a.map_or(0, |a| a.starts.len());
    let last_end = a.map_or(0.0, |a| a.ends.iter().copied().fold(f64::NEG_INFINITY, js_max));
    let total = if chunk.duration_sec > 0.0 { chunk.duration_sec } else { last_end };
    if total.is_nan() || total <= 0.0 {
        return None;
    }
    let len_f = f64::from(len);
    let entry = |k: u32| -> usize {
        let k = k as usize;
        if n == len as usize || len <= 1 {
            k.min(n - 1)
        } else {
            let mapped = js_round(k as f64 * (n - 1) as f64 / (len_f - 1.0));
            (mapped.max(0.0) as usize).min(n - 1)
        }
    };
    let count = keep as usize + 1;
    let mut starts = Vec::with_capacity(count);
    let mut ends = Vec::with_capacity(count);
    for k in 0..=keep {
        let kf = f64::from(k);
        starts.push(match a {
            Some(a) => a.starts[entry(k)],
            None => total * kf / len_f,
        });
        ends.push(if k == keep {
            js_max(total, last_end)
        } else {
            match a {
                Some(a) => a.ends[entry(k)],
                None => total * (kf + 1.0) / len_f,
            }
        });
    }
    Some(Alignment { chars: vec![String::new(); count], starts, ends })
}

/// `withoutText(chunks)`: same chunks with every alignment char replaced by `""`.
pub fn without_text(chunks: &[CachedChunk]) -> Vec<CachedChunk> {
    chunks
        .iter()
        .map(|c| CachedChunk { alignment: c.alignment.as_ref().map(Alignment::redacted), ..c.clone() })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn chunk(index: u32, start: u32, end: u32, duration: f64, alignment: Option<Alignment>) -> CachedChunk {
        CachedChunk {
            index,
            text_start: start,
            text_end: end,
            mime: "audio/mpeg".into(),
            alignment,
            duration_sec: duration,
            audio: Arc::from(vec![index as u8; 4]),
        }
    }

    fn even(n: usize, total: f64) -> Alignment {
        let per = total / n as f64;
        Alignment {
            chars: (0..n).map(|i| format!("c{i}")).collect(),
            starts: (0..n).map(|i| i as f64 * per).collect(),
            ends: (0..n).map(|i| (i + 1) as f64 * per).collect(),
        }
    }

    #[test]
    fn preview_replay_rebuilds_the_text() {
        let preview = "a".repeat(160);
        let chunks = vec![
            chunk(2, 300, 500, 8.0, Some(even(200, 8.0))),
            chunk(0, 0, 100, 4.0, Some(even(100, 4.0))),
            chunk(1, 101, 299, 6.0, Some(even(198, 6.0))),
        ];
        let (text, out) = preview_replay(&preview, chunks);
        assert_eq!(text, format!("{preview}\u{2026}\n\u{2026}"));
        assert_eq!(utf16_len(&text), 163);
        // Sorted by index; the first chunk is kept unchanged.
        assert_eq!(out.iter().map(|c| c.index).collect::<Vec<_>>(), [0, 1, 2]);
        assert_eq!((out[0].text_start, out[0].text_end), (0, 100));
        assert_eq!(out[0].alignment.as_ref().unwrap().chars[0], "c0");
        // The straddling chunk covers the preview tail + the appended `…`.
        assert_eq!((out[1].text_start, out[1].text_end), (101, 161));
        let a = out[1].alignment.as_ref().unwrap();
        assert_eq!(a.chars.len(), 60); // keep = 59 visible units + one for the rest
        assert!(a.chars.iter().all(String::is_empty));
        assert_eq!(*a.ends.last().unwrap(), 6.0);
        // Entirely past: its own `\n…` line, no alignment; audio and duration kept.
        assert_eq!((out[2].text_start, out[2].text_end), (162, 163));
        assert!(out[2].alignment.is_none());
        assert_eq!(out[2].duration_sec, 8.0);
        assert_eq!(&out[2].audio[..], &[2, 2, 2, 2]);
        // Every offset lies inside the session text.
        assert!(out.iter().all(|c| c.text_end as usize <= utf16_len(&text)));
    }

    #[test]
    fn preview_replay_edge_cases() {
        // Everything inside the preview: unchanged, no ellipsis.
        let (text, out) = preview_replay("hello", vec![chunk(0, 0, 5, 1.0, None)]);
        assert_eq!(text, "hello");
        assert_eq!(out[0].text_end, 5);
        // A chunk ending before it starts is treated as empty (end = start).
        let (text, out) = preview_replay("hi", vec![chunk(0, 1, 0, 1.0, None)]);
        assert_eq!(text, "hi");
        assert_eq!(out[0].text_end, 0); // kept unchanged (not floored), like the original
        // A chunk starting exactly at the preview end is entirely past it.
        let (text, out) = preview_replay("hi", vec![chunk(0, 2, 9, 1.0, None)]);
        assert_eq!(text, "hi\n\u{2026}");
        assert_eq!((out[0].text_start, out[0].text_end), (3, 4));
        // UTF-16 lengths: an emoji preview is 2 units.
        let (text, out) = preview_replay("😀", vec![chunk(0, 0, 4, 1.0, None)]);
        assert_eq!(text, "😀\u{2026}");
        assert_eq!((out[0].text_start, out[0].text_end), (0, 3));
        // Empty input.
        let (text, out) = preview_replay("", vec![]);
        assert_eq!((text.as_str(), out.len()), ("", 0));
        // Equal indices keep their input order.
        let (_, out) = preview_replay("abcdef", vec![chunk(1, 3, 6, 1.0, None), chunk(1, 0, 3, 1.0, None)]);
        assert_eq!(out[0].text_start, 3);
    }

    #[test]
    fn clip_alignment_maps_proportionally() {
        // 1:1 alignment (n == len): the kept entries are copied, the rest is one entry to the end.
        let c = chunk(0, 0, 4, 2.0, Some(even(4, 2.0)));
        let a = clip_alignment(&c, 4, 2).unwrap();
        assert_eq!(a.chars, ["", "", ""]);
        assert_eq!(a.starts, [0.0, 0.5, 1.0]);
        assert_eq!(a.ends, [0.5, 1.0, 2.0]);
        // Proportional (n = 3 entries over len = 5 units): entry(k) = round(k × 2 / 4).
        let c = chunk(0, 0, 5, 3.0, Some(even(3, 3.0)));
        let a = clip_alignment(&c, 5, 3).unwrap();
        assert_eq!(a.starts, [0.0, 1.0, 1.0, 2.0]); // entries 0, round(0.5)=1, 1, round(1.5)=2
        assert_eq!(a.ends, [1.0, 2.0, 2.0, 3.0]);
        // No alignment: linear timing over the duration.
        let c = chunk(0, 0, 4, 2.0, None);
        let a = clip_alignment(&c, 4, 1).unwrap();
        assert_eq!(a.starts, [0.0, 0.5]);
        assert_eq!(a.ends, [0.5, 2.0]);
        // Duration ≤ 0 → the last alignment end; the final end is max(total, lastEnd).
        let c = chunk(0, 0, 4, 0.0, Some(even(4, 2.0)));
        assert_eq!(clip_alignment(&c, 4, 1).unwrap().ends, [0.5, 2.0]);
        let c = chunk(0, 0, 4, 1.0, Some(even(4, 2.0)));
        assert_eq!(*clip_alignment(&c, 4, 1).unwrap().ends.last().unwrap(), 2.0);
        // Nothing known → None.
        assert!(clip_alignment(&chunk(0, 0, 4, 0.0, None), 4, 1).is_none());
        assert!(clip_alignment(&chunk(0, 0, 4, f64::NAN, None), 4, 1).is_none());
        // Inconsistent alignment arrays count as no alignment.
        let mut bad = even(4, 2.0);
        bad.ends.pop();
        let a = clip_alignment(&chunk(0, 0, 4, 2.0, Some(bad)), 4, 1).unwrap();
        assert_eq!(a.starts, [0.0, 0.5]);
        // len ≤ 1 uses min(k, n − 1).
        let a = clip_alignment(&chunk(0, 0, 1, 1.0, Some(even(3, 1.0))), 1, 0).unwrap();
        assert_eq!(a.starts.len(), 1);
        // A NaN end makes lastEnd NaN: with a positive duration the final end is NaN (harmless).
        let mut nan = even(2, 1.0);
        nan.ends[0] = f64::NAN;
        let a = clip_alignment(&chunk(0, 0, 2, 1.0, Some(nan.clone())), 2, 1).unwrap();
        assert!(a.ends.last().unwrap().is_nan());
        assert!(clip_alignment(&chunk(0, 0, 2, 0.0, Some(nan)), 2, 1).is_none());
    }

    #[test]
    fn without_text_keeps_timing() {
        let chunks = vec![chunk(0, 0, 2, 1.0, Some(even(2, 1.0))), chunk(1, 3, 4, 1.0, None)];
        let out = without_text(&chunks);
        let a = out[0].alignment.as_ref().unwrap();
        assert_eq!(a.chars, ["", ""]);
        assert_eq!(a.starts, chunks[0].alignment.as_ref().unwrap().starts);
        assert!(out[1].alignment.is_none());
        assert_eq!(out[1].text_start, 3);
    }
}
