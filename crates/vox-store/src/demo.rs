//! Demo data (persistence.md §8, contracts.md §7.3): the seeded history of demo/screenshot mode and the canned
//! voices/models/account served when demo mode has no key. Deterministic: output depends only on `now`, the time
//! zone and `count`; the `rand()` call order of the spec must be kept exactly (test vectors in persistence.md §12).
//!
//! Word counts (ARCHITECTURE.md §8 D-P-14): the generator sets `word_count =
//! vox_core::history::count_whitespace_words(text)` (persistence.md §8.4 `countWords`, whitespace tokens), **not** the
//! reader's `count_words` — four snippets count differently under the two rules (Council 81 vs 82, Tenancy 123 vs
//! 127, …) and the §12 vectors (totals.words 9171/15235/2036, avgWordsPerRead 129/127, avgListenWpm,
//! readingTimeEquivalentSec 2117/3491) depend on the whitespace rule.

use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Weekday};
use vox_core::constants::{APP_NAME, DAY_MS, ESTIMATED_CHARS_PER_SECOND};
use vox_core::history::{count_whitespace_words, ReadRecord, ReadSource};
use vox_core::js::round1;
use vox_core::text16::utf16_len;
use vox_core::time::{add_days, from_ms, local_midnight, to_ms};
use vox_core::voices::{AccountInfo, ModelLanguage, TtsModel, Voice};

/// mulberry32 (seed `0x5EED70C5` for the demo history).
#[derive(Debug, Clone)]
pub struct Mulberry32 {
    a: u32,
}

impl Mulberry32 {
    pub fn new(seed: u32) -> Self {
        Self { a: seed }
    }
    /// Next value in `[0, 1)`.
    pub fn next_f64(&mut self) -> f64 {
        self.a = self.a.wrapping_add(0x6D2B_79F5);
        let mut t = self.a;
        t = (t ^ (t >> 15)).wrapping_mul(t | 1);
        t ^= t.wrapping_add((t ^ (t >> 7)).wrapping_mul(t | 61));
        ((t ^ (t >> 14)) as f64) / 4_294_967_296.0
    }
}

const SEED: u32 = 0x5EED_70C5;
const SPAN_DAYS: i64 = 60;
const REPLAYS: usize = 3;
const ERRORS: [&str; 2] =
    ["Rate limited by ElevenLabs. Try again in a moment.", "Network error. Check your connection and try again."];
/// Day offsets (0 = today) with no reads: a current streak of 9 and a longest of 18.
const GAP_DAYS: [i64; 8] = [9, 13, 14, 33, 41, 42, 49, 55];
const HOUR_MS: f64 = 3_600_000.0;
const MINUTE_MS: f64 = 60_000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    News,
    Web,
    Research,
    Email,
    Doc,
    Notes,
    Fiction,
    Chat,
}

struct Snippet {
    kind: Kind,
    title: &'static str,
    url: Option<&'static str>,
    text: &'static str,
}

struct AppDef {
    name: &'static str,
    process: &'static str,
    weight: f64,
    kinds: &'static [Kind],
    browser: bool,
    /// Window title = `prefix + snippet title + suffix`.
    title: (&'static str, &'static str),
}

impl AppDef {
    fn window_title(&self, snippet_title: &str) -> String {
        format!("{}{snippet_title}{}", self.title.0, self.title.1)
    }
}

const APPS: [AppDef; 9] = [
    AppDef { name: "Chrome", process: "chrome.exe", weight: 35.0, kinds: &[Kind::News, Kind::Web, Kind::Research], browser: true, title: ("", " - Google Chrome") },
    AppDef { name: "Edge", process: "msedge.exe", weight: 11.0, kinds: &[Kind::News, Kind::Web, Kind::Research], browser: true, title: ("", " - Microsoft Edge") },
    AppDef { name: "Word", process: "winword.exe", weight: 11.0, kinds: &[Kind::Doc], browser: false, title: ("", ".docx - Word") },
    AppDef { name: "Acrobat", process: "acrobat.exe", weight: 9.0, kinds: &[Kind::Research, Kind::Doc], browser: false, title: ("", ".pdf - Adobe Acrobat") },
    AppDef { name: "Notion", process: "notion.exe", weight: 8.0, kinds: &[Kind::Doc, Kind::Notes], browser: false, title: ("", "") },
    AppDef { name: "Outlook", process: "olk.exe", weight: 9.0, kinds: &[Kind::Email], browser: false, title: ("", " - Outlook") },
    AppDef { name: "Slack", process: "slack.exe", weight: 6.0, kinds: &[Kind::Chat], browser: false, title: ("Slack - ", "") },
    AppDef { name: "Kindle", process: "kindle.exe", weight: 6.0, kinds: &[Kind::Fiction], browser: false, title: ("Kindle - ", "") },
    AppDef { name: "Obsidian", process: "obsidian.exe", weight: 5.0, kinds: &[Kind::Notes, Kind::Research], browser: false, title: ("", " - Obsidian") },
];

/// (id, name, weight)
const VOICES: [(&str, &str, f64); 4] = [
    ("JBFqnCBsd6RMkjVDRZzb", "George", 40.0),
    ("EXAVITQu4vr4xnSDxMaL", "Sarah", 25.0),
    ("nPczCjzI2devNBz1zQrb", "Brian", 20.0),
    ("XB0fDUnXU5powFXDhCwa", "Charlotte", 15.0),
];

const MODELS: [(&str, f64); 3] = [("eleven_flash_v2_5", 70.0), ("eleven_multilingual_v2", 20.0), ("eleven_turbo_v2_5", 10.0)];

const BROWSER_SOURCES: [(ReadSource, f64); 5] = [
    (ReadSource::ContextChip, 38.0),
    (ReadSource::BrowserExtension, 32.0),
    (ReadSource::Hotkey, 20.0),
    (ReadSource::SelectionPopup, 7.0),
    (ReadSource::Clipboard, 3.0),
];

const DESKTOP_SOURCES: [(ReadSource, f64); 4] =
    [(ReadSource::ContextChip, 50.0), (ReadSource::Hotkey, 32.0), (ReadSource::SelectionPopup, 10.0), (ReadSource::Clipboard, 8.0)];

/// (rate, weight)
const RATES: [(f64, f64); 4] = [(1.0, 55.0), (1.25, 25.0), (1.5, 15.0), (2.0, 5.0)];

