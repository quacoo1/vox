//! Integration tests for vox-art: determinism, sizes, palette coverage, robustness, PNG output.
//! The timing test is `#[ignore]`d; run it with
//! `cargo test -p vox-art --release -- --ignored --nocapture`.

use std::collections::HashMap;
use std::time::Instant;
use vox_art::{accent, backdrop, palette_for, thumbnail, Art, Palette};

const NAMES: [&str; 20] = [
    "george", "charlotte", "aria", "roger", "sarah", "laura", "charlie", "river", "liam", "will", "jessica", "eric",
    "chris", "brian", "daniel", "lily", "bill", "alice", "matilda", "callum",
];

fn check_shape(art: &Art, w: u32, h: u32) {
    assert_eq!((art.width, art.height), (w, h));
    assert_eq!(art.rgba.len(), (w * h * 4) as usize);
    assert!(art.rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 255), "silk art is opaque");
}

/// Mean (luma, chroma) over the image in sRGB 0..1.
fn stats(art: &Art) -> (f32, f32) {
    let n = (art.rgba.len() / 4).max(1) as f32;
    let (mut l, mut c) = (0.0, 0.0);
    for p in art.rgba.as_chunks::<4>().0 {
        let (r, g, b) = (p[0] as f32 / 255.0, p[1] as f32 / 255.0, p[2] as f32 / 255.0);
        l += 0.2126 * r + 0.7152 * g + 0.0722 * b;
        c += r.max(g).max(b) - r.min(g).min(b);
    }
    (l / n, c / n)
}

#[test]
fn thumbnails_are_deterministic() {
    for seed in ["george", "JBFqnCBsd6RMkjVDRZzb", "", "日本語の声"] {
        for (w, h) in [(220, 164), (88, 88), (32, 32)] {
            assert_eq!(thumbnail(seed, w, h), thumbnail(seed, w, h), "seed {seed:?} {w}x{h}");
        }
        assert_eq!(accent(seed), accent(seed));
        assert_eq!(palette_for(seed), palette_for(seed));
    }
}

#[test]
fn backdrop_is_deterministic() {
    assert_eq!(backdrop(320, 200, true), backdrop(320, 200, true));
    assert_eq!(backdrop(320, 200, false), backdrop(320, 200, false));
}

#[test]
fn sizes_and_aspect_ratios() {
    for (w, h) in [(220, 164), (110, 82), (88, 88), (56, 56), (44, 44), (32, 32), (28, 28), (16, 16), (1, 1), (1, 40), (300, 20), (7, 3)] {
        check_shape(&thumbnail("aria", w, h), w, h);
    }
    for (w, h) in [(1600, 1000), (1, 1), (33, 900), (640, 360)] {
        check_shape(&backdrop(w, h, true), w, h);
        check_shape(&backdrop(w, h, false), w, h);
    }
}

#[test]
fn zero_sizes_do_not_panic() {
    for (w, h) in [(0, 0), (0, 10), (10, 0)] {
        let a = thumbnail("x", w, h);
        assert!(a.rgba.is_empty());
        assert!(backdrop(w, h, true).rgba.is_empty());
    }
}

#[test]
fn odd_seeds_do_not_panic() {
    let long = "z".repeat(10_000);
    for seed in ["", " ", "\0", "🎙️🔊", "Ωmega", "a\u{0301}", "\u{FEFF}", long.as_str()] {
        check_shape(&thumbnail(seed, 44, 44), 44, 44);
        check_shape(&thumbnail(seed, 1, 1), 1, 1);
        let _ = accent(seed);
    }
}

#[test]
fn palette_coverage_is_broad_and_balanced() {
    let mut counts: HashMap<Palette, usize> = HashMap::new();
    let n = 2000;
    for i in 0..n {
        *counts.entry(palette_for(&format!("voice-{i}"))).or_default() += 1;
    }
    for p in Palette::ALL {
        let c = counts.get(&p).copied().unwrap_or(0);
        let share = c as f32 / n as f32;
        let fair = 1.0 / Palette::ALL.len() as f32;
        assert!(share > fair * 0.6 && share < fair * 1.4, "{} share {share:.3}", p.name());
    }
    // The names used by the design previews already cover most palettes.
    let named: std::collections::HashSet<Palette> = NAMES.iter().map(|s| palette_for(s)).collect();
    assert!(named.len() >= 7, "only {} palettes across the preview names", named.len());
}

#[test]
fn palette_names_round_trip() {
    for p in Palette::ALL {
        assert_eq!(Palette::from_name(p.name()), Some(p));
        assert_eq!(Palette::from_name(&p.name().to_uppercase()), Some(p));
    }
    assert_eq!(Palette::from_name("nope"), None);
}

