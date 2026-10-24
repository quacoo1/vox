//! `computeAnalytics` (persistence.md §7): pure, local-time, calendar-day arithmetic. Generic over the time zone so
//! the spec vectors (computed with `TZ=UTC`, persistence.md §12) run as tests with `chrono::Utc`.
//!
//! One deliberate change (BRIEF.md "All time uses one consistent window"; ARCHITECTURE.md §8 D-AN-2): for
//! `AnalyticsRange::All` the `daily` series covers the whole window from the range start to today (no 365-day cap),
//! so every figure on the page uses the same window. Finite ranges are unchanged (exactly N days). The persistence.md
//! §12 vectors still hold (the demo history spans 60 days). The `all` window starts no earlier than
//! `today − (MAX_ALL_DAYS − 1)` days, so a malformed `createdAt` (0, −1e14) cannot build a huge series; records
//! before that start fall outside the range everywhere (streaks excepted: they have no lower bound).
//!
//! Records are iterated in the order given (the caller passes them newest first): float sums are accumulated in that
//! order and rounded only at the very end, like the original, so the spec vectors reproduce bit for bit.
//!
//! Integer counters saturate at `u64::MAX` instead of overflowing: a stored `wordCount` / `charCount` /
//! `charactersBilled` such as `1e300` loads as `u64::MAX` (JS sums doubles and never fails).

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};
use std::hash::Hash;

use chrono::{DateTime, NaiveDate, TimeZone, Timelike};
use vox_core::analytics::{AnalyticsRange, AnalyticsSummary, DailyEntry, SourceCount, Streak, TodayStats, TopApp, TopVoice, Totals};
use vox_core::constants::{ESTIMATED_CHARS_PER_SECOND, MAX_ALL_DAYS, SILENT_READING_WPM, TOP_N};
use vox_core::history::{ReadRecord, ReadSource};
use vox_core::js::{js_round, js_trim, round1};
use vox_core::listen_log::listening_entries;
use vox_core::time::{add_days, day_key_of_date, from_ms, local_midnight, monday_index, to_ms};

/// `nonNeg(v)`: v if finite and > 0, else 0.
fn non_neg(v: f64) -> f64 {
    if v.is_finite() && v > 0.0 {
        v
    } else {
        0.0
    }
}

/// `Math.round` of a non-negative ratio as an integer count.
fn round_count(v: f64) -> u64 {
    let r = js_round(v);
    if r.is_finite() && r > 0.0 {
        r as u64
    } else {
        0
    }
}

/// An insertion-ordered map (JS `Map` iteration order matters for the stable sorts below).
struct Ordered<K, V> {
    keys: Vec<K>,
    map: HashMap<K, V>,
}

impl<K: Eq + Hash + Clone, V> Ordered<K, V> {
    fn new() -> Self {
        Ordered { keys: Vec::new(), map: HashMap::new() }
    }

    fn entry(&mut self, key: &K, init: impl FnOnce() -> V) -> &mut V {
        if !self.map.contains_key(key) {
            self.keys.push(key.clone());
        }
        self.map.entry(key.clone()).or_insert_with(init)
    }

    fn into_vec(mut self) -> Vec<(K, V)> {
        let mut out = Vec::with_capacity(self.keys.len());
        for k in self.keys {
            if let Some(v) = self.map.remove(&k) {
                out.push((k, v));
            }
        }
        out
    }
}

#[derive(Default)]
struct Group {
    reads: u64,
    listened: f64,
}

struct VoiceGroup {
    reads: u64,
    listened: f64,
    name: String,
    name_at: f64,
}

/// `localeCompare` approximation: case-insensitive first, then lower case before upper case (ICU tertiary order).
fn locale_cmp(a: &str, b: &str) -> Ordering {
    a.to_lowercase().cmp(&b.to_lowercase()).then_with(|| b.cmp(a))
}

/// `byListened`: listened (rounded) desc, then reads desc, then name ascending.
fn by_listened(a: (f64, u64, &str), b: (f64, u64, &str)) -> Ordering {
    b.0.partial_cmp(&a.0).unwrap_or(Ordering::Equal).then(b.1.cmp(&a.1)).then_with(|| locale_cmp(a.2, b.2))
}

