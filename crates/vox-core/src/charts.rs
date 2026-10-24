//! Chart data models for Home and Analytics (ui-main.md §5, charts/model.ts): bucketing daily points into
//! day/week/month columns, axis ticks and labels, bar levels, the heatmap normalisation and the top lists. Geometry
//! that depended on the old dot-matrix look is not reproduced; pages get 0..1 levels and draw bars in Slint.

use chrono::{Datelike, NaiveDate};

use crate::analytics::DailyEntry;
use crate::format::{format_duration, format_hm_dot, format_number, MONTHS_LONG, MONTHS_SHORT, WEEKDAYS_SHORT};
use crate::js::js_round;
use crate::time::parse_day_key;

/// Column unit after bucketing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BucketUnit {
    Day,
    Week,
    Month,
}

/// One chart column.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnBucket {
    /// Day key, first day key of the week, or `YYYY-MM`.
    pub key: String,
    pub start: NaiveDate,
    /// Inclusive.
    pub end: NaiveDate,
    pub days: u32,
    pub listened_sec: f64,
    pub reads: u64,
    pub words: u64,
    pub is_today: bool,
}

fn day_of(key: &str) -> NaiveDate {
    parse_day_key(key).unwrap_or_else(|| NaiveDate::from_ymd_opt(1970, 1, 1).expect("valid"))
}

/// Groups daily points into at most `max_columns` columns: days if they fit; else 7-day weeks counted back from the
/// newest day (the newest column always ends on the newest day; the oldest may be partial); else calendar months.
pub fn bucket_daily(daily: &[DailyEntry], max_columns: usize, today_key: &str) -> (BucketUnit, Vec<ColumnBucket>) {
    let max = max_columns.max(1);
    let new_bucket = |key: &str, start: NaiveDate| ColumnBucket {
        key: key.to_string(),
        start,
        end: start,
        days: 0,
        listened_sec: 0.0,
        reads: 0,
        words: 0,
        is_today: false,
    };
    if daily.len() <= max {
        let b = daily
            .iter()
            .map(|p| {
                let mut b = new_bucket(&p.date, day_of(&p.date));
                b.days = 1;
                b.listened_sec = p.listened_sec;
                b.reads = p.reads;
                b.words = p.words;
                b.is_today = p.date == today_key;
                b
            })
            .collect();
        return (BucketUnit::Day, b);
    }
    if daily.len().div_ceil(7) <= max {
        let mut out = Vec::new();
        let mut end = daily.len();
        while end > 0 {
            let slice = &daily[end.saturating_sub(7)..end];
            let mut b = new_bucket(&slice[0].date, day_of(&slice[0].date));
            b.end = day_of(&slice[slice.len() - 1].date);
            b.days = slice.len() as u32;
            for p in slice {
                b.listened_sec += p.listened_sec;
                b.reads += p.reads;
                b.words += p.words;
                b.is_today |= p.date == today_key;
            }
            out.insert(0, b);
            end = end.saturating_sub(7);
        }
        return (BucketUnit::Week, out);
    }
    let mut out: Vec<ColumnBucket> = Vec::new();
    for p in daily {
        let key = p.date.get(..7).unwrap_or("").to_string();
        let d = day_of(&p.date);
        if out.last().map(|b| b.key != key).unwrap_or(true) {
            out.push(new_bucket(&key, d));
        }
        let b = out.last_mut().expect("just pushed");
        b.end = d;
        b.days += 1;
        b.listened_sec += p.listened_sec;
        b.reads += p.reads;
        b.words += p.words;
        b.is_today |= p.date == today_key;
    }
    (BucketUnit::Month, out)
}

/// Indices to label on an axis of `n` columns, anchored at the newest column.
pub fn pick_ticks(n: usize, max_labels: usize) -> Vec<usize> {
    if n == 0 {
        return Vec::new();
    }
    let cap = max_labels.max(1);
    let step = n.div_ceil(cap).max(1);
    let mut out: Vec<usize> = (0..n).rev().step_by(step).collect();
    out.reverse();
    out
}

/// Axis label: month `JAN` (January shows the 2-digit year: `JAN 26`); day with ≤ 7 columns: `SUN`…; else `SEP 14`.
/// Upper-case like the original; pages may restyle.
pub fn axis_label(b: &ColumnBucket, unit: BucketUnit, total: usize) -> String {
    let m = MONTHS_SHORT[b.start.month0() as usize].to_uppercase();
    match unit {
        BucketUnit::Month if b.start.month() == 1 => format!("{m} {:02}", b.start.year() % 100),
        BucketUnit::Month => m,
        BucketUnit::Day if total <= 7 => WEEKDAYS_SHORT[b.start.weekday().num_days_from_sunday() as usize].to_uppercase(),
        _ => format!("{m} {}", b.start.day()),
    }
}

