//! vox-art: render Vox silk art to PNG files.
//!
//!   vox-art thumbs <out_dir> [seed...]          thumb-<seed>.png (220x164) + sheet.png
//!   vox-art backdrop <out.png> <W>x<H> [--light]
//!   vox-art styles <out_dir> [seed]             styles.png: every palette x every style (prints layout scores)
//!   vox-art style <name> <out_dir> [count]      style-<name>.png: one style, every palette x `count` seeds
//!   vox-art previews <out_dir>                  the design-preview sample set (ui/previews/art)
//!
//! The preview set is seeded exactly like the app (DESIGN.md "Art"): voice art by **voice id** (the ElevenLabs premade
//! ids; the demo catalog uses the same ids), written as `thumb-<name>.png` (220×164) and `small-<name>.png` (88×88)
//! so the fixtures can name voices; the onboarding heroes by the app's step seeds (`hero-<step>.png`).

use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;
use tiny_skia::{FillRule, FilterQuality, Paint, Path as SkPath, PathBuilder, Pattern, Pixmap, SpreadMode, Transform};
use vox_art::{accent, backdrop, palette_for, thumbnail, Art, Palette};

const PREVIEW_SEEDS: [&str; 20] = [
    "george", "charlotte", "aria", "roger", "sarah", "laura", "charlie", "river", "liam", "will", "jessica", "eric",
    "chris", "brian", "daniel", "lily", "bill", "alice", "matilda", "callum",
];

/// The preview voices' ElevenLabs premade voice ids (the art seeds the app uses), by preview name.
const PREVIEW_VOICE_IDS: [(&str, &str); 20] = [
    ("george", "JBFqnCBsd6RMkjVDRZzb"),
    ("charlotte", "XB0fDUnXU5powFXDhCwa"),
    ("aria", "9BWtsMINqrJLrRacOk9x"),
    ("roger", "CwhRBWXzGAHq8TQ4Fs17"),
    ("sarah", "EXAVITQu4vr4xnSDxMaL"),
    ("laura", "FGY2WhTYpPnrIDTdsKH5"),
    ("charlie", "IKne3meq5aSn9XLyUdCD"),
    ("river", "SAz9YHcvj6GT2YYXdXww"),
    ("liam", "TX3LPaxmHKxFdv7VOQHJ"),
    ("will", "bIHbv24MWmeRgasZH58o"),
    ("jessica", "cgSgspJ2msm6clMCkdW9"),
    ("eric", "cjVigY5qzO86Huf0OWal"),
    ("chris", "iP95p4xoKVk53GoZ742B"),
    ("brian", "nPczCjzI2devNBz1zQrb"),
    ("daniel", "onwK4e9ZLuTAKqWW03F9"),
    ("lily", "pFZP5JQG7iQjIQuC4Bku"),
    ("bill", "pqHfZKP75CvOlQylNhV4"),
    ("alice", "Xb7hH8MSUJpSbSDYk0k2"),
    ("matilda", "XrExE9yKIg1WjnnlVkGX"),
    ("callum", "N2lVS1w4EtoT3dr4eOWO"),
];

/// The onboarding step art seeds (`ui::onboarding::step_art_seed`; the voice and try steps use the voice's art).
const HERO_SEEDS: [(&str, &str); 3] =
    [("welcome", "vox-onboarding-welcome"), ("key", "vox-onboarding-key"), ("done", "vox-onboarding-done")];

const EXTRA_SEEDS: [&str; 10] = [
    "JBFqnCBsd6RMkjVDRZzb",
    "EXAVITQu4vr4xnSDxMaL",
    "XB0fDUnXU5powFXDhCwa",
    "pFZP5JQG7iQjIQuC4Bku",
    "onwK4e9ZLuTAKqWW03F9",
    "cgSgspJ2msm6clMCkdW9",
    "Xb7hH8MSUJpSbSDYk0k2",
    "chrome",
    "outlook",
    "slack",
];

fn usage() -> ExitCode {
    eprintln!(
        "usage:\n  vox-art thumbs <out_dir> [seed...]\n  vox-art backdrop <out.png> <W>x<H> [--light]\n  \
         vox-art styles <out_dir> [seed]\n  vox-art style <name> <out_dir> [count]\n  vox-art previews <out_dir>"
    );
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else { return usage() };
    let result = match cmd.as_str() {
        "thumbs" if args.len() >= 2 => cmd_thumbs(Path::new(&args[1]), &args[2..]),
        "backdrop" if args.len() >= 3 => cmd_backdrop(Path::new(&args[1]), &args[2], args.iter().any(|a| a == "--light")),
        "styles" if args.len() >= 2 => cmd_styles(Path::new(&args[1]), args.get(2).map(|s| s.as_str()).unwrap_or("silk")),
        "style" if args.len() >= 3 => cmd_style(&args[1], Path::new(&args[2]), args.get(3).and_then(|s| s.parse().ok()).unwrap_or(6)),
        "previews" if args.len() >= 2 => cmd_previews(Path::new(&args[1])),
        _ => return usage(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("vox-art: {e}");
            ExitCode::FAILURE
        }
    }
}