/// The group key of a read's voice: `voiceId || voiceName || "unknown"`.
fn voice_key(r: &ReadRecord) -> String {
    if !r.voice_id.is_empty() {
        r.voice_id.clone()
    } else if !r.voice_name.is_empty() {
        r.voice_name.clone()
    } else {
        "unknown".to_string()
    }
}

/// The summary for `range` at `now` (records dated after today are ignored everywhere).
pub fn compute_analytics<Tz: TimeZone>(records: &[ReadRecord], range: AnalyticsRange, now: DateTime<Tz>) -> AnalyticsSummary {
    let tz = now.timezone();
    let today = now.date_naive();
    let today_ms = to_ms(&local_midnight(&tz, today));
    let tomorrow_ms = to_ms(&local_midnight(&tz, add_days(today, 1)));
    let valid: Vec<&ReadRecord> = records.iter().filter(|r| r.created_at.is_finite() && r.created_at < tomorrow_ms).collect();

    let start_day = match range.days() {
        Some(n) => add_days(today, -(i64::from(n) - 1)),
        None => {
            let first = valid.iter().fold(today_ms, |m, r| m.min(r.created_at));
            let first_day = from_ms(&tz, first).date_naive();
            first_day.max(add_days(today, -(MAX_ALL_DAYS as i64 - 1)))
        }
    };
    let start_ms = to_ms(&local_midnight(&tz, start_day));

    let mut daily: Vec<DailyEntry> = Vec::new();
    let mut daily_index: HashMap<NaiveDate, usize> = HashMap::new();
    let mut day = start_day;
    while day <= today {
        daily_index.insert(day, daily.len());
        daily.push(DailyEntry { date: day_key_of_date(day), listened_sec: 0.0, reads: 0, words: 0 });
        match day.succ_opt() {
            Some(next) => day = next,
            None => break,
        }
    }

    let mut totals = Totals::default();
    let mut audio_sec = 0.0;
    let mut listened_total = 0.0;
    let mut heatmap = [[0.0f64; 24]; 7];
    let mut apps: Ordered<String, Group> = Ordered::new();
    let mut voices: Ordered<String, VoiceGroup> = Ordered::new();
    let mut sources: Ordered<ReadSource, u64> = Ordered::new();
    let mut heard_words = 0.0;
    let mut heard_listened_sec = 0.0;

    // Pass 1: reads, words and characters count on the day a read was created.
    for r in valid.iter().filter(|r| r.created_at >= start_ms) {
        let words = r.word_count;
        totals.reads += 1;
        totals.words = totals.words.saturating_add(words);
        totals.chars = totals.chars.saturating_add(r.char_count);
        audio_sec += non_neg(r.audio_duration_sec);
        totals.characters_billed = totals.characters_billed.saturating_add(r.characters_billed);
        if r.completed {
            totals.completed_reads += 1;
        }
        heard_words += words as f64 * heard_fraction(r);
        heard_listened_sec += non_neg(r.listened_sec);

        if let Some(&i) = daily_index.get(&from_ms(&tz, r.created_at).date_naive()) {
            daily[i].reads += 1;
            daily[i].words = daily[i].words.saturating_add(words);
        }
        apps.entry(&app_name(r), Group::default).reads += 1;
        let vg = voices.entry(&voice_key(r), || VoiceGroup { reads: 0, listened: 0.0, name: String::new(), name_at: f64::NEG_INFINITY });
        vg.reads += 1;
        if !r.voice_name.is_empty() && r.created_at >= vg.name_at {
            vg.name = r.voice_name.clone();
            vg.name_at = r.created_at;
        }
        *sources.entry(&r.source, || 0) += 1;
    }

    // Pass 2: listening counts when it happened (the record's ledger), for every valid record.
    let mut today_stats = TodayStats::default();
    let mut today_listened = 0.0;
    for r in &valid {
        if r.created_at >= today_ms {
            today_stats.reads += 1;
            today_stats.words = today_stats.words.saturating_add(r.word_count);
        }
        for (at, sec) in listening_entries(&tz, r.created_at, r.listened_sec, r.listen_log.as_ref()) {
            let ms = to_ms(&at);
            if ms < start_ms || ms >= tomorrow_ms {
                continue;
            }
            listened_total += sec;
            if ms >= today_ms {
                today_listened += sec;
            }
            if let Some(&i) = daily_index.get(&at.date_naive()) {
                daily[i].listened_sec += sec;
            }
            heatmap[monday_index(&at)][at.hour() as usize] += sec;
            apps.entry(&app_name(r), Group::default).listened += sec;
            voices
                .entry(&voice_key(r), || VoiceGroup {
                    reads: 0,
                    listened: 0.0,
                    name: r.voice_name.clone(),
                    name_at: if r.voice_name.is_empty() { f64::NEG_INFINITY } else { r.created_at },
                })
                .listened += sec;
        }
    }

    for d in &mut daily {
        d.listened_sec = round1(d.listened_sec);
    }
    for row in &mut heatmap {
        for cell in row.iter_mut() {
            *cell = round1(*cell);
        }
    }
    totals.listened_sec = round1(listened_total);
    totals.audio_sec = round1(audio_sec);
    today_stats.listened_sec = round1(today_listened);

    let mut top_apps: Vec<TopApp> = apps
        .into_vec()
        .into_iter()
        .map(|(name, g)| TopApp { name, reads: g.reads, listened_sec: round1(g.listened) })
        .filter(|a| a.reads > 0 || a.listened_sec > 0.0)
        .collect();
    top_apps.sort_by(|a, b| by_listened((a.listened_sec, a.reads, &a.name), (b.listened_sec, b.reads, &b.name)));
    top_apps.truncate(TOP_N);

    let mut top_voices: Vec<TopVoice> = voices
        .into_vec()
        .into_iter()
        .map(|(key, g)| TopVoice {
            name: if g.name.is_empty() { key.clone() } else { g.name },
            voice_id: key,
            reads: g.reads,
            listened_sec: round1(g.listened),
        })
        .collect();
    top_voices.sort_by(|a, b| by_listened((a.listened_sec, a.reads, &a.name), (b.listened_sec, b.reads, &b.name)));
    top_voices.truncate(TOP_N);

    let mut source_counts: Vec<SourceCount> =
        sources.into_vec().into_iter().map(|(source, reads)| SourceCount { source, reads }).collect();
    source_counts.sort_by(|a, b| b.reads.cmp(&a.reads).then_with(|| a.source.as_str().cmp(b.source.as_str())));

    let avg_words_per_read = if totals.reads > 0 { round_count(totals.words as f64 / totals.reads as f64) } else { 0 };
    let avg_listen_wpm =
        if heard_listened_sec > 0.0 { round_count(heard_words / (heard_listened_sec / 60.0)) } else { 0 };
    let reading_time_equivalent_sec = round_count(heard_words / SILENT_READING_WPM * 60.0);

    AnalyticsSummary {
        range,
        generated_at: to_ms(&now),
        totals,
        today: today_stats,
        streak: streaks_of(&tz, &valid, today, tomorrow_ms),
        daily,
        heatmap,
        top_apps,
        top_voices,
        sources: source_counts,
        avg_words_per_read,
        avg_listen_wpm,
        reading_time_equivalent_sec,
    }
}