/// `weighted(items)`: `r = rand() × Σweight`; the first item where `r` drops below 0; else the last.
fn weighted<'a, T>(rand: &mut Mulberry32, items: &'a [T], weight: impl Fn(&T) -> f64) -> &'a T {
    let total = items.iter().fold(0.0, |s, i| s + weight(i));
    let mut r = rand.next_f64() * total;
    for item in items {
        r -= weight(item);
        if r < 0.0 {
            return item;
        }
    }
    &items[items.len() - 1]
}

/// `pick(items)` = `items[floor(rand() × len)]`.
fn pick<'a, T>(rand: &mut Mulberry32, items: &[&'a T]) -> &'a T {
    let i = (rand.next_f64() * items.len() as f64).floor() as usize;
    items[i.min(items.len() - 1)]
}

/// `uuid()`: 32 random hex digits; digit 12 := 4, digit 16 := (d & 3) | 8.
fn uuid(rand: &mut Mulberry32) -> String {
    let mut digits: Vec<u32> = (0..32).map(|_| (rand.next_f64() * 16.0).floor() as u32).collect();
    digits[12] = 4;
    digits[16] = (digits[16] & 0x3) | 0x8;
    let hex: String = digits.iter().map(|d| char::from_digit(*d, 16).unwrap_or('0')).collect();
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

/// `allocate(count, weights)` (largest remainder, then single reads moved into empty active days).
fn allocate(count: usize, weights: &[f64]) -> Vec<usize> {
    let total = weights.iter().fold(0.0, |s, w| s + w);
    if total <= 0.0 || count == 0 {
        return vec![0; weights.len()];
    }
    let exact: Vec<f64> = weights.iter().map(|w| (w / total) * count as f64).collect();
    let mut out: Vec<usize> = exact.iter().map(|e| e.floor() as usize).collect();
    let mut left = count.saturating_sub(out.iter().sum());
    let mut order: Vec<usize> = (0..exact.len()).collect();
    order.sort_by(|&a, &b| {
        let fa = exact[a] - exact[a].floor();
        let fb = exact[b] - exact[b].floor();
        fb.partial_cmp(&fa).unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut k = 0;
    while left > 0 {
        out[order[k]] += 1;
        k = (k + 1) % order.len();
        left -= 1;
    }
    for i in 0..out.len() {
        if weights[i] <= 0.0 || out[i] > 0 {
            continue;
        }
        let mut donor = 0;
        for j in 1..out.len() {
            if out[j] > out[donor] {
                donor = j;
            }
        }
        if out[donor] <= 1 {
            break;
        }
        out[donor] -= 1;
        out[i] += 1;
    }
    out
}

/// Hour of day for a read: morning (7–10) and evening (19–23) peaks, a little daytime.
fn time_of_day_ms(rand: &mut Mulberry32) -> f64 {
    let r = rand.next_f64();
    let (start, span) = if r < 0.45 {
        (7.0, 3.0)
    } else if r < 0.85 {
        (19.0, 4.9)
    } else {
        (11.0, 7.0)
    };
    (start + rand.next_f64() * span) * HOUR_MS
}

struct Origin {
    snippet: &'static Snippet,
    app: Option<&'static AppDef>,
    source: ReadSource,
}

fn choose_origin(rand: &mut Mulberry32) -> Origin {
    let roll = rand.next_f64();
    if roll < 0.04 {
        let all: Vec<&Snippet> = SNIPPETS.iter().collect();
        return Origin { snippet: pick(rand, &all), app: None, source: ReadSource::Manual };
    }
    let app = weighted(rand, &APPS, |a| a.weight);
    let candidates: Vec<&Snippet> = SNIPPETS.iter().filter(|s| app.kinds.contains(&s.kind)).collect();
    let snippet = pick(rand, &candidates);
    let sources: &[(ReadSource, f64)] = if app.browser { &BROWSER_SOURCES } else { &DESKTOP_SOURCES };
    let mut source = weighted(rand, sources, |s| s.1).0;
    if source == ReadSource::BrowserExtension && snippet.url.is_none() {
        source = ReadSource::ContextChip;
    }
    Origin { snippet, app: Some(app), source }
}

fn sort_newest_first(records: &mut [ReadRecord]) {
    records.sort_by(|a, b| b.created_at.partial_cmp(&a.created_at).unwrap_or(std::cmp::Ordering::Equal));
}

/// `generateDemoRecords(now, count = 120)` (persistence.md §8.5), newest first.
pub fn generate_demo_records<Tz: TimeZone>(now: DateTime<Tz>, count: usize) -> Vec<ReadRecord> {
    let total = count;
    if total == 0 {
        return Vec::new();
    }
    let tz = now.timezone();
    let mut rand = Mulberry32::new(SEED);
    let now_ms = to_ms(&now);
    let today: NaiveDate = now.date_naive();
    let midnight = |date: NaiveDate| to_ms(&local_midnight(&tz, date));
    let ms_into_today = now_ms - midnight(today);

    let replay_count = if total >= 20 { REPLAYS } else { 0 };
    let error_count = if total >= 20 { ERRORS.len() } else { 0 };
    let base_count = total - replay_count;

    // 1. Per-day weights: weekday-heavy, growing toward the present, today scaled by how much of it has passed.
    //    rand() is consumed for every day, gap days included.
    let mut weights = Vec::with_capacity(SPAN_DAYS as usize);
    for offset in 0..SPAN_DAYS {
        let day = add_days(today, -offset);
        let mut w = if GAP_DAYS.contains(&offset) { 0.0 } else { 1.0 };
        if matches!(day.weekday(), Weekday::Sat | Weekday::Sun) {
            w *= 0.75;
        }
        w *= 0.7 + 0.6 * (1.0 - offset as f64 / (SPAN_DAYS - 1) as f64);
        w *= 0.6 + rand.next_f64() * 0.8;
        if offset == 0 {
            w = if ms_into_today < 30.0 * MINUTE_MS { 0.0 } else { w * (ms_into_today / (16.0 * HOUR_MS)).min(1.0) };
        }
        weights.push(w);
    }
    let per_day = allocate(base_count, &weights);

    // 2. The reads.
    let mut records: Vec<ReadRecord> = Vec::with_capacity(total);
    for offset in 0..SPAN_DAYS {
        let day_start = midnight(add_days(today, -offset));
        // Clamp to the calendar day: on a 23-hour DST day a late evening time would spill into the next day.
        let day_end = midnight(add_days(today, -offset + 1));
        for _ in 0..per_day[offset as usize] {
            let mut created_at = (day_start + time_of_day_ms(&mut rand)).min(day_end - MINUTE_MS);
            if offset == 0 && created_at >= now_ms - MINUTE_MS {
                let earliest = if ms_into_today > 8.0 * HOUR_MS { 7.0 * HOUR_MS } else { 0.0 };
                created_at = day_start + earliest + rand.next_f64() * (ms_into_today - 2.0 * MINUTE_MS - earliest).max(0.0);
            }
            let created_at = created_at.floor();
            let Origin { snippet, app, source } = choose_origin(&mut rand);
            let voice = weighted(&mut rand, &VOICES, |v| v.2);
            let rate = weighted(&mut rand, &RATES, |r| r.1).0;
            let len16 = utf16_len(snippet.text) as f64;
            let audio = round1((len16 / ESTIMATED_CHARS_PER_SECOND) * (0.92 + rand.next_f64() * 0.16));
            let completed = rand.next_f64() < 0.8;
            let listened =
                if completed { round1(audio / rate) } else { round1((audio * (0.08 + rand.next_f64() * 0.8)) / rate) };
            // In the original object-literal order: id, then modelId, then favorite.
            let id = uuid(&mut rand);
            let model_id = weighted(&mut rand, &MODELS, |m| m.1).0;
            let favorite = rand.next_f64() < 0.1;

            let mut rec = ReadRecord::new(id, created_at, source);
            rec.text = snippet.text.to_string();
            rec.char_count = len16 as u64;
            rec.word_count = count_whitespace_words(snippet.text);
            rec.voice_id = voice.0.to_string();
            rec.voice_name = voice.1.to_string();
            rec.model_id = model_id.to_string();
            rec.audio_duration_sec = audio;
            rec.listened_sec = listened;
            rec.completed = completed;
            rec.characters_billed = len16 as u64;
            rec.favorite = favorite;
            if let Some(app) = app {
                rec.source_app = Some(app.name.to_string());
                rec.source_process = Some(app.process.to_string());
                rec.source_title = Some(if source == ReadSource::BrowserExtension {
                    snippet.title.to_string()
                } else {
                    app.window_title(snippet.title)
                });
                if source == ReadSource::BrowserExtension {
                    rec.source_url = snippet.url.map(str::to_string);
                }
            }
            records.push(rec);
        }
    }
    sort_newest_first(&mut records);

    // 3. A couple of failed reads (not billed, nothing heard) among the older partial listens.
    let partials: Vec<usize> = (0..records.len()).filter(|&i| !records[i].completed && !records[i].favorite).skip(3).collect();
    for i in 0..error_count.min(partials.len()) {
        let idx = partials.get(i * 3).copied().unwrap_or(partials[i]);
        let r = &mut records[idx];
        r.error = Some(ERRORS[i].to_string());
        r.listened_sec = 0.0;
        r.audio_duration_sec = 0.0;
        r.characters_billed = 0;
    }

    // 4. Free replays of earlier reads, a day or more after the original, placed next to an existing read so they
    //    never fill one of the deliberate gap days.
    let originals: Vec<ReadRecord> =
        records.iter().filter(|r| r.error.is_none() && r.created_at < now_ms - 3.0 * DAY_MS).cloned().collect();
    let same_day = |a: f64, b: f64| from_ms(&tz, a).date_naive() == from_ms(&tz, b).date_naive();
    let mut made = 0;
    while made < replay_count && !originals.is_empty() {
        made += 1;
        let orig = &originals[((rand.next_f64() * originals.len() as f64).floor() as usize).min(originals.len() - 1)];
        let anchors: Vec<f64> = records
            .iter()
            .filter(|r| r.source != ReadSource::Replay && r.created_at > orig.created_at + DAY_MS)
            .map(|r| r.created_at)
            .collect();
        if anchors.is_empty() {
            continue;
        }
        let anchor = anchors[((rand.next_f64() * anchors.len() as f64).floor() as usize).min(anchors.len() - 1)];
        let shift = ((2.0 + rand.next_f64() * 18.0) * MINUTE_MS).floor();
        let mut later = anchor + shift;
        if !same_day(later, anchor) || later >= now_ms - MINUTE_MS {
            later = anchor - shift;
        }
        if !same_day(later, anchor) {
            later = anchor - 1.0;
        }
        let audio = if orig.audio_duration_sec != 0.0 {
            orig.audio_duration_sec
        } else {
            round1(orig.char_count as f64 / ESTIMATED_CHARS_PER_SECOND)
        };
        let mut replay = orig.clone();
        replay.id = uuid(&mut rand);
        replay.created_at = later;
        replay.source = ReadSource::Replay;
        replay.audio_duration_sec = audio;
        replay.listened_sec = round1(audio);
        replay.completed = true;
        replay.characters_billed = 0;
        replay.favorite = false;
        if orig.source_app.is_none() {
            replay.source_app = Some(APP_NAME.to_string());
        }
        records.push(replay);
    }
    sort_newest_first(&mut records);
    records
}

/// The 8 demo voices (contracts.md §7.3), all premade, in the table's order (Liam = `TX3LPaxmHKxFdv7VOQHJ` …):
/// `voiceId`, `name`, category `premade`, `description`, labels `{accent, age, gender, use_case}` exactly as listed.
pub fn demo_voices() -> Vec<Voice> {
    const ROWS: [[&str; 7]; 8] = [
        ["JBFqnCBsd6RMkjVDRZzb", "George", "Warm, captivating storyteller", "British", "middle aged", "male", "narration"],
        ["EXAVITQu4vr4xnSDxMaL", "Sarah", "Mature, reassuring, confident", "American", "young", "female", "news"],
        ["nPczCjzI2devNBz1zQrb", "Brian", "Deep, resonant and comforting", "American", "middle aged", "male", "narration"],
        ["XB0fDUnXU5powFXDhCwa", "Charlotte", "Relaxed and soothing", "Swedish", "young", "female", "characters"],
        ["onwK4e9ZLuTAKqWW03F9", "Daniel", "Steady broadcaster", "British", "middle aged", "male", "news"],
        ["pFZP5JQG7iQjIQuC4Bku", "Lily", "Velvety actress", "British", "middle aged", "female", "narration"],
        ["cgSgspJ2msm6clMCkdW9", "Jessica", "Playful, bright, warm", "American", "young", "female", "conversational"],
        ["TX3LPaxmHKxFdv7VOQHJ", "Liam", "Energetic, social media creator", "American", "young", "male", "social media"],
    ];
    ROWS.iter()
        .map(|[id, name, description, accent, age, gender, use_case]| Voice {
            voice_id: id.to_string(),
            name: name.to_string(),
            labels: vec![
                ("accent".to_string(), accent.to_string()),
                ("age".to_string(), age.to_string()),
                ("gender".to_string(), gender.to_string()),
                ("use_case".to_string(), use_case.to_string()),
            ],
            category: Some("premade".to_string()),
            description: Some(description.to_string()),
            preview_url: None,
        })
        .collect()
}

/// The 4 demo models (contracts.md §7.3), in order: `modelId`, `name`, `description`, `maxChars`
/// 40000/40000/10000/5000, `costFactor` 0.5/0.5/1/1, `languages = [{id: "en", name: "English"}]`.
pub fn demo_models() -> Vec<TtsModel> {
    const ROWS: [(&str, &str, &str, f64, f64); 4] = [
        ("eleven_flash_v2_5", "Eleven Flash v2.5", "Ultra low latency, 32 languages", 40_000.0, 0.5),
        ("eleven_turbo_v2_5", "Eleven Turbo v2.5", "Balanced quality and speed", 40_000.0, 0.5),
        ("eleven_multilingual_v2", "Eleven Multilingual v2", "Most life-like, rich emotion", 10_000.0, 1.0),
        ("eleven_v3", "Eleven v3", "Most expressive", 5_000.0, 1.0),
    ];
    ROWS.iter()
        .map(|(id, name, description, max_chars, cost)| TtsModel {
            model_id: id.to_string(),
            name: name.to_string(),
            description: Some(description.to_string()),
            languages: vec![ModelLanguage { id: "en".into(), name: "English".into() }],
            max_chars: Some(*max_chars),
            cost_factor: Some(*cost),
        })
        .collect()
}

/// `{tier:'creator', characterCount:61240, characterLimit:100000, nextResetUnix: now_s + 12 × 86 400,
/// status:'active'}`.
pub fn demo_account(now_ms: f64) -> AccountInfo {
    let now_s = if now_ms.is_finite() { (now_ms / 1000.0).floor() } else { 0.0 };
    AccountInfo {
        tier: "creator".into(),
        character_count: 61_240.0,
        character_limit: 100_000.0,
        next_reset_unix: Some(now_s + 12.0 * 86_400.0),
        status: Some("active".into()),
    }
}

/// The demo snippets (persistence.md Appendix A, verbatim, in order).
const SNIPPETS: [Snippet; 27] = [
    Snippet {
        kind: Kind::News,
        title: "Harlow Bay tests floating breakwaters as winter storms intensify",
        url: Some("https://coastalledger.net/2026/harlow-bay-floating-breakwaters"),
        text: "Harlow Bay will spend the winter as a test site for a new kind of sea defence: a chain of anchored concrete pontoons that rise and fall with the tide instead of standing against it. Engineers say the floating breakwaters absorb wave energy before it reaches the harbour wall, which has been overtopped four times since January. The pilot covers a 300-metre stretch outside the fishing quay and costs a fraction of raising the existing wall. Local skippers are cautiously supportive. \"If it keeps the slipway open in February, I don't care what it looks like,\" said one, who has fished from the bay for thirty years. Sensors on each pontoon will report movement and strain every ten minutes, and the council has promised to publish the data monthly.",
    },
    Snippet {
        kind: Kind::News,
        title: "Council approves overnight bus network after two-year trial",
        url: Some("https://northfieldgazette.net/local/overnight-buses-approved"),
        text: "The city council voted on Tuesday to make its overnight bus network permanent after a two-year trial that carried more than 1.4 million passengers. Five routes will run every thirty minutes between midnight and five in the morning, linking the hospital district, the airport and the three largest residential neighbourhoods. Night-shift workers made up almost half of all trips during the trial, according to the transport office. Councillors who opposed the plan questioned the running costs, but the motion passed comfortably.",
    },
    Snippet {
        kind: Kind::News,
        title: "Battery recycling plant opens on the site of a former steelworks",
        url: Some("https://industrydispatch.net/energy/battery-recycling-plant-opens"),
        text: "A battery recycling plant opened this week on land that once held one of the region's largest steelworks, bringing around 220 jobs to a town that lost thousands when the furnaces closed. The facility shreds spent lithium-ion cells from cars and power tools into a powder known as black mass, then uses a water-based process to recover lithium, nickel and cobalt. The operator says recovered metals are close enough to mined material to go straight back into new cells. Capacity in the first year is modest, roughly the batteries from forty thousand cars, but the site has room for a second line. Former steelworkers make up a fifth of the new workforce, many retrained through a programme run with the local college. Environmental groups welcomed the plant while urging regulators to monitor water use closely, since the chemical stage requires large volumes that must be treated before release. The company expects to reach full output by the end of next year.",
    },
    Snippet {
        kind: Kind::News,
        title: "Heatwave pushes regional grid to record evening demand",
        url: Some("https://coastalledger.net/2026/heatwave-grid-record-demand"),
        text: "Electricity demand reached a record on Wednesday evening as temperatures stayed above thirty degrees well after sunset, keeping air conditioners running across the region. Grid operators said the peak arrived later than usual, around half past eight, after solar output had already faded. Large batteries installed over the past two years supplied almost a tenth of demand during the critical hour, and no rolling outages were needed. Residents have been asked to run dishwashers and washing machines after ten at night until the heat breaks.",
    },
    Snippet {
        kind: Kind::News,
        title: "Small libraries are lending drills, sewing machines and telescopes",
        url: Some("https://northfieldgazette.net/features/library-of-things"),
        text: "On a Saturday morning the busiest shelf at the Eastgate branch library is not fiction or travel but a row of labelled plastic boxes holding a pressure washer, two sewing machines, a tile cutter and a surprisingly popular telescope. The \"library of things\" started three years ago with a handful of donated tools and now lends more than four hundred items. Members borrow them for a week, just like a book, and late returns are rare. Staff say the idea grew out of a simple observation: most households own a drill that is used for a few minutes a year. Lending one out a hundred times saves money, cupboard space and a surprising amount of plastic packaging. The collection now includes a projector, camping stoves, a carpet cleaner and a set of jam-making pans that is booked solid every autumn. Volunteers run repair sessions on the first Sunday of each month, and the most common fix is a frayed power cable. Several neighbouring towns have asked for advice on starting their own collections.",
    },
    Snippet {
        kind: Kind::Web,
        title: "Better pour-over coffee in five small steps",
        url: Some("https://slowmorningnotes.com/pour-over-basics"),
        text: "Good pour-over coffee is mostly about consistency. Start with fresh beans and grind them just before brewing, roughly as coarse as sea salt. Use about sixty grams of coffee for every litre of water, and weigh both rather than guessing with scoops. Heat the water to just off the boil, around ninety-four degrees. Rinse the paper filter first to remove any papery taste, then add the grounds and pour just enough water to wet them. Wait thirty seconds while the coffee blooms and releases gas. Pour the rest slowly in small circles, keeping the bed evenly submerged, and aim to finish draining at about three and a half minutes. If the cup tastes sour, grind finer; if it tastes bitter or dry, grind coarser. Change one thing at a time and take notes.",
    },
    Snippet {
        kind: Kind::Web,
        title: "Why your sourdough starter smells like nail polish",
        url: Some("https://flourandpatience.com/starter-smells"),
        text: "If your starter suddenly smells sharp, like nail polish remover, it is hungry rather than ruined. As the yeast and bacteria run out of fresh flour they produce more acetic acid and other volatile compounds. Feed it more often, or use a larger proportion of fresh flour and water at each feed, and the smell usually fades within a couple of days. A layer of grey liquid on top means the same thing and can simply be poured off or stirred back in.",
    },
    Snippet {
        kind: Kind::Web,
        title: "Ten minutes at a time: the quiet case for micro-walks",
        url: Some("https://slowmorningnotes.com/micro-walks"),
        text: "You do not need an hour at the gym to feel the benefit of moving. Short walks of around ten minutes, spread through the day, lower blood sugar after meals, loosen stiff hips and give your eyes a break from the screen. The trick is to attach them to things you already do: after lunch, between meetings, before opening email in the morning. Leave the headphones behind once in a while. Boredom is where a surprising number of good ideas come from.",
    },
    Snippet {
        kind: Kind::Research,
        title: "Urban tree canopy and land surface temperature in mid-sized cities",
        url: Some("https://openfieldjournal.org/articles/urban-canopy-surface-temperature"),
        text: "Abstract. Urban heat is increasingly recognised as a public health risk, yet most studies of mitigation focus on large metropolitan areas. We examined the relationship between tree canopy cover and summer land surface temperature across twenty-three mid-sized cities using satellite thermal imagery from five consecutive summers and high-resolution canopy maps. At the neighbourhood scale, each ten percentage point increase in canopy cover was associated with a reduction of 1.2 °C in mean afternoon surface temperature, after controlling for building density, impervious surface and distance to water. The cooling effect was non-linear: gains were largest when canopy rose from very low levels and flattened above roughly forty percent cover. Neighbourhoods with lower median incomes had, on average, half the canopy of wealthier areas and were consistently the hottest. Street trees planted within the past decade contributed measurably less cooling than mature trees, highlighting the long lead time of planting programmes. We discuss implications for prioritising planting in the hottest, least-shaded districts and for protecting existing mature trees during development.",
    },
    Snippet {
        kind: Kind::Research,
        title: "Spaced retrieval practice and vocabulary retention in adult learners",
        url: None,
        text: "Abstract. We compared spaced retrieval practice with massed rereading in a twelve-week study of 184 adults learning Portuguese vocabulary online. Participants in the retrieval condition attempted to recall each word before seeing its translation, at intervals that expanded from one day to three weeks. The rereading group spent the same total time reviewing word lists. On a delayed test eight weeks after the final session, the retrieval group recalled 61 percent of items compared with 34 percent for rereading. The advantage held across age groups and prior language experience, although participants consistently rated rereading as the more effective strategy. These findings suggest that learners' intuitions about study methods are an unreliable guide and that tools should schedule retrieval rather than leave it to preference.",
    },
    Snippet {
        kind: Kind::Research,
        title: "Methods: low-cost soil moisture sensing in allotment gardens",
        url: None,
        text: "Methods. Capacitive soil moisture probes were installed at depths of 10 cm and 30 cm in forty-two raised beds across six allotment sites. Each probe was paired with a microcontroller that recorded readings every fifteen minutes and transmitted them over a low-power radio link to a gateway at each site. Before deployment, probes were calibrated against gravimetric samples taken from the same soil, giving a calibration curve for each site. Gardeners kept paper diaries of watering times and approximate volumes, which were later digitised and matched against the sensor record. Beds were excluded from analysis when more than twenty percent of readings were missing in any given month.",
    },
    Snippet {
        kind: Kind::Research,
        title: "Discussion: meeting load after the shift to hybrid work",
        url: None,
        text: "Discussion. Our results indicate that the move to hybrid work did not reduce meeting load so much as redistribute it. Scheduled meetings fell by eleven percent, but short unscheduled calls more than doubled, and the share of the working day with no calendar commitments fell from 38 to 29 percent. Employees who spent three or more days in the office reported the longest uninterrupted blocks of focused work, largely because colleagues could resolve small questions in person rather than booking a call. These effects were strongest for team leads, whose calendars absorbed much of the coordination that had previously happened informally. We caution against reading these findings as an argument for full office attendance: self-reported wellbeing was highest among people with two office days a week. Instead, the data point to the value of protected focus time agreed at team level, and to the hidden cost of replacing quick conversations with calendar invitations.",
    },
    Snippet {
        kind: Kind::Email,
        title: "Re: Venue options for the March offsite",
        url: None,
        text: "Hi Priya, thanks for pulling the shortlist together. I've had a look at all three. The converted mill is lovely but the main room only seats thirty in a horseshoe, which is tight if the design team joins us. The hotel option is easiest for travel, though the breakout rooms have no daylight and we know how that goes by mid-afternoon. My vote is the farmhouse: two decent rooms, good food and a train station fifteen minutes away. Could you check whether they can hold the second week of March and whether there is space for a few people to stay on site? Happy to call them myself if that's easier. Thanks again, Sam",
    },
    Snippet {
        kind: Kind::Email,
        title: "Invoice 2291 — payment received",
        url: None,
        text: "Hello, this is a quick note to confirm we have received your payment for invoice 2291. Thank you. A receipt is attached for your records, and no further action is needed. If any details on the receipt need correcting, simply reply to this email and we will reissue it within two working days.",
    },
    Snippet {
        kind: Kind::Email,
        title: "Weekly product update — sync, search and a new export",
        url: None,
        text: "Hi all, here's what shipped this week and what is coming next. Sync: the new conflict resolver is now live for everyone. Edits made offline on two devices are merged paragraph by paragraph instead of keeping both copies, and early numbers show duplicate documents down by roughly eighty percent. Please keep forwarding odd cases to the support channel. Search: results now update as you type and include matches inside attachments. Indexing large workspaces takes a few minutes the first time, so you may see a progress note at the top of the panel. Export: by popular demand, you can export any folder as a single PDF with a table of contents. Page breaks follow your headings, and images keep their original resolution. Next week we're focusing on performance for workspaces with more than ten thousand documents and on the long-requested dark theme for the mobile apps. Thanks to everyone who joined the testing sessions on Thursday — your notes shaped most of the changes above. As always, reply here with questions. Have a good weekend, Lena",
    },
    Snippet {
        kind: Kind::Email,
        title: "Quick favour — could you review the draft?",
        url: None,
        text: "Hey, quick favour if you have twenty minutes this week. I've attached the draft of the grant summary. Mostly I need fresh eyes on the budget section, which I've rewritten three times and can no longer see clearly. No need for line edits, just flag anything that doesn't make sense. Thank you so much!",
    },
    Snippet {
        kind: Kind::Doc,
        title: "Onboarding checklist — Support team",
        url: None,
        text: "Welcome to the support team. Your first week is about learning the product and the people, not closing tickets. Day one: collect your laptop, set up your accounts and join the team channel. Your buddy will walk you through the help desk and the internal knowledge base. Day two and three: shadow two experienced colleagues for at least three hours each, and keep a list of every question you have, however small. Day four: answer your first five tickets in draft mode, with your buddy reviewing replies before they are sent. Day five: join the weekly review, where we look at the trickiest conversations from the past week and agree on how to handle similar cases. By the end of the month you should be comfortable with billing questions, account recovery and the most common integration issues. Please ask for help early; nobody expects you to know everything yet.",
    },
    Snippet {
        kind: Kind::Doc,
        title: "Q4 planning memo",
        url: None,
        text: "Purpose. This memo sets out the priorities for the fourth quarter and the trade-offs we agreed during last week's planning sessions. It is meant to be read before Monday's all-hands so that the meeting can focus on questions rather than presentation. Priorities. First, reliability. The two outages in September cost us more goodwill than any feature launch has earned this year, and we will dedicate a third of engineering time to the incident follow-ups, better alerting and a tested failover for the primary database. Second, onboarding. New accounts that do not complete setup within three days rarely return, and the current flow asks for too much too early. The growth team will ship a shorter setup with sensible defaults by mid-November. Third, the partner programme, which will move from pilot to general availability with a self-serve application form. What we are not doing. We are pausing work on the advanced reporting module until January. It remains important, but splitting attention across four large projects is how we ended up with none of them finished last quarter. Risks. Hiring for two senior roles is behind schedule, and the holiday period shortens the quarter by roughly two weeks. Owners for each priority will share a one-page plan with milestones by Friday.",
    },
    Snippet {
        kind: Kind::Doc,
        title: "Tenancy agreement — Section 7, Repairs and maintenance",
        url: None,
        text: "7.1 The landlord shall keep in repair the structure and exterior of the property, including drains, gutters and external pipes, and shall keep in proper working order the installations for the supply of water, gas, electricity, sanitation and heating. 7.2 The tenant shall notify the landlord in writing of any defect or disrepair within a reasonable time of becoming aware of it. 7.3 The landlord or their agent may enter the property to inspect its condition or carry out repairs, provided that at least twenty-four hours' written notice has been given, except in an emergency. 7.4 The tenant shall not be responsible for fair wear and tear, nor for damage caused by an insured risk unless that damage results from the tenant's negligence.",
    },
    Snippet {
        kind: Kind::Doc,
        title: "Design review notes — widget redesign",
        url: None,
        text: "Attendees agreed the new player card reads clearly at a glance, but the progress dots are too small on high-density screens and should scale with the card. The red accent on the live indicator works; using it for the stop button as well felt alarming and will be removed. Action items: test the compact layout with very long titles, add keyboard shortcuts to the tooltip copy, and check contrast for the grey \"buffered\" dots on the paper theme. Next review is on Thursday.",
    },
    Snippet {
        kind: Kind::Notes,
        title: "Reading notes — attention and deep work",
        url: None,
        text: "Main idea: attention behaves more like a budget than a muscle. Every switch between tasks leaves a residue that takes several minutes to clear, so a day of short gaps between meetings produces almost no deep work even when the gaps add up to hours. Practical takeaways: batch shallow tasks into two windows a day, protect at least one ninety-minute block each morning, and keep a written list of open loops so they stop competing for attention. Worth rereading: the chapter on deliberately scheduling boredom.",
    },
    Snippet {
        kind: Kind::Notes,
        title: "Trip plan — Lisbon long weekend",
        url: None,
        text: "Friday: land around noon, drop bags at the guesthouse in Graça, late lunch nearby, then walk down through Alfama towards the river before sunset. Saturday: tram early before the queues, the tile museum in the afternoon, dinner booked for eight. Sunday: day trip to Sintra by train, leave by nine and pack a rain jacket because the hills make their own weather. Monday: slow breakfast, the market hall, airport by three. Budget roughly what we spent in Porto last spring. Remember adapters and comfortable shoes; everything is uphill.",
    },
    Snippet {
        kind: Kind::Fiction,
        title: "The Lantern Keeper — Chapter 3",
        url: None,
        text: "The storm arrived an hour before it was due, as storms on that coast always did, and by the time Mara reached the top of the tower the glass of the lantern room was streaming with rain. She lit the wick with hands that would not quite keep still. Below her the sea had turned the colour of slate, and somewhere out in it, if the harbourmaster's message was right, a small boat was trying to find its way home. She wound the clockwork that turned the lens and listened to it catch, a sound her father used to call the lighthouse clearing its throat. The beam swung out across the water, white and steady, and for a moment she saw the whole bay laid bare: the black teeth of the reef, the foam over the sandbar, the long empty curve of the shore. Then the rain closed in again. She sat on the iron step with her back against the warm brass housing and began to count the turns. Thirty, forty, fifty. On the sixty-third, a second light answered from the dark, low and yellow and bobbing, far closer to the reef than it should have been. Mara was on her feet before she knew she had decided anything. The bell rope was at the bottom of the stairs, and there were one hundred and twelve of them. She had counted those too, every night for eleven years.",
    },
    Snippet {
        kind: Kind::Fiction,
        title: "Salt and Static — Chapter 11",
        url: None,
        text: "The radio had not worked properly since the summer, but Theo kept it on the windowsill out of habit, and on the night the power failed it was the only thing in the flat that still made a sound. He sat on the floor with his back to the cold radiator and turned the dial slowly through the hiss. Mostly there was nothing. Then, near the end of the band, a woman's voice came through, calm and unhurried, reading what sounded like a list of tides. High water at four minutes past two. Low water at twenty past eight. She read them for harbours he had never heard of, towns that might not exist, and he found himself writing them down on the back of an envelope by the light of his phone. When the signal faded he turned the dial back and forth for an hour, but she did not return. In the morning the power came back, and the radio, as if embarrassed, played nothing but static. He kept the envelope anyway.",
    },
    Snippet {
        kind: Kind::Fiction,
        title: "The Orchard at Low Tide",
        url: None,
        text: "Every autumn the tide went out so far that the old orchard came back. Nobody in the village could remember it on dry land; the sea had taken it long before any of them were born. But on the lowest tides of October you could walk out across the ribbed sand and find the stumps in their neat rows, black and smooth as river stones, and if you were lucky, a single crab apple lodged between the roots, bright red and impossibly fresh. Nell's grandmother said they were left there on purpose. Nell, who was eleven and believed in tides more than in ghosts, intended to find out by whom.",
    },
    Snippet {
        kind: Kind::Chat,
        title: "#design-team",
        url: None,
        text: "Morning all! Uploaded the new icon set to the shared folder. The 24px versions are final; the 16px ones still need a pass because the thinner strokes disappear on the dark theme. If anyone has a minute today, I'd love a second pair of eyes on the settings and history icons in particular.",
    },
    Snippet {
        kind: Kind::Chat,
        title: "#launch-room",
        url: None,
        text: "Quick status before standup: the release build passed all checks overnight and the installer is signed. Two things left before we go: the store listing screenshots are still the old theme, and the support macros need the new shortcut names. I can take the screenshots if someone from support owns the macros. Planning to flip the switch at 3pm unless anyone shouts.",
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 17, 12, 0, 0).unwrap()
    }

    #[test]
    fn mulberry32_vector() {
        let mut r = Mulberry32::new(0x5EED_70C5);
        let first: Vec<f64> = (0..5).map(|_| r.next_f64()).collect();
        assert_eq!(first, vec![0.9154872244689614, 0.0947299508843571, 0.13766842219047248, 0.9967876193113625, 0.2670450499281287]);
    }

    #[test]
    fn snippet_table() {
        assert_eq!(SNIPPETS.len(), 27);
        assert_eq!(SNIPPETS.iter().filter(|s| s.url.is_some()).count(), 9);
        // D-P-14: the whitespace rule differs from the reader's for these snippets.
        let council = SNIPPETS.iter().find(|s| s.title.starts_with("Council approves")).unwrap();
        assert_eq!(count_whitespace_words(council.text), 81);
        assert_eq!(vox_core::history::count_words(council.text), 82);
        let tenancy = SNIPPETS.iter().find(|s| s.title.starts_with("Tenancy agreement")).unwrap();
        assert_eq!(count_whitespace_words(tenancy.text), 123);
        assert_eq!(vox_core::history::count_words(tenancy.text), 127);
        // Every app has at least one snippet of its kinds.
        for app in &APPS {
            assert!(SNIPPETS.iter().any(|s| app.kinds.contains(&s.kind)), "{}", app.name);
        }
    }

    #[test]
    fn persistence_vectors() {
        let records = generate_demo_records(now(), 120);
        assert_eq!(records.len(), 120);
        assert_eq!(records.iter().filter(|r| r.source == ReadSource::Replay).count(), 3);
        assert_eq!(records.iter().filter(|r| r.error.is_some()).count(), 2);
        assert_eq!(records.iter().filter(|r| r.favorite).count(), 14);
        assert_eq!(records.iter().filter(|r| r.completed).count(), 99);
        let now_ms = to_ms(&now());
        assert!(records.iter().all(|r| r.created_at < now_ms));
        assert!(records.windows(2).all(|w| w[0].created_at >= w[1].created_at), "newest first");
        assert!(records.iter().all(|r| !r.cached && r.listen_log.is_none() && r.max_position_sec.is_none()));

        let r0 = &records[0];
        assert_eq!(r0.id, "7c134be0-e9c6-4e9e-a122-0a916f3827ff");
        assert_eq!(r0.created_at, 1792229746902.0);
        assert_eq!(vox_core::time::iso_utc_ms(r0.created_at), "2026-10-17T09:35:46.902Z");
        assert_eq!((r0.source, r0.source_app.as_deref()), (ReadSource::ContextChip, Some("Chrome")));
        assert_eq!((r0.voice_name.as_str(), r0.model_id.as_str()), ("Brian", "eleven_multilingual_v2"));
        assert_eq!((r0.audio_duration_sec, r0.listened_sec, r0.completed), (69.3, 69.3, true));
        assert_eq!((r0.char_count, r0.word_count), (969, 153));

        let r1 = &records[1];
        assert_eq!(r1.id, "1157c6bc-ea84-475a-b04b-d46941f67a8d");
        assert_eq!(r1.created_at, 1792220998996.0);
        assert_eq!((r1.source, r1.source_app.as_deref()), (ReadSource::ContextChip, Some("Chrome")));
        assert_eq!((r1.voice_name.as_str(), r1.model_id.as_str()), ("George", "eleven_flash_v2_5"));
        assert_eq!((r1.audio_duration_sec, r1.listened_sec, r1.completed), (45.8, 45.8, true));
        assert_eq!((r1.char_count, r1.word_count), (687, 108));

        let r2 = &records[2];
        assert_eq!(r2.id, "7c333d39-59c5-4346-8aae-d8447f5c5246");
        assert_eq!(r2.created_at, 1792155597089.0);
        assert_eq!(vox_core::time::iso_utc_ms(r2.created_at), "2026-10-16T12:59:57.089Z");
        assert_eq!((r2.source, r2.source_app.as_deref()), (ReadSource::Hotkey, Some("Word")));
        assert_eq!((r2.voice_name.as_str(), r2.model_id.as_str()), ("Charlotte", "eleven_multilingual_v2"));
        assert_eq!((r2.audio_duration_sec, r2.listened_sec, r2.completed), (87.2, 87.2, true));
        assert_eq!((r2.char_count, r2.word_count), (1286, 209));

        let oldest = records.last().unwrap();
        assert_eq!(oldest.id, "94b96364-54a4-43d4-941e-2799bee1e1be");
        assert_eq!(oldest.created_at, 1787126485687.0);
        assert_eq!(oldest.source, ReadSource::ContextChip);
    }

    #[test]
    fn special_records() {
        let records = generate_demo_records(now(), 120);
        let special: Vec<&ReadRecord> =
            records.iter().filter(|r| r.source == ReadSource::Replay || r.error.is_some()).collect();
        let summary: Vec<(&str, f64, ReadSource)> = special.iter().map(|r| (r.id.as_str(), r.created_at, r.source)).collect();
        assert_eq!(
            summary,
            vec![
                ("ff637708-165e-4f12-a24c-e037a7893430", 1790981659614.0, ReadSource::Replay),
                ("cb704166-b49a-4981-a85b-f09e29400b03", 1790761719271.0, ReadSource::Hotkey),
                ("085ab926-5432-4e12-a582-47ee08ac5690", 1790587199843.0, ReadSource::Replay),
                ("3a1c73cd-e159-48fb-9c8b-707c03dd4044", 1789889475246.0, ReadSource::ContextChip),
                ("d71e7ede-2665-462e-8e31-3f886692526c", 1788775017457.0, ReadSource::Replay),
            ]
        );
        let replay_apps: Vec<(Option<&str>, f64, f64, u64)> = special
            .iter()
            .filter(|r| r.source == ReadSource::Replay)
            .map(|r| (r.source_app.as_deref(), r.listened_sec, r.audio_duration_sec, r.characters_billed))
            .collect();
        assert_eq!(
            replay_apps,
            vec![(Some("Notion"), 32.9, 32.9, 0), (Some("Chrome"), 63.2, 63.2, 0), (Some("Word"), 59.9, 59.9, 0)]
        );
        assert!(special.iter().filter(|r| r.source == ReadSource::Replay).all(|r| r.completed && !r.favorite));
        let errors: Vec<(&str, f64, f64, u64)> = special
            .iter()
            .filter_map(|r| r.error.as_deref().map(|e| (e, r.listened_sec, r.audio_duration_sec, r.characters_billed)))
            .collect();
        assert_eq!(
            errors,
            vec![
                ("Rate limited by ElevenLabs. Try again in a moment.", 0.0, 0.0, 0),
                ("Network error. Check your connection and try again.", 0.0, 0.0, 0)
            ]
        );
    }

    #[test]
    fn records_are_canonical_for_the_store() {
        for r in generate_demo_records(now(), 120) {
            assert_eq!(r.clone().normalized(), r);
            let v: serde_json::Value = serde_json::from_str(&r.to_json()).unwrap();
            assert_eq!(ReadRecord::from_value(&v, None).unwrap(), r);
        }
    }

    #[test]
    fn deterministic_and_small_counts() {
        assert_eq!(generate_demo_records(now(), 120), generate_demo_records(now(), 120));
        assert!(generate_demo_records(now(), 0).is_empty());
        let few = generate_demo_records(now(), 10);
        assert_eq!(few.len(), 10);
        assert!(few.iter().all(|r| r.source != ReadSource::Replay && r.error.is_none()));
        let twenty = generate_demo_records(now(), 20);
        assert_eq!(twenty.len(), 20);
        assert_eq!(twenty.iter().filter(|r| r.source == ReadSource::Replay).count(), 3);
    }

    #[test]
    fn gap_days_stay_empty_and_today_is_quiet_early() {
        let records = generate_demo_records(now(), 120);
        let today = now().date_naive();
        let days: std::collections::HashSet<NaiveDate> =
            records.iter().map(|r| from_ms(&Utc, r.created_at).date_naive()).collect();
        for gap in GAP_DAYS {
            assert!(!days.contains(&add_days(today, -gap)), "gap day {gap}");
        }
        // 10 minutes into the day: no reads today yet.
        let early = Utc.with_ymd_and_hms(2026, 10, 17, 0, 10, 0).unwrap();
        let records = generate_demo_records(early, 120);
        assert!(records.iter().all(|r| from_ms(&Utc, r.created_at).date_naive() < early.date_naive()));
    }

    #[test]
    fn allocate_largest_remainder() {
        assert_eq!(allocate(0, &[1.0, 2.0]), vec![0, 0]);
        assert_eq!(allocate(5, &[0.0, 0.0]), vec![0, 0]);
        assert_eq!(allocate(10, &[1.0, 1.0, 2.0]), vec![3, 2, 5]);
        // Every active day gets a read when possible.
        assert_eq!(allocate(3, &[10.0, 0.0, 0.1, 0.1]), vec![1, 0, 1, 1]);
        assert_eq!(allocate(7, &[1.0; 7]).iter().sum::<usize>(), 7);
    }

    #[test]
    fn uuid_shape() {
        let mut r = Mulberry32::new(1);
        for _ in 0..20 {
            let id = uuid(&mut r);
            assert!(vox_core::ids::is_valid_read_id(&id) && id.len() == 36, "{id}");
            assert_eq!(&id[14..15], "4");
            assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"));
        }
    }

    #[test]
    fn demo_catalog_tables() {
        let voices = demo_voices();
        assert_eq!(voices.len(), 8);
        let names: Vec<&str> = voices.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, vec!["George", "Sarah", "Brian", "Charlotte", "Daniel", "Lily", "Jessica", "Liam"]);
        assert_eq!(voices[7].voice_id, "TX3LPaxmHKxFdv7VOQHJ");
        assert!(voices.iter().all(|v| v.category.as_deref() == Some("premade") && v.preview_url.is_none()));
        let json = vox_core::json::to_js_compact(&voices[0]);
        assert_eq!(
            json,
            r#"{"voiceId":"JBFqnCBsd6RMkjVDRZzb","name":"George","labels":{"accent":"British","age":"middle aged","gender":"male","use_case":"narration"},"category":"premade","description":"Warm, captivating storyteller"}"#
        );
        assert_eq!(voices[7].label("use_case"), Some("social media"));
        assert_eq!(voices[3].description.as_deref(), Some("Relaxed and soothing"));
        assert_eq!(voices[3].label("accent"), Some("Swedish"));

        let models = demo_models();
        let rows: Vec<(&str, &str, Option<f64>, Option<f64>)> =
            models.iter().map(|m| (m.model_id.as_str(), m.name.as_str(), m.max_chars, m.cost_factor)).collect();
        assert_eq!(
            rows,
            vec![
                ("eleven_flash_v2_5", "Eleven Flash v2.5", Some(40000.0), Some(0.5)),
                ("eleven_turbo_v2_5", "Eleven Turbo v2.5", Some(40000.0), Some(0.5)),
                ("eleven_multilingual_v2", "Eleven Multilingual v2", Some(10000.0), Some(1.0)),
                ("eleven_v3", "Eleven v3", Some(5000.0), Some(1.0)),
            ]
        );
        assert!(models.iter().all(|m| m.languages == vec![ModelLanguage { id: "en".into(), name: "English".into() }]));
        assert_eq!(models[0].description.as_deref(), Some("Ultra low latency, 32 languages"));

        let a = demo_account(1_792_238_400_123.0);
        assert_eq!((a.tier.as_str(), a.character_count, a.character_limit), ("creator", 61240.0, 100000.0));
        assert_eq!(a.next_reset_unix, Some(1_792_238_400.0 + 12.0 * 86_400.0));
        assert_eq!(a.status.as_deref(), Some("active"));
    }
}