fn safe_name(seed: &str) -> String {
    let s: String = seed
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if s.is_empty() {
        "_".into()
    } else {
        s
    }
}

fn cmd_thumbs(out: &Path, seeds: &[String]) -> std::io::Result<()> {
    std::fs::create_dir_all(out)?;
    let seeds: Vec<String> = if seeds.is_empty() {
        PREVIEW_SEEDS.iter().chain(EXTRA_SEEDS.iter()).map(|s| s.to_string()).collect()
    } else {
        seeds.to_vec()
    };
    let mut total = 0.0;
    for seed in &seeds {
        let t = Instant::now();
        let art = thumbnail(seed, 220, 164);
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        total += ms;
        art.save_png(&out.join(format!("thumb-{}.png", safe_name(seed))))?;
        println!(
            "{:<24} {:<8} {:<8} {:>6.1} ms  accent #{:02x}{:02x}{:02x}",
            seed,
            palette_for(seed).name(),
            vox_art::debug::style_name(seed),
            ms,
            accent(seed)[0],
            accent(seed)[1],
            accent(seed)[2]
        );
    }
    println!("mean thumbnail time {:.1} ms", total / seeds.len().max(1) as f64);
    let sheet = contact_sheet(&seeds);
    let path = out.join("sheet.png");
    sheet.save_png(&path).map_err(std::io::Error::other)?;
    println!("wrote {}", path.display());
    Ok(())
}

fn parse_size(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.to_ascii_lowercase().split_once('x').map(|(a, b)| (a.to_string(), b.to_string()))?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

fn cmd_backdrop(out: &Path, size: &str, light: bool) -> std::io::Result<()> {
    let Some((w, h)) = parse_size(size) else {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("bad size '{size}', expected WxH")));
    };
    let t = Instant::now();
    let art = backdrop(w, h, !light);
    println!("backdrop {}x{} ({}) in {:.1} ms", w, h, if light { "light" } else { "dark" }, t.elapsed().as_secs_f64() * 1000.0);
    if let Some(dir) = out.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    art.save_png(out)
}

fn cmd_styles(out: &Path, seed: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(out)?;
    let (tw, th, gap) = (220u32, 164u32, 12u32);
    let cols = vox_art::debug::STYLES.len() as u32;
    let rows = Palette::ALL.len() as u32;
    let mut sheet = Pixmap::new(gap + cols * (tw + gap), gap + rows * (th + gap)).unwrap();
    sheet.fill(tiny_skia::Color::from_rgba8(0x1a, 0x1a, 0x1a, 255));
    for (r, pal) in Palette::ALL.iter().enumerate() {
        for c in 0..cols {
            let cell_seed = format!("{seed}-{}", pal.name());
            let (scores, chosen) = vox_art::debug::variant_scores(&cell_seed, *pal, c as usize);
            println!("{:<8} {:<8} variant {chosen}  scores {:.3?}", pal.name(), vox_art::debug::STYLES[c as usize], scores);
            let art = vox_art::debug::thumbnail_with(&cell_seed, *pal, c as usize, tw, th);
            draw_art(&mut sheet, &art, (gap + c * (tw + gap)) as f32, (gap + r as u32 * (th + gap)) as f32, 24.0);
        }
    }
    let path = out.join("styles.png");
    sheet.save_png(&path).map_err(std::io::Error::other)?;
    println!("wrote {} (rows: palettes {:?}; columns: {:?})", path.display(), Palette::ALL.map(|p| p.name()), vox_art::debug::STYLES);
    Ok(())
}

/// One composition style for every palette: rows are palettes, columns are `count` seeds, each cell the
/// 220x164 thumbnail with its 88 and 32 px icons underneath (the sizes where mud shows first).
fn cmd_style(name: &str, out: &Path, count: u32) -> std::io::Result<()> {
    let Some(style) = vox_art::debug::STYLES.iter().position(|s| s.eq_ignore_ascii_case(name)) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("unknown style '{name}', expected one of {:?}", vox_art::debug::STYLES),
        ));
    };
    std::fs::create_dir_all(out)?;
    let count = count.clamp(1, 12);
    let (tw, th, gap) = (220u32, 164u32, 12u32);
    let ch = th + 8 + 88;
    let rows = Palette::ALL.len() as u32;
    let mut sheet = Pixmap::new(gap + count * (tw + gap), gap + rows * (ch + gap)).unwrap();
    sheet.fill(tiny_skia::Color::from_rgba8(0x1a, 0x1a, 0x1a, 255));
    for (r, pal) in Palette::ALL.iter().enumerate() {
        for c in 0..count {
            let seed = format!("{name}-{}-{c}", pal.name());
            let (scores, chosen) = vox_art::debug::variant_scores(&seed, *pal, style);
            println!("{:<8} col {c}: variant {chosen}  scores {:.3?}", pal.name(), scores);
            let x = (gap + c * (tw + gap)) as f32;
            let y = (gap + r as u32 * (ch + gap)) as f32;
            draw_art(&mut sheet, &vox_art::debug::thumbnail_with(&seed, *pal, style, tw, th), x, y, 24.0);
            draw_art(&mut sheet, &vox_art::debug::thumbnail_with(&seed, *pal, style, 88, 88), x, y + th as f32 + 8.0, 20.0);
            draw_art(&mut sheet, &vox_art::debug::thumbnail_with(&seed, *pal, style, 32, 32), x + 100.0, y + th as f32 + 8.0, 8.0);
        }
    }
    let path = out.join(format!("style-{}.png", vox_art::debug::STYLES[style]));
    sheet.save_png(&path).map_err(std::io::Error::other)?;
    println!("wrote {} (rows: palettes {:?})", path.display(), Palette::ALL.map(|p| p.name()));
    Ok(())
}