fn short_day(d: NaiveDate) -> String {
    format!("{} {}", MONTHS_SHORT[d.month0() as usize], d.day())
}

/// Tooltip/table title: `Tue, Sep 30` (day), `Sep 8 – Sep 14` (week; single date for 1 day), `September 2026`.
pub fn bucket_title(b: &ColumnBucket, unit: BucketUnit) -> String {
    match unit {
        BucketUnit::Day => format!("{}, {}", WEEKDAYS_SHORT[b.start.weekday().num_days_from_sunday() as usize], short_day(b.start)),
        BucketUnit::Month => format!("{} {}", MONTHS_LONG[b.start.month0() as usize], b.start.year()),
        BucketUnit::Week if b.days <= 1 => short_day(b.start),
        BucketUnit::Week => format!("{} \u{2013} {}", short_day(b.start), short_day(b.end)),
    }
}

/// Bucket tooltip: `(title, value, sub)` = (`bucket_title`, `42H18M`-style hours, `3 reads · 1,204 words`).
pub fn bucket_tooltip(b: &ColumnBucket, unit: BucketUnit) -> (String, String, String) {
    let reads = format!("{} read{}", format_number(b.reads as f64), if b.reads == 1 { "" } else { "s" });
    (bucket_title(b, unit), format_hm_dot(b.listened_sec), format!("{reads} \u{00B7} {} words", format_number(b.words as f64)))
}

/// `dotLevels`: integer levels 0..=steps; any positive value lights at least 1; the max lights all.
pub fn levels(values: &[f64], steps: u32) -> Vec<u32> {
    let top = values.iter().copied().fold(0.0, f64::max);
    if top.is_nan() || top <= 0.0 || steps == 0 {
        return vec![0; values.len()];
    }
    values
        .iter()
        .map(|v| if *v > 0.0 { (js_round(v.min(top) / top * steps as f64) as u32).clamp(1, steps) } else { 0 })
        .collect()
}

/// Continuous 0..1 bar heights relative to the max (0 for non-positive values).
pub fn bar_fractions(values: &[f64]) -> Vec<f64> {
    let top = values.iter().copied().fold(0.0, f64::max);
    values.iter().map(|v| if top > 0.0 && *v > 0.0 { (v / top).min(1.0) } else { 0.0 }).collect()
}

/// `barDots(value, max, count)`.
pub fn bar_dots(value: f64, max: f64, count: u32) -> u32 {
    let share = value / max;
    if max.is_nan() || max <= 0.0 || share.is_nan() || share <= 0.0 {
        return 0;
    }
    (js_round(share * count as f64) as u32).clamp(1, count)
}

/// Days with any activity / all days.
pub fn active_days(daily: &[DailyEntry]) -> (usize, usize) {
    (daily.iter().filter(|d| d.reads > 0 || d.listened_sec > 0.0).count(), daily.len())
}

/// Last `n` days' activity flags, oldest → newest.
pub fn recent_activity(daily: &[DailyEntry], n: usize) -> Vec<bool> {
    daily[daily.len().saturating_sub(n)..].iter().map(|d| d.reads > 0 || d.listened_sec > 0.0).collect()
}

/// One heatmap cell (`day` 0 = Monday).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HeatCell {
    pub day: usize,
    pub hour: usize,
    pub value: f64,
    /// Area-true intensity `sqrt(value / max)`, 0 for empty.
    pub level: f64,
}

/// Normalised heatmap: `(cells Mon→Sun × 0→23, max, peak (first strictly largest positive), total)`.
pub fn heat_cells(heatmap: &[[f64; 24]; 7]) -> (Vec<HeatCell>, f64, Option<HeatCell>, f64) {
    let mut cells = Vec::with_capacity(168);
    let mut max: f64 = 0.0;
    let mut total = 0.0;
    for (day, row) in heatmap.iter().enumerate() {
        for (hour, v) in row.iter().enumerate() {
            let v = if v.is_finite() { v.max(0.0) } else { 0.0 };
            max = max.max(v);
            total += v;
            cells.push(HeatCell { day, hour, value: v, level: 0.0 });
        }
    }
    let mut peak: Option<HeatCell> = None;
    for c in &mut cells {
        c.level = if max > 0.0 { (c.value / max).sqrt() } else { 0.0 };
        if c.value > 0.0 && peak.map(|p| c.value > p.value).unwrap_or(true) {
            peak = Some(*c);
        }
    }
    (cells, max, peak, total)
}