/// `heardFraction(r)` (persistence.md §7.3).
pub fn heard_fraction(r: &ReadRecord) -> f64 {
    let listened = non_neg(r.listened_sec);
    if listened == 0.0 {
        return 0.0;
    }
    if r.completed {
        return 1.0;
    }
    let audio = match non_neg(r.audio_duration_sec) {
        a if a > 0.0 => a,
        _ => r.char_count as f64 / ESTIMATED_CHARS_PER_SECOND,
    };
    // `!(audio > 0)`: `audio` is finite and non-negative here.
    if audio <= 0.0 {
        return 1.0;
    }
    let reached = non_neg(r.max_position_sec.unwrap_or(0.0));
    ((if reached > 0.0 { reached } else { listened }) / audio).min(1.0)
}

/// `appName(r)`: trimmed `sourceApp`, else the per-source fallback (`ReadSource::fallback_app_name`).
pub fn app_name(r: &ReadRecord) -> String {
    match r.source_app.as_deref().map(js_trim) {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => r.source.fallback_app_name().to_string(),
    }
}

/// `streaks()` over all valid records (persistence.md §7.5).
pub fn streaks<Tz: TimeZone>(records: &[ReadRecord], now: &DateTime<Tz>) -> Streak {
    let tz = now.timezone();
    let today = now.date_naive();
    let tomorrow_ms = to_ms(&local_midnight(&tz, add_days(today, 1)));
    let valid: Vec<&ReadRecord> = records.iter().filter(|r| r.created_at.is_finite() && r.created_at < tomorrow_ms).collect();
    streaks_of(&tz, &valid, today, tomorrow_ms)
}