fn cmd_previews(out: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(out)?;
    for (name, voice_id) in PREVIEW_VOICE_IDS {
        thumbnail(voice_id, 220, 164).save_png(&out.join(format!("thumb-{name}.png")))?;
        thumbnail(voice_id, 88, 88).save_png(&out.join(format!("small-{name}.png")))?;
    }
    for (step, seed) in HERO_SEEDS {
        thumbnail(seed, 220, 164).save_png(&out.join(format!("hero-{step}.png")))?;
    }
    let t = Instant::now();
    backdrop(1600, 1000, true).save_png(&out.join("backdrop-dark.png"))?;
    let dark_ms = t.elapsed().as_secs_f64() * 1000.0;
    let t = Instant::now();
    backdrop(1600, 1000, false).save_png(&out.join("backdrop-light.png"))?;
    let light_ms = t.elapsed().as_secs_f64() * 1000.0;
    let count = PREVIEW_VOICE_IDS.len() * 2 + HERO_SEEDS.len() + 2;
    println!("wrote {count} preview assets to {} (backdrops: dark {dark_ms:.0} ms, light {light_ms:.0} ms incl. PNG)", out.display());
    Ok(())
}

// ------------------------------------------------------------------ contact sheet

fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<SkPath> {
    let r = r.min(w * 0.5).min(h * 0.5);
    let k = 0.552_284_8 * r;
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish()
}

fn art_pixmap(art: &Art) -> Pixmap {
    let size = tiny_skia::IntSize::from_wh(art.width, art.height).unwrap();
    Pixmap::from_vec(art.rgba.clone(), size).unwrap() // opaque: premultiplied == straight
}

fn draw_art(sheet: &mut Pixmap, art: &Art, x: f32, y: f32, radius: f32) {
    let pm = art_pixmap(art);
    let paint = Paint {
        anti_alias: true,
        shader: Pattern::new(pm.as_ref(), SpreadMode::Pad, FilterQuality::Nearest, 1.0, Transform::from_translate(x, y)),
        ..Default::default()
    };
    if let Some(path) = rounded_rect(x, y, art.width as f32, art.height as f32, radius) {
        sheet.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    }
}

fn dot(sheet: &mut Pixmap, cx: f32, cy: f32, r: f32, rgb: [u8; 3]) {
    let mut paint = Paint { anti_alias: true, ..Default::default() };
    paint.set_color_rgba8(rgb[0], rgb[1], rgb[2], 255);
    if let Some(path) = PathBuilder::from_circle(cx, cy, r) {
        sheet.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    }
}

/// Each cell: the 220x164 list thumbnail, then 88, 56 and 32 px squares (44, 28 and 16 px
/// at 2x) and the accent dot, on the panel colour.
fn contact_sheet(seeds: &[String]) -> Pixmap {
    let cols = 5usize;
    let (cw, ch, gap) = (220u32, 164 + 10 + 88, 18u32);
    let rows = seeds.len().div_ceil(cols) as u32;
    let mut sheet = Pixmap::new(gap + cols as u32 * (cw + gap), gap + rows * (ch + gap)).unwrap();
    sheet.fill(tiny_skia::Color::from_rgba8(0x1a, 0x1a, 0x1a, 255));
    for (i, seed) in seeds.iter().enumerate() {
        let x = (gap + (i % cols) as u32 * (cw + gap)) as f32;
        let y = (gap + (i / cols) as u32 * (ch + gap)) as f32;
        draw_art(&mut sheet, &thumbnail(seed, 220, 164), x, y, 24.0);
        let yy = y + 174.0;
        draw_art(&mut sheet, &thumbnail(seed, 88, 88), x, yy, 20.0);
        draw_art(&mut sheet, &thumbnail(seed, 56, 56), x + 100.0, yy, 14.0);
        draw_art(&mut sheet, &thumbnail(seed, 32, 32), x + 168.0, yy, 8.0);
        dot(&mut sheet, x + 184.0, yy + 62.0, 8.0, accent(seed));
    }
    sheet
}