#[test]
fn seeds_vary_within_a_palette() {
    // Collect a few seeds per palette and make sure their thumbnails differ clearly.
    let mut by_palette: HashMap<Palette, Vec<String>> = HashMap::new();
    for i in 0..400 {
        let s = format!("seed{i}");
        by_palette.entry(palette_for(&s)).or_default().push(s);
    }
    for (p, seeds) in by_palette {
        let a = thumbnail(&seeds[0], 64, 48);
        let b = thumbnail(&seeds[1], 64, 48);
        let diff: u64 = a.rgba.iter().zip(b.rgba.iter()).map(|(x, y)| (*x as i32 - *y as i32).unsigned_abs() as u64).sum();
        let mean = diff as f32 / a.rgba.len() as f32;
        assert!(mean > 8.0, "{}: {} vs {} differ by only {mean:.1}/255 on average", p.name(), seeds[0], seeds[1]);
    }
}

#[test]
fn small_icons_keep_their_colour() {
    // A 32 px icon (16 px at 2x) should carry about as much colour as the list thumbnail,
    // not turn to mud.
    for seed in NAMES {
        if palette_for(seed) == Palette::Steel {
            continue; // deliberately near-monochrome
        }
        let (_, c_big) = stats(&thumbnail(seed, 220, 164));
        let (l_small, c_small) = stats(&thumbnail(seed, 32, 32));
        assert!(c_small > 0.55 * c_big, "{seed}: chroma {c_small:.3} vs {c_big:.3}");
        assert!(l_small > 0.08, "{seed}: icon too dark ({l_small:.3})");
    }
}

#[test]
fn flat_layouts_are_rerolled() {
    // A random layout can leave the frame a smooth, generic wash (the crease or folds just
    // outside it). Those are re-rolled, so almost every seed ends up with real structure, while
    // most seeds keep their first layout.
    let n = 120;
    let (mut flat, mut first) = (0, 0);
    for i in 0..n {
        let seed = format!("voice-{i}");
        let name = vox_art::debug::style_name(&seed);
        let style = vox_art::debug::STYLES.iter().position(|s| *s == name).unwrap();
        let (scores, chosen) = vox_art::debug::variant_scores(&seed, palette_for(&seed), style);
        let best = scores.iter().copied().fold(f32::MIN, f32::max);
        if chosen == 0 {
            first += 1;
        } else {
            // Re-rolls stop at the first layout that passes, or take the best one.
            assert!(scores[chosen as usize] >= 0.08 || scores[chosen as usize] == best, "{seed}: {scores:?} -> {chosen}");
        }
        if scores[chosen as usize] < 0.08 {
            flat += 1;
        }
    }
    assert!(flat * 100 <= n * 3, "{flat} of {n} thumbnails are still flat");
    assert!(first * 100 >= n * 60, "only {first} of {n} seeds kept their first layout");
}

#[test]
fn accents_are_visible_on_the_dark_panel() {
    for seed in NAMES {
        let [r, g, b] = accent(seed);
        let l = 0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32;
        assert!(l > 60.0, "{seed}: accent #{r:02x}{g:02x}{b:02x} too dark");
    }
}

#[test]
fn backdrops_have_the_right_key() {
    let (dark_l, _) = stats(&backdrop(400, 250, true));
    let (light_l, _) = stats(&backdrop(400, 250, false));
    assert!(dark_l < 0.2, "dark backdrop mean luma {dark_l:.3}");
    assert!(light_l > 0.75, "light backdrop mean luma {light_l:.3}");
}

#[test]
fn png_round_trip() {
    let art = thumbnail("roger", 61, 37);
    let dir = std::env::temp_dir().join(format!("vox-art-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("roger.png");
    art.save_png(&path).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    let decoded = tiny_skia::Pixmap::decode_png(&bytes).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (61, 37));
    assert_eq!(decoded.data(), &art.rgba[..], "opaque art round-trips exactly");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore]
fn timing() {
    // Warm up (lookup tables, page faults).
    let _ = thumbnail("warmup", 220, 164);
    let _ = backdrop(400, 250, true);

    let t = Instant::now();
    for s in NAMES {
        let _ = thumbnail(s, 220, 164);
    }
    let thumb_ms = t.elapsed().as_secs_f64() * 1000.0 / NAMES.len() as f64;

    let t = Instant::now();
    for s in NAMES {
        let _ = thumbnail(s, 88, 88);
    }
    let small_ms = t.elapsed().as_secs_f64() * 1000.0 / NAMES.len() as f64;

    let t = Instant::now();
    let _ = backdrop(1600, 1000, true);
    let dark_ms = t.elapsed().as_secs_f64() * 1000.0;
    let t = Instant::now();
    let _ = backdrop(1600, 1000, false);
    let light_ms = t.elapsed().as_secs_f64() * 1000.0;

    eprintln!("thumbnail 220x164: {thumb_ms:.1} ms mean; 88x88: {small_ms:.1} ms mean");
    eprintln!("backdrop 1600x1000: dark {dark_ms:.0} ms, light {light_ms:.0} ms");
    assert!(thumb_ms < 25.0, "thumbnail too slow: {thumb_ms:.1} ms");
    assert!(dark_ms < 500.0 && light_ms < 500.0, "backdrop too slow");
}