/// A ranked row for Top apps / Top voices.
#[derive(Debug, Clone, PartialEq)]
pub struct TopItem {
    pub key: String,
    pub name: String,
    pub value: f64,
    pub display: String,
    pub sub: String,
}

/// `topItems`: by listening when any row has listening (rows with 0 listening drop out), else by reads; first 6.
pub fn top_items(rows: &[(String, String, u64, f64)]) -> Vec<TopItem> {
    let by_listening = rows.iter().any(|r| r.3 > 0.0);
    rows.iter()
        .map(|(key, name, reads, listened)| {
            let name = if name.is_empty() { "Unknown".to_string() } else { name.clone() };
            let sub = format!("{} read{}", format_number(*reads as f64), if *reads == 1 { "" } else { "s" });
            if by_listening {
                TopItem { key: key.clone(), name, value: *listened, display: format_duration(*listened), sub }
            } else {
                TopItem { key: key.clone(), name, value: *reads as f64, display: format!("{reads}"), sub }
            }
        })
        .filter(|t| t.value > 0.0)
        .take(6)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::{add_days, day_key_of_date};

    fn days(n: usize, today: NaiveDate) -> Vec<DailyEntry> {
        (0..n)
            .map(|i| DailyEntry {
                date: day_key_of_date(add_days(today, i as i64 - (n as i64 - 1))),
                listened_sec: i as f64,
                reads: 1,
                words: 2,
            })
            .collect()
    }

    #[test]
    fn bucketing() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 17).unwrap();
        let tk = day_key_of_date(today);
        let (u, b) = bucket_daily(&days(365, today), 106, &tk);
        assert_eq!(u, BucketUnit::Week);
        assert_eq!(b.len(), 53);
        assert_eq!(b[0].days, 1);
        assert_eq!(b.last().unwrap().end, today);
        assert!(b.last().unwrap().is_today);
        let (u, b) = bucket_daily(&days(90, today), 106, &tk);
        assert_eq!((u, b.len()), (BucketUnit::Day, 90));
        let (u, b) = bucket_daily(&days(365, today), 50, &tk);
        assert_eq!(u, BucketUnit::Month);
        assert!(b.len() == 12 || b.len() == 13);
        assert_eq!(axis_label(&b[3], u, b.len()), "JAN 26");
        assert_eq!(bucket_title(&b[3], u), "January 2026");
    }

    #[test]
    fn ticks_levels_dots() {
        assert_eq!(pick_ticks(30, 10), vec![2, 5, 8, 11, 14, 17, 20, 23, 26, 29]);
        assert_eq!(levels(&[0.0, 100.0, 50.0, 1.0], 15), vec![0, 15, 8, 1]);
        assert_eq!(levels(&[0.0, 3.0, 6.0], 6), vec![0, 3, 6]);
        assert_eq!(bar_dots(1.0, 100.0, 20), 1);
        assert_eq!(bar_dots(50.0, 100.0, 20), 10);
        assert_eq!(bar_fractions(&[0.0, 5.0, 10.0]), vec![0.0, 0.5, 1.0]);
    }

    #[test]
    fn heat_and_top() {
        let mut h = [[0.0; 24]; 7];
        h[5][21] = 400.0;
        h[2][8] = 100.0;
        h[6][1] = 400.0;
        let (cells, max, peak, total) = heat_cells(&h);
        assert_eq!(cells.len(), 168);
        assert_eq!(max, 400.0);
        assert_eq!(total, 900.0);
        let p = peak.unwrap();
        assert_eq!((p.day, p.hour), (5, 21)); // first strictly largest
        assert_eq!(cells[2 * 24 + 8].level, 0.5);
        let rows = vec![("a".into(), "Chrome".into(), 31, 1495.0), ("b".into(), "Word".into(), 5, 0.0)];
        let t = top_items(&rows);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].display, "24m"); // 1495 s = 24.9 min, minutes floored
        let reads_only = vec![("a".into(), "".into(), 3, 0.0)];
        assert_eq!(top_items(&reads_only)[0].name, "Unknown");
        assert_eq!(top_items(&reads_only)[0].display, "3");
    }
}