fn streaks_of<Tz: TimeZone>(tz: &Tz, valid: &[&ReadRecord], today: NaiveDate, tomorrow_ms: f64) -> Streak {
    // Any read counts (even a failed or unheard one), plus every day with ≥ 1 s of listening before tomorrow.
    let mut days: BTreeSet<NaiveDate> = valid.iter().map(|r| from_ms(tz, r.created_at).date_naive()).collect();
    for r in valid {
        for (at, sec) in listening_entries(tz, r.created_at, r.listened_sec, r.listen_log.as_ref()) {
            if sec >= 1.0 && to_ms(&at) < tomorrow_ms {
                days.insert(at.date_naive());
            }
        }
    }
    let mut current = 0u32;
    let mut cursor = if days.contains(&today) { Some(today) } else { today.pred_opt() };
    while let Some(day) = cursor.filter(|d| days.contains(d)) {
        current += 1;
        cursor = day.pred_opt();
    }
    let mut longest = 0u32;
    let mut run = 0u32;
    let mut prev: Option<NaiveDate> = None;
    for &day in &days {
        run = if prev.and_then(|p| p.succ_opt()) == Some(day) { run + 1 } else { 1 };
        longest = longest.max(run);
        prev = Some(day);
    }
    Streak { current, longest: longest.max(current) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, Utc};
    use vox_core::listen_log::ListenLog;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 17, 12, 0, 0).unwrap()
    }

    fn demo() -> Vec<ReadRecord> {
        crate::demo::generate_demo_records(now(), 120)
    }

    fn ms(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> f64 {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap().timestamp_millis() as f64
    }

    fn rec(id: &str, at: f64) -> ReadRecord {
        let mut r = ReadRecord::new(id.into(), at, ReadSource::Manual);
        r.word_count = 10;
        r.char_count = 60;
        r
    }

    fn top_apps(s: &AnalyticsSummary) -> Vec<(String, u64, f64)> {
        s.top_apps.iter().map(|a| (a.name.clone(), a.reads, a.listened_sec)).collect()
    }

    fn top_voices(s: &AnalyticsSummary) -> Vec<(String, u64, f64)> {
        s.top_voices.iter().map(|v| (v.name.clone(), v.reads, v.listened_sec)).collect()
    }

    fn sources(s: &AnalyticsSummary) -> Vec<(&'static str, u64)> {
        s.sources.iter().map(|c| (c.source.as_str(), c.reads)).collect()
    }

    fn t(name: &str, reads: u64, sec: f64) -> (String, u64, f64) {
        (name.to_string(), reads, sec)
    }

    #[test]
    fn persistence_vectors_30d() {
        let s = compute_analytics(&demo(), AnalyticsRange::D30, now());
        assert_eq!(s.range, AnalyticsRange::D30);
        assert_eq!(s.generated_at, ms(2026, 10, 17, 12, 0));
        let tt = &s.totals;
        assert_eq!(
            (tt.listened_sec, tt.reads, tt.words, tt.chars, tt.audio_sec, tt.completed_reads, tt.characters_billed),
            (3032.4, 71, 9171, 55107, 3570.0, 60, 52235)
        );
        assert_eq!((s.today.listened_sec, s.today.reads, s.today.words), (115.1, 2, 261));
        assert_eq!(s.streak, Streak { current: 9, longest: 18 });
        assert_eq!((s.avg_words_per_read, s.avg_listen_wpm, s.reading_time_equivalent_sec), (129, 166, 2117));
        assert_eq!(s.daily.len(), 30);
        assert_eq!(s.daily[0].date, "2026-09-18");
        assert_eq!(s.daily[29].date, "2026-10-17");
        assert_eq!(top_apps(&s)[..3], [t("Chrome", 31, 1495.0), t("Word", 5, 296.6), t("Notion", 10, 287.4)]);
        assert_eq!(
            sources(&s),
            vec![
                ("context-chip", 38),
                ("hotkey", 13),
                ("browser-extension", 10),
                ("clipboard", 3),
                ("manual", 3),
                ("replay", 2),
                ("selection-popup", 2)
            ]
        );
        assert_eq!(
            top_voices(&s),
            vec![t("George", 34, 1491.0), t("Sarah", 18, 822.6), t("Brian", 10, 421.0), t("Charlotte", 9, 297.8)]
        );
        assert_eq!(s.top_voices[0].voice_id, "JBFqnCBsd6RMkjVDRZzb");
    }

    #[test]
    fn persistence_vectors_all() {
        let s = compute_analytics(&demo(), AnalyticsRange::All, now());
        let tt = &s.totals;
        assert_eq!(
            (tt.listened_sec, tt.reads, tt.words, tt.chars, tt.audio_sec, tt.completed_reads, tt.characters_billed),
            (4992.3, 120, 15235, 92267, 6036.4, 99, 88530)
        );
        assert_eq!(s.streak, Streak { current: 9, longest: 18 });
        assert_eq!(s.daily.len(), 60);
        assert_eq!(s.daily[0].date, "2026-08-19");
        assert_eq!((s.avg_words_per_read, s.avg_listen_wpm, s.reading_time_equivalent_sec), (127, 166, 3491));
        assert_eq!(top_apps(&s)[..3], [t("Chrome", 53, 2427.4), t("Word", 10, 544.7), t("Notion", 16, 498.3)]);
        assert_eq!(
            top_voices(&s),
            vec![t("George", 51, 2175.5), t("Sarah", 33, 1483.9), t("Brian", 20, 781.6), t("Charlotte", 16, 551.3)]
        );
        assert_eq!(
            sources(&s),
            vec![
                ("context-chip", 64),
                ("hotkey", 24),
                ("browser-extension", 12),
                ("clipboard", 8),
                ("selection-popup", 6),
                ("manual", 3),
                ("replay", 3)
            ]
        );
    }

    #[test]
    fn persistence_vectors_7d() {
        let s = compute_analytics(&demo(), AnalyticsRange::D7, now());
        let tt = &s.totals;
        assert_eq!(
            (tt.listened_sec, tt.reads, tt.words, tt.chars, tt.audio_sec, tt.completed_reads, tt.characters_billed),
            (729.9, 17, 2036, 12357, 823.5, 15, 12357)
        );
        let days: Vec<(&str, f64, u64, u64)> =
            s.daily.iter().map(|d| (d.date.as_str(), d.listened_sec, d.reads, d.words)).collect();
        assert_eq!(
            days,
            vec![
                ("2026-10-11", 39.7, 2, 136),
                ("2026-10-12", 125.1, 3, 386),
                ("2026-10-13", 96.2, 2, 284),
                ("2026-10-14", 173.1, 4, 424),
                ("2026-10-15", 46.6, 2, 207),
                ("2026-10-16", 134.1, 2, 338),
                ("2026-10-17", 115.1, 2, 261)
            ]
        );
        let rows: Vec<f64> = s.heatmap.iter().map(|row| round1(row.iter().sum())).collect();
        assert_eq!(rows, vec![125.1, 96.2, 173.1, 46.6, 134.1, 115.1, 39.7]);
        assert_eq!(s.streak, Streak { current: 9, longest: 18 });
    }

    #[test]
    fn finite_ranges_have_exactly_n_days() {
        for (range, n) in [(AnalyticsRange::D7, 7), (AnalyticsRange::D30, 30), (AnalyticsRange::D90, 90), (AnalyticsRange::D365, 365)] {
            let s = compute_analytics(&[], range, now());
            assert_eq!(s.daily.len(), n);
            assert_eq!(s.daily.last().unwrap().date, "2026-10-17");
        }
        let s = compute_analytics(&[], AnalyticsRange::All, now());
        assert_eq!(s.daily.len(), 1, "all with no records = today only");
        assert_eq!(s.totals, Totals::default());
        assert_eq!(s.streak, Streak::default());
        assert!(s.top_apps.is_empty() && s.top_voices.is_empty() && s.sources.is_empty());
        assert_eq!((s.avg_words_per_read, s.avg_listen_wpm, s.reading_time_equivalent_sec), (0, 0, 0));
    }

    #[test]
    fn all_is_capped_at_max_all_days() {
        let mut records = vec![rec("recent", ms(2026, 10, 16, 9, 0)), rec("epoch", 0.0), rec("ancient", -1e14)];
        records[1].listened_sec = 30.0;
        let s = compute_analytics(&records, AnalyticsRange::All, now());
        assert_eq!(s.daily.len(), MAX_ALL_DAYS);
        let first = add_days(now().date_naive(), -(MAX_ALL_DAYS as i64 - 1));
        assert_eq!(s.daily[0].date, day_key_of_date(first));
        assert_eq!(s.totals.reads, 1, "records before the window are outside the range everywhere");
        assert_eq!(s.totals.listened_sec, 0.0);
        // Streaks have no lower bound (the epoch day counts as a day) but these days are not consecutive.
        assert_eq!(s.streak, Streak { current: 1, longest: 1 });
    }

    #[test]
    fn future_records_are_ignored_everywhere() {
        let mut future = rec("future", ms(2026, 10, 18, 0, 0));
        future.listened_sec = 100.0;
        future.completed = true;
        let s = compute_analytics(&[future.clone()], AnalyticsRange::D7, now());
        assert_eq!(s.totals, Totals::default());
        assert_eq!(s.today, TodayStats::default());
        assert_eq!(s.streak, Streak::default());
        // Later today is fine.
        future.created_at = ms(2026, 10, 17, 23, 59);
        let s = compute_analytics(&[future], AnalyticsRange::D7, now());
        assert_eq!((s.totals.reads, s.today.reads), (1, 1));
    }

    #[test]
    fn listening_is_credited_when_it_happened() {
        // An old read (outside 7d) replayed today from the cache: reads stay on its day, listening counts today.
        let mut old = rec("old", ms(2026, 9, 1, 10, 0));
        old.source_app = Some("  Kindle ".into());
        old.voice_id = "v1".into();
        old.voice_name = "Old name".into();
        old.listened_sec = 50.0;
        let mut log = ListenLog::new();
        log.0.insert("2026-09-01T10".into(), 20.0);
        log.0.insert("2026-10-17T08".into(), 30.0);
        old.listen_log = Some(log);
        let s = compute_analytics(&[old.clone()], AnalyticsRange::D7, now());
        assert_eq!(s.totals.reads, 0);
        assert_eq!(s.totals.listened_sec, 30.0);
        assert_eq!(s.today.listened_sec, 30.0);
        assert_eq!(s.today.reads, 0);
        assert_eq!(s.daily[6].listened_sec, 30.0);
        assert_eq!(s.heatmap[5][8], 30.0, "Saturday 08:00");
        assert_eq!(top_apps(&s), vec![t("Kindle", 0, 30.0)]);
        assert_eq!(top_voices(&s), vec![t("Old name", 0, 30.0)]);
        assert!(s.sources.is_empty());
        // Over all time both entries count.
        let s = compute_analytics(&[old], AnalyticsRange::All, now());
        assert_eq!(s.totals.listened_sec, 50.0);
        assert_eq!(s.totals.reads, 1);
    }

    #[test]
    fn ledger_larger_than_total_is_scaled_and_rest_goes_to_created_at() {
        let mut r = rec("r", ms(2026, 10, 17, 6, 0));
        r.listened_sec = 10.0;
        let mut log = ListenLog::new();
        log.0.insert("2026-10-17T07".into(), 15.0);
        log.0.insert("2026-10-17T09".into(), 5.0);
        r.listen_log = Some(log);
        let s = compute_analytics(&[r.clone()], AnalyticsRange::D7, now());
        assert_eq!((s.heatmap[5][7], s.heatmap[5][9]), (7.5, 2.5));
        assert_eq!(s.totals.listened_sec, 10.0);
        // Less in the ledger than the total: the rest lands at createdAt.
        r.listened_sec = 25.0;
        let s = compute_analytics(&[r], AnalyticsRange::D7, now());
        assert_eq!((s.heatmap[5][6], s.heatmap[5][7], s.heatmap[5][9]), (5.0, 15.0, 5.0));
    }

    #[test]
    fn top_lists_sort_and_filter() {
        let mut a = rec("a", ms(2026, 10, 17, 9, 0));
        a.source_app = Some("banana".into());
        let mut b = rec("b", ms(2026, 10, 17, 9, 0));
        b.source_app = Some("Apple".into());
        let mut c = rec("c", ms(2026, 10, 17, 9, 0));
        c.source_app = Some("cherry".into());
        c.listened_sec = 0.04; // rounds to 0.0 and has no read in a range that excludes it
        c.created_at = ms(2026, 10, 1, 9, 0);
        let mut log = ListenLog::new();
        log.0.insert("2026-10-17T09".into(), 0.04);
        c.listen_log = Some(log);
        let s = compute_analytics(&[a, b, c], AnalyticsRange::D7, now());
        // Equal listening and reads: name order, case-insensitive (Apple < banana).
        assert_eq!(top_apps(&s), vec![t("Apple", 1, 0.0), t("banana", 1, 0.0)], "the rounded-to-zero group is dropped");
        // Voices have no zero filter.
        assert_eq!(s.top_voices.len(), 1);
        assert_eq!(s.top_voices[0].voice_id, "unknown");
        assert_eq!(s.top_voices[0].name, "unknown");
        assert_eq!(s.top_voices[0].reads, 2);
    }

    #[test]
    fn top_lists_keep_six() {
        let records: Vec<ReadRecord> = (0..9)
            .map(|i| {
                let mut r = rec(&format!("r{i}"), ms(2026, 10, 17, 9, i));
                r.source_app = Some(format!("App {i}"));
                r.voice_id = format!("voice-{i}");
                r.listened_sec = i as f64;
                r
            })
            .collect();
        let s = compute_analytics(&records, AnalyticsRange::D7, now());
        assert_eq!(s.top_apps.len(), TOP_N);
        assert_eq!(s.top_apps[0].name, "App 8");
        assert_eq!(s.top_voices.len(), TOP_N);
        assert_eq!(s.top_voices[0].name, "voice-8", "no voiceName: the key");
    }

    #[test]
    fn voice_group_name_is_the_newest_in_range() {
        let mut older = rec("older", ms(2026, 10, 15, 9, 0));
        older.voice_id = "v".into();
        older.voice_name = "Old".into();
        let mut newer = rec("newer", ms(2026, 10, 16, 9, 0));
        newer.voice_id = "v".into();
        newer.voice_name = "New".into();
        let mut unnamed = rec("unnamed", ms(2026, 10, 17, 9, 0));
        unnamed.voice_id = "v".into();
        // Newest first, like the store passes them.
        let s = compute_analytics(&[unnamed, newer, older], AnalyticsRange::D7, now());
        assert_eq!(top_voices(&s), vec![t("New", 3, 0.0)]);
    }

    #[test]
    fn heard_fraction_rules() {
        let mut r = rec("r", 0.0);
        assert_eq!(heard_fraction(&r), 0.0, "nothing listened");
        r.listened_sec = 10.0;
        r.audio_duration_sec = 40.0;
        assert_eq!(heard_fraction(&r), 0.25, "listened-time fallback");
        r.max_position_sec = Some(30.0);
        assert_eq!(heard_fraction(&r), 0.75, "furthest position");
        r.max_position_sec = Some(80.0);
        assert_eq!(heard_fraction(&r), 1.0, "capped");
        r.max_position_sec = None;
        r.audio_duration_sec = 0.0;
        r.char_count = 300; // 20 s estimated
        assert_eq!(heard_fraction(&r), 0.5);
        r.char_count = 0;
        assert_eq!(heard_fraction(&r), 1.0, "unknown audio length");
        r.completed = true;
        r.char_count = 3000;
        assert_eq!(heard_fraction(&r), 1.0);
        r.listened_sec = f64::NAN;
        assert_eq!(heard_fraction(&r), 0.0);
    }

    #[test]
    fn app_name_fallbacks() {
        let mut r = rec("r", 0.0);
        let cases = [
            (ReadSource::Manual, "Vox"),
            (ReadSource::Replay, "Vox"),
            (ReadSource::Clipboard, "Clipboard"),
            (ReadSource::Explorer, "Explorer"),
            (ReadSource::Cli, "Command line"),
            (ReadSource::BrowserExtension, "Browser"),
            (ReadSource::Hotkey, "Other"),
            (ReadSource::ContextChip, "Other"),
            (ReadSource::SelectionPopup, "Other"),
        ];
        for (source, name) in cases {
            r.source = source;
            assert_eq!(app_name(&r), name);
        }
        r.source_app = Some(" \u{FEFF} ".into());
        assert_eq!(app_name(&r), "Other");
        r.source_app = Some(" Word ".into());
        assert_eq!(app_name(&r), "Word");
    }

    #[test]
    fn streak_rules() {
        let at = |d: u32| ms(2026, 10, d, 9, 0);
        // Today empty: anchored on yesterday.
        let records = vec![rec("a", at(16)), rec("b", at(15)), rec("c", at(13)), rec("d", at(12)), rec("e", at(11))];
        assert_eq!(streaks(&records, &now()), Streak { current: 2, longest: 3 });
        // Nothing yesterday or today: current 0.
        assert_eq!(streaks(&records[2..], &now()), Streak { current: 0, longest: 3 });
        // Listening of ≥ 1 s on a day counts that day; less does not.
        let mut replayed = rec("f", at(1));
        replayed.listened_sec = 1.5;
        let mut log = ListenLog::new();
        log.0.insert("2026-10-17T08".into(), 1.0);
        log.0.insert("2026-10-14T08".into(), 0.5);
        replayed.listen_log = Some(log);
        let mut all = records.clone();
        all.push(replayed);
        assert_eq!(streaks(&all, &now()), Streak { current: 3, longest: 3 });
        let s = compute_analytics(&all, AnalyticsRange::D7, now());
        assert_eq!(s.streak, Streak { current: 3, longest: 3 });
    }

    #[test]
    fn huge_stored_counts_saturate_in_every_range() {
        let dir = tempfile::tempdir().unwrap();
        let profile = crate::paths::Profile::at(dir.path(), false);
        let (a, b) = (ms(2026, 10, 17, 9, 0), ms(2026, 10, 16, 9, 0));
        // Electron accepts any finite number here; vox-core loads 1e300 as u64::MAX.
        std::fs::write(
            profile.history_file(),
            format!(
                r#"{{"version":1,"records":[
                {{"id":"a","createdAt":{a},"source":"manual","wordCount":1e300,"charCount":1e300,"charactersBilled":1e300}},
                {{"id":"b","createdAt":{b},"source":"manual","wordCount":5,"charCount":30,"charactersBilled":30}},
                {{"id":"c","createdAt":{a},"source":"manual","wordCount":1e300,"charCount":1e300,"charactersBilled":1e300}}]}}"#
            ),
        )
        .unwrap();
        let records = crate::history_store::HistoryStore::open(&profile).all();
        assert_eq!(records.len(), 3);
        assert_eq!(records.iter().filter(|r| r.word_count == u64::MAX).count(), 2);
        for range in AnalyticsRange::ALL {
            let s = compute_analytics(&records, range, now());
            assert_eq!(s.totals.reads, 3, "{range:?}");
            assert_eq!((s.totals.words, s.totals.chars, s.totals.characters_billed), (u64::MAX, u64::MAX, u64::MAX));
            assert_eq!(s.today.words, u64::MAX);
            assert_eq!(s.daily.last().unwrap().words, u64::MAX);
        }
    }

    #[test]
    fn local_days_follow_the_time_zone() {
        // 2026-10-16T23:30Z is Saturday 2026-10-17 01:30 at +02:00.
        let tz = FixedOffset::east_opt(2 * 3600).unwrap();
        let now = tz.with_ymd_and_hms(2026, 10, 17, 12, 0, 0).unwrap();
        let mut r = rec("r", ms(2026, 10, 16, 23, 30));
        r.listened_sec = 60.0;
        let s = compute_analytics(&[r], AnalyticsRange::D7, now);
        assert_eq!(s.today.reads, 1);
        assert_eq!(s.daily[6].date, "2026-10-17");
        assert_eq!((s.daily[6].reads, s.daily[6].listened_sec), (1, 60.0));
        assert_eq!(s.heatmap[5][1], 60.0);
    }
}
