//! Pure display formatting (ui-main.md §11, ui-settings-widget.md §2/§6, reader-playback.md §8, shell.md §7.2).
//! Pages receive ready-made strings built with these helpers; `.slint` files never format.
//!
//! Locale (BRIEF.md Product rulings; ARCHITECTURE.md §8 D-FMT-1): where the old app used `Intl` defaults, numbers and
//! dates follow the Windows user locale's *conventions* through [`crate::locale::current`] (decimal and grouping
//! separators, 12/24-hour clock, day-first vs month-first dates); names and words stay English because the copy is
//! English. Strings the old app pinned to en-US use [`format_number_en`]. Tests run with
//! [`crate::locale::Locale::EN_US`] unless they wrap a call in [`crate::locale::with_locale`]. Rounding follows JS
//! `Math.round` ([`js_round`]). Model names use one style everywhere: [`model_name`] (`Flash v2.5`, `Eleven v3`).

use chrono::{DateTime, Datelike, TimeZone, Timelike};

use crate::constants::{DAY_MS, LOW_CREDITS_SHARE, TOOLTIP_MAX};
use crate::js::{capitalize_first, collapse_whitespace, fixed_trimmed, js_round, js_trim_end, to_fixed};
use crate::locale::{current as locale, DateOrder, Locale};
use crate::player::{PlayerSnapshot, PlayerState, PlayerStatus};
use crate::read::ReadErrorCode;
use crate::text16::{prefix16, utf16_len};
use crate::time::{calendar_days_between, from_ms, parse_day_key};
use crate::voices::TtsModel;

pub const WEEKDAYS_LONG: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
pub const WEEKDAYS_SHORT: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
pub const WEEKDAY_INITIALS: [&str; 7] = ["S", "M", "T", "W", "T", "F", "S"];
/// Monday-first full names (heatmap rows).
pub const WEEKDAYS_MON_FIRST: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
pub const MONTHS_LONG: [&str; 12] =
    ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
pub const MONTHS_SHORT: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

fn pad2(n: u64) -> String {
    format!("{n:02}")
}

fn sunday_index<Tz: TimeZone>(dt: &DateTime<Tz>) -> usize {
    dt.weekday().num_days_from_sunday() as usize
}

// ───────────────────────────── durations ─────────────────────────────

/// `263 → "4:23"`, `3723 → "1:02:03"` (floored).
pub fn format_clock(sec: f64) -> String {
    let s = if sec.is_finite() { sec.floor().max(0.0) as u64 } else { 0 };
    let (h, m, ss) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}:{}:{}", pad2(m), pad2(ss))
    } else {
        format!("{m}:{}", pad2(ss))
    }
}

/// `'-3:20'`, `'~-3:20'` while estimated (Home/sidebar; rounded).
pub fn remaining_clock(position_sec: f64, duration_sec: f64, estimate: bool) -> String {
    let left = js_round(duration_sec - position_sec).max(0.0) as u64;
    let (m, s) = (left / 60, left % 60);
    let h = m / 60;
    let clock = if h > 0 { format!("{h}:{}:{}", pad2(m % 60), pad2(s)) } else { format!("{m}:{}", pad2(s)) };
    format!("{}-{clock}", if estimate { "~" } else { "" })
}

/// The widget's time label (reader-playback.md §8.3): ended → total length at the current rate; otherwise the
/// remaining wall time prefixed `≈` (estimate) or `−` (U+2212). Returns `(label, is_estimate)`.
pub fn widget_time_label(state: &PlayerState) -> (String, bool) {
    let rate = if state.rate > 0.0 { state.rate } else { 1.0 };
    if state.status == PlayerStatus::Ended {
        return (format_clock(state.duration_sec / rate), false);
    }
    let left = format_clock((state.duration_sec - state.position_sec).max(0.0) / rate);
    if state.duration_is_estimate {
        (format!("\u{2248}{left}"), true)
    } else {
        (format!("\u{2212}{left}"), false)
    }
}

/// The widget progress bar's accessible value (reader-playback.md §8.3; ui-settings-widget.md §6.3):
/// `{clock(pos)} of {about }{clock(dur)}` in media seconds (no rate division), `about ` while the duration is an
/// estimate — `"1:03 of about 4:23"`.
pub fn progress_accessible(state: &PlayerState) -> String {
    format!(
        "{} of {}{}",
        format_clock(state.position_sec),
        if state.duration_is_estimate { "about " } else { "" },
        format_clock(state.duration_sec)
    )
}

/// `15600 → "4H20M"`, `3900 → "1H05M"`, `42 → "42S"` (floored; the old dot-matrix style).
pub fn dot_duration(sec: f64) -> String {
    let s = if sec.is_finite() { sec.floor().max(0.0) as u64 } else { 0 };
    if s < 60 {
        return format!("{s}S");
    }
    let (h, m) = (s / 3600, (s % 3600) / 60);
    if h > 0 {
        format!("{h}H{}M", pad2(m))
    } else {
        format!("{m}M")
    }
}

/// `152280 → "42H18M"`, `1080 → "18M"`, `45 → "45S"`, `0 → "0M"`, `360000 → "100H"` (rounded).
pub fn format_hm_dot(sec: f64) -> String {
    let s = js_round(sec.max(0.0)) as u64;
    if s > 0 && s < 60 {
        return format!("{s}S");
    }
    let mins = s / 60;
    let (h, m) = (mins / 60, mins % 60);
    if h == 0 {
        format!("{m}M")
    } else if h >= 100 {
        format!("{h}H")
    } else {
        format!("{h}H{}M", pad2(m))
    }
}

fn speech(s: u64) -> String {
    if s < 60 {
        return format!("{s} second{}", if s == 1 { "" } else { "s" });
    }
    let (h, m) = (s / 3600, (s % 3600) / 60);
    let mut parts = Vec::new();
    if h > 0 {
        parts.push(format!("{h} hour{}", if h == 1 { "" } else { "s" }));
    }
    if m > 0 || h == 0 {
        parts.push(format!("{m} minute{}", if m == 1 { "" } else { "s" }));
    }
    parts.join(" ")
}

/// `'4 hours 20 minutes'` (floored).
pub fn duration_speech(sec: f64) -> String {
    speech(if sec.is_finite() { sec.floor().max(0.0) as u64 } else { 0 })
}

/// Like [`duration_speech`] but rounded.
pub fn speak_duration(sec: f64) -> String {
    speech(js_round(sec.max(0.0)) as u64)
}

/// `15600 → "4h 20m"`, `300 → "5m"`, `20 → "20s"`, `3600 → "1h"` (rounded). Also `shortDuration`.
pub fn format_duration(sec: f64) -> String {
    let s = if sec.is_finite() { js_round(sec.max(0.0)) as u64 } else { 0 };
    if s < 60 {
        return format!("{s}s");
    }
    let mins = s / 60;
    let (h, m) = (mins / 60, mins % 60);
    if h == 0 {
        format!("{m}m")
    } else if m == 0 {
        format!("{h}h")
    } else {
        format!("{h}h {m}m")
    }
}

/// `"21:00–22:00"` (en dash; 24 wraps to 00).
pub fn hour_range(hour: u32) -> String {
    format!("{:02}:00\u{2013}{:02}:00", hour % 24, (hour + 1) % 24)
}

// ───────────────────────────── numbers ─────────────────────────────

/// `round(n)` with the user locale's grouping: `38766 → "38,766"` (en-US) / `"38.766"` (de-DE).
pub fn format_number(n: f64) -> String {
    format_number_in(&locale(), n)
}

/// [`format_number`] pinned to en-US (`toLocaleString('en-US')` in the old app: the truncation toast).
pub fn format_number_en(n: f64) -> String {
    format_number_in(&Locale::EN_US, n)
}

fn format_number_in(l: &Locale, n: f64) -> String {
    let r = if n.is_finite() { js_round(n) } else { 0.0 };
    let neg = r < 0.0;
    let out = l.group_digits(&format!("{}", r.abs() as u128));
    if neg {
        format!("-{out}")
    } else {
        out
    }
}

/// Compact notation with at most one fraction digit (English suffixes, locale decimal separator): `12400 → "12.4K"`,
/// `100000 → "100K"`, `999 → "999"`, `1500000 → "1.5M"`; de-DE `"12,4K"`.
pub fn format_compact(n: f64) -> String {
    if !n.is_finite() {
        return "0".into();
    }
    const UNITS: [(f64, &str); 5] = [(1.0, ""), (1e3, "K"), (1e6, "M"), (1e9, "B"), (1e12, "T")];
    let abs = n.abs();
    let mut idx = UNITS.iter().rposition(|(d, _)| abs >= *d).unwrap_or(0);
    loop {
        let (div, suffix) = UNITS[idx];
        let v = (n / div * 10.0).abs().round() / 10.0 * n.signum();
        if v.abs() >= 1000.0 && idx + 1 < UNITS.len() {
            idx += 1;
            continue;
        }
        let num = if v == v.trunc() { format!("{}", v as i64) } else { locale().localize_decimal(&to_fixed(v, 1)) };
        return format!("{num}{suffix}");
    }
}

/// `"{n} {singular|plural}"` with locale grouping (`"1 read"`, `"1,204 reads"`).
pub fn count_label(n: u64, singular: &str, plural: &str) -> String {
    format!("{} {}", format_number(n as f64), if n == 1 { singular } else { plural })
}

/// Playback rate label: `1 → "1×"`, `1.25 → "1.25×"` (`Number(r.toFixed(2))`).
pub fn rate_label(rate: f64) -> String {
    format!("{}\u{00D7}", fixed_trimmed(if rate.is_finite() && rate != 0.0 { rate } else { 1.0 }, 2))
}

/// Percentage text `"39%"` of a 0..1 fraction.
pub fn percent(fraction: f64) -> String {
    format!("{}%", js_round(fraction.clamp(0.0, 1.0) * 100.0) as i64)
}

// ───────────────────────────── text ─────────────────────────────

/// Whitespace collapsed; longer than `max` units → first `max − 1` units, trimmed, plus `…`.
pub fn truncate(text: &str, max: usize) -> String {
    let t = collapse_whitespace(text);
    if utf16_len(&t) <= max {
        return t;
    }
    format!("{}\u{2026}", js_trim_end(prefix16(&t, max.saturating_sub(1))))
}

/// `https://www.example.com/a/b?q=1 → "example.com/a/b"`; unparsable → the raw string.
pub fn display_url(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(u) => {
            let host = u.host_str().unwrap_or("");
            let host = host.strip_prefix("www.").unwrap_or(host);
            let path = u.path();
            if path == "/" {
                host.to_string()
            } else {
                format!("{host}{path}")
            }
        }
        Err(_) => url.to_string(),
    }
}

/// Strips a leading `SomethingError: ` from an error message; empty → `Something went wrong`.
pub fn clean_error(raw: &str) -> String {
    let t = raw.trim();
    let stripped = match t.find(": ") {
        Some(i) if t[..i].ends_with("Error") && t[..i].bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') => t[i + 2..].trim_start(),
        _ => t,
    };
    let s = stripped.trim();
    if s.is_empty() {
        "Something went wrong".into()
    } else {
        s.to_string()
    }
}

// ───────────────────────────── dates ─────────────────────────────

/// `"9:05 PM"` (12-hour locales) / `"21:05"` (24-hour locales).
pub fn time_of_day<Tz: TimeZone>(dt: &DateTime<Tz>) -> String {
    let h = dt.hour();
    if !locale().hour12 {
        return format!("{h:02}:{:02}", dt.minute());
    }
    let (h12, ampm) = match h {
        0 => (12, "AM"),
        1..=11 => (h, "AM"),
        12 => (12, "PM"),
        _ => (h - 12, "PM"),
    };
    format!("{h12}:{:02} {ampm}", dt.minute())
}

/// `"Friday, October 17"` (month-first) / `"Friday 17 October"` (day-first).
pub fn date_line<Tz: TimeZone>(now: &DateTime<Tz>) -> String {
    let (wd, mo, d) = (WEEKDAYS_LONG[sunday_index(now)], MONTHS_LONG[now.month0() as usize], now.day());
    match locale().date_order {
        DateOrder::MonthFirst => format!("{wd}, {mo} {d}"),
        DateOrder::DayFirst => format!("{wd} {d} {mo}"),
    }
}

/// `"Friday, October 17, 2026 at 9:05 PM"` / `"Friday 17 October 2026 at 21:05"`.
pub fn full_date_time<Tz: TimeZone>(tz: &Tz, ms: f64) -> String {
    let dt = from_ms(tz, ms);
    let (wd, mo, d, y) = (WEEKDAYS_LONG[sunday_index(&dt)], MONTHS_LONG[dt.month0() as usize], dt.day(), dt.year());
    match locale().date_order {
        DateOrder::MonthFirst => format!("{wd}, {mo} {d}, {y} at {}", time_of_day(&dt)),
        DateOrder::DayFirst => format!("{wd} {d} {mo} {y} at {}", time_of_day(&dt)),
    }
}

/// `"Oct 8, 2026"` / `"8 Oct 2026"`.
pub fn short_date_with_year<Tz: TimeZone>(dt: &DateTime<Tz>) -> String {
    let (mo, d, y) = (MONTHS_SHORT[dt.month0() as usize], dt.day(), dt.year());
    match locale().date_order {
        DateOrder::MonthFirst => format!("{mo} {d}, {y}"),
        DateOrder::DayFirst => format!("{d} {mo} {y}"),
    }
}

/// `"Tue, Sep 30"` / `"Tue 30 Sep"`.
pub fn weekday_month_day<Tz: TimeZone>(dt: &DateTime<Tz>) -> String {
    let (wd, mo, d) = (WEEKDAYS_SHORT[sunday_index(dt)], MONTHS_SHORT[dt.month0() as usize], dt.day());
    match locale().date_order {
        DateOrder::MonthFirst => format!("{wd}, {mo} {d}"),
        DateOrder::DayFirst => format!("{wd} {d} {mo}"),
    }
}

/// Greeting by local hour: 05–11 morning, 12–17 afternoon, else evening.
pub fn greeting<Tz: TimeZone>(now: &DateTime<Tz>) -> &'static str {
    match now.hour() {
        5..=11 => "Good morning",
        12..=17 => "Good afternoon",
        _ => "Good evening",
    }
}

fn relative(n: i64, unit: &str) -> String {
    let abs = n.unsigned_abs();
    let plural = if abs == 1 { "" } else { "s" };
    if n < 0 {
        format!("{abs} {unit}{plural} ago")
    } else {
        format!("in {abs} {unit}{plural}")
    }
}

/// `Intl.RelativeTimeFormat('en', {numeric: 'auto'})` style: `just now` (< 45 s), minutes, hours, then calendar days
/// (`yesterday`, `3 days ago`) within a week, then `Oct 8, 2026`.
pub fn format_relative<Tz: TimeZone>(tz: &Tz, ms: f64, now_ms: f64) -> String {
    let diff = (ms - now_ms) / 1000.0;
    let abs = diff.abs();
    if abs < 45.0 {
        return "just now".into();
    }
    if abs < 3600.0 {
        let n = js_round(diff / 60.0) as i64;
        return if n == 0 { "this minute".into() } else { relative(n, "minute") };
    }
    if abs < 86_400.0 {
        let n = js_round(diff / 3600.0) as i64;
        return if n == 0 { "this hour".into() } else { relative(n, "hour") };
    }
    let a = from_ms(tz, ms);
    let days = calendar_days_between(from_ms(tz, now_ms).date_naive(), a.date_naive());
    if days.abs() < 7 {
        return match days {
            0 => "today".into(),
            -1 => "yesterday".into(),
            1 => "tomorrow".into(),
            n => relative(n, "day"),
        };
    }
    short_date_with_year(&a)
}

/// History day-group label: `Today` (also future), `Yesterday`, weekday within the last week, `Mon, Sep 14` this
/// year, else `Mon, Sep 14, 2025` (day-first locales: `Mon 14 Sep`, `Mon 14 Sep 2025`).
pub fn day_label<Tz: TimeZone>(tz: &Tz, ms: f64, now_ms: f64) -> String {
    let d = from_ms(tz, ms);
    let n = from_ms(tz, now_ms);
    let diff = calendar_days_between(d.date_naive(), n.date_naive());
    if diff <= 0 {
        "Today".into()
    } else if diff == 1 {
        "Yesterday".into()
    } else if diff < 7 {
        WEEKDAYS_LONG[sunday_index(&d)].into()
    } else if d.year() == n.year() {
        weekday_month_day(&d)
    } else {
        match locale().date_order {
            DateOrder::MonthFirst => format!("{}, {}", weekday_month_day(&d), d.year()),
            DateOrder::DayFirst => format!("{} {}", weekday_month_day(&d), d.year()),
        }
    }
}

/// History row time: relative within 6 days, else the time of day.
pub fn row_time<Tz: TimeZone>(tz: &Tz, ms: f64, now_ms: f64) -> String {
    if now_ms - ms < 6.0 * DAY_MS {
        format_relative(tz, ms, now_ms)
    } else {
        time_of_day(&from_ms(tz, ms))
    }
}

/// `S M T W T F S` initial of a day key; `·` when unparsable.
pub fn weekday_initial(day_key: &str) -> &'static str {
    parse_day_key(day_key).map(|d| WEEKDAY_INITIALS[d.weekday().num_days_from_sunday() as usize]).unwrap_or("\u{00B7}")
}

/// `Monday` for a day key; the key itself when unparsable.
pub fn weekday_name(day_key: &str) -> String {
    parse_day_key(day_key).map(|d| WEEKDAYS_LONG[d.weekday().num_days_from_sunday() as usize].to_string()).unwrap_or_else(|| day_key.to_string())
}

// ───────────────────────────── credits / account ─────────────────────────────

/// `limit ≤ 0` → 0; else `clamp((limit − count) / limit, 0, 1)`.
pub fn credits_left_fraction(count: f64, limit: f64) -> f64 {
    if limit.is_nan() || limit <= 0.0 {
        return 0.0;
    }
    ((limit - count) / limit).clamp(0.0, 1.0)
}

/// True below 15 %.
pub fn credits_low(fraction_left: f64) -> bool {
    fraction_left < LOW_CREDITS_SHARE
}

/// `Resets in 12 days` / `Resets tomorrow` / `Resets today` (any reset within 24 h is "tomorrow", parity quirk).
pub fn reset_label(next_reset_unix: Option<f64>, now_ms: f64) -> Option<String> {
    let u = next_reset_unix.filter(|u| *u != 0.0 && u.is_finite())?;
    let days = ((u * 1000.0 - now_ms) / DAY_MS).ceil() as i64;
    Some(match days {
        d if d <= 0 => "Resets today".into(),
        1 => "Resets tomorrow".into(),
        d => format!("Resets in {d} days"),
    })
}

/// `38.8K of 100K left` (locale decimal separator).
pub fn credits_facts(remaining: f64, limit: f64) -> String {
    format!("{} of {} left", format_compact(remaining), format_compact(limit))
}

/// `38,766 of 100,000 characters left. Resets in 12 days` (locale grouping).
pub fn credits_detail(remaining: f64, limit: f64, reset: Option<&str>) -> String {
    let base = format!("{} of {} characters left", format_number(remaining), format_number(limit));
    match reset {
        Some(r) => format!("{base}. {r}"),
        None => base,
    }
}

/// Analytics tier badge: `[_-]+` → space, capitalise each word; `""` → `Plan`.
pub fn tier_label(tier: &str) -> String {
    let words: Vec<String> = tier.split(['_', '-']).filter(|w| !w.is_empty()).map(|w| w.split(' ').map(capitalize_first).collect::<Vec<_>>().join(" ")).collect();
    let s = words.join(" ");
    if s.trim().is_empty() {
        "Plan".into()
    } else {
        s
    }
}

/// Home tier badge: split on `[_\s-]+`, capitalise each word; empty stays empty (no badge).
pub fn title_case_words(tier: &str) -> String {
    tier.split(|c: char| c == '_' || c == '-' || c.is_whitespace()).filter(|w| !w.is_empty()).map(capitalize_first).collect::<Vec<_>>().join(" ")
}

// ───────────────────────────── models ─────────────────────────────

/// Home's model label for ids the list lacks: `eleven_flash_v2_5 → "Flash v2.5"`, `eleven_v3 → "Eleven v3"`.
pub fn pretty_model_id(id: &str) -> String {
    let Some(rest) = id.strip_prefix("eleven_") else { return id.to_string() };
    // rest = [name_]v<major>[_<minor>]
    let parts: Vec<&str> = rest.split('_').collect();
    let vpos = parts.iter().rposition(|p| p.len() > 1 && p.starts_with('v') && p[1..].bytes().all(|b| b.is_ascii_digit()));
    let Some(vpos) = vpos else { return id.to_string() };
    let minor = parts.get(vpos + 1);
    if vpos + 1 < parts.len() && !(vpos + 2 == parts.len() && minor.is_some_and(|m| !m.is_empty() && m.bytes().all(|b| b.is_ascii_digit()))) {
        return id.to_string();
    }
    let version = match minor {
        Some(m) => format!("{}.{m}", parts[vpos]),
        None => parts[vpos].to_string(),
    };
    let name: Vec<String> = parts[..vpos].iter().filter(|w| !w.is_empty()).map(|w| capitalize_first(w)).collect();
    if name.is_empty() {
        format!("Eleven {version}")
    } else {
        format!("{} {version}", name.join(" "))
    }
}

/// Home's short model name (= [`model_name`]).
pub fn model_label(id: &str, models: &[TtsModel]) -> String {
    model_name(id, models)
}

/// Title-cases an arbitrary model id: `eleven_turbo_v2_5 → "Eleven Turbo v2.5"`, `eleven_v3 → "Eleven v3"`,
/// `my_model → "My Model"` (the last fallback of [`model_name`]).
pub fn pretty_model_id_settings(id: &str) -> String {
    let parts: Vec<&str> = id.split(|c: char| c == '_' || c.is_whitespace()).filter(|p| !p.is_empty()).collect();
    let is_v = |p: &str| p.len() > 1 && (p.starts_with('v') || p.starts_with('V')) && p[1..].bytes().all(|b| b.is_ascii_digit());
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < parts.len() {
        let p = parts[i];
        if is_v(p) {
            match parts.get(i + 1) {
                Some(minor) if minor.bytes().all(|b| b.is_ascii_digit()) => {
                    out.push(format!("{}.{minor}", p.to_lowercase()));
                    i += 1;
                }
                _ => out.push(p.to_lowercase()),
            }
        } else {
            out.push(capitalize_first(p));
        }
        i += 1;
    }
    if out.is_empty() {
        id.to_string()
    } else {
        out.join(" ")
    }
}

/// Drops a leading `Eleven ` unless a version follows it (`Eleven Flash v2.5 → Flash v2.5`, `Eleven v3` kept).
fn strip_eleven(name: &str) -> String {
    if name.len() > 7 && name.is_char_boundary(7) && name[..7].eq_ignore_ascii_case("eleven ") {
        let rest = name[7..].trim_start();
        let is_version = rest.starts_with(['v', 'V']) && rest[1..].starts_with(|c: char| c.is_ascii_digit());
        if !is_version && !rest.is_empty() {
            return rest.to_string();
        }
    }
    name.to_string()
}

/// **The** model display name (BRIEF.md Product rulings: one style everywhere, e.g. `Flash v2.5`): the catalogue
/// name without a leading `Eleven ` (kept before a version), else [`pretty_model_id`], else the title-cased id
/// without `Eleven `. Used by Home, History details, Settings → Voice & model options and Onboarding.
pub fn model_name(id: &str, models: &[TtsModel]) -> String {
    if let Some(m) = models.iter().find(|m| m.model_id == id && !m.name.is_empty()) {
        return strip_eleven(&m.name);
    }
    let pretty = pretty_model_id(id);
    if pretty != id {
        return pretty;
    }
    strip_eleven(&pretty_model_id_settings(id))
}

/// History details model label: [`model_name`] without a catalogue; `""` → `Unknown model`.
pub fn model_label_history(id: &str) -> String {
    if id.is_empty() {
        "Unknown model".into()
    } else {
        model_name(id, &[])
    }
}

// ───────────────────────────── bytes ─────────────────────────────

/// Decimal units: `187_400_000 → "187 MB"`, `1_600_000_000 → "1.6 GB"`, `0 → "0 MB"` (locale decimal separator).
pub fn format_bytes(bytes: u64) -> String {
    let b = bytes as f64;
    if b < 1e6 {
        return if bytes == 0 { "0 MB".into() } else { format!("{} KB", js_round(b / 1000.0).max(1.0) as u64) };
    }
    if b < 1e9 {
        return if b < 1e7 {
            format!("{} MB", locale().localize_decimal(&to_fixed(b / 1e6, 1)))
        } else {
            format!("{} MB", js_round(b / 1e6) as u64)
        };
    }
    format!("{} GB", locale().localize_decimal(&to_fixed(b / 1e9, 1)))
}

/// `500 → "500 MB"`, `2000 → "2 GB"`, `1500 → "1.5 GB"`.
pub fn format_megabytes(mb: u32) -> String {
    if mb < 1000 {
        return format!("{mb} MB");
    }
    let gb = mb as f64 / 1000.0;
    if gb == gb.trunc() {
        format!("{} GB", gb as u64)
    } else {
        format!("{} GB", locale().localize_decimal(&to_fixed(gb, 1)))
    }
}

/// Header badge form without the space: `"187MB"`.
pub fn cache_badge(bytes: u64) -> String {
    format_bytes(bytes).replace(' ', "")
}

// ───────────────────────────── player presentation ─────────────────────────────

/// Widget status label (sentence case per DESIGN.md): `Stopped early`, `Starting`, `Ready`, `Synthesizing`, `Reading`,
/// `Paused`, `Finished`, `Error`.
pub fn status_label(s: &PlayerSnapshot) -> &'static str {
    let st = s.state.status;
    if st == PlayerStatus::Ended && s.warning.is_some() {
        return "Stopped early";
    }
    if s.starting {
        return "Starting";
    }
    match st {
        PlayerStatus::Idle => "Ready",
        PlayerStatus::Loading => "Synthesizing",
        PlayerStatus::Playing => "Reading",
        PlayerStatus::Paused => "Paused",
        PlayerStatus::Ended => "Finished",
        PlayerStatus::Error => "Error",
    }
}

/// Sidebar mini-player label.
pub fn mini_status_label(status: PlayerStatus) -> &'static str {
    match status {
        PlayerStatus::Loading => "Preparing",
        PlayerStatus::Playing => "Now reading",
        PlayerStatus::Paused => "Paused",
        PlayerStatus::Error => "Couldn’t read",
        _ => "",
    }
}

/// The widget's source label: `sourceApp`, else `Replay` for cached replays, else `Vox` (not upper-cased in the new
/// design).
pub fn widget_source_label(s: &PlayerSnapshot) -> String {
    match &s.session {
        Some(sess) => sess
            .source_app
            .clone()
            .filter(|a| !a.is_empty())
            .unwrap_or_else(|| if sess.from_cache { "Replay".into() } else { crate::constants::APP_NAME.into() }),
        None => crate::constants::APP_NAME.into(),
    }
}

/// `Stopped early` (ended) / `Stops early`.
pub fn warning_lead(ended: bool) -> &'static str {
    if ended {
        "Stopped early"
    } else {
        "Stops early"
    }
}

/// Short reason for a non-fatal error.
pub fn warning_reason(code: ReadErrorCode) -> &'static str {
    match code {
        ReadErrorCode::NoApiKey => "no API key",
        ReadErrorCode::Unauthorized => "API key rejected",
        ReadErrorCode::Quota => "out of credits",
        ReadErrorCode::RateLimit => "ElevenLabs is busy",
        ReadErrorCode::Network => "connection lost",
        ReadErrorCode::TooLong => "text too long",
        _ => "synthesis failed",
    }
}

/// Error-card title by code.
pub fn error_title(code: ReadErrorCode) -> &'static str {
    match code {
        ReadErrorCode::NoApiKey => "Add your ElevenLabs API key",
        ReadErrorCode::Unauthorized => "ElevenLabs refused the request",
        ReadErrorCode::Quota => "Out of ElevenLabs credits",
        ReadErrorCode::RateLimit => "ElevenLabs is busy right now",
        ReadErrorCode::Network => "Can\u{2019}t reach ElevenLabs",
        ReadErrorCode::EmptyText => "Nothing to read there",
        ReadErrorCode::TooLong => "That selection is too long",
        _ => "Something went wrong",
    }
}

/// Error-card detail: the error if present and different from the title, else the generic line.
pub fn error_detail(state: &PlayerState) -> String {
    let code = state.error_code.unwrap_or(ReadErrorCode::Unknown);
    match state.error.as_deref().filter(|e| !e.is_empty() && *e != error_title(code)) {
        Some(e) => e.to_string(),
        None => "The text could not be read aloud.".into(),
    }
}

/// Tray tooltip (shell.md §7.2), cut to 127 UTF-16 units keeping a closing `”`.
pub fn tray_tooltip(state: &PlayerState) -> String {
    let app = crate::constants::APP_NAME;
    let quoted = |verb: &str| {
        if state.title.is_empty() {
            format!("{app} \u{00B7} {verb}")
        } else {
            format!("{app} \u{00B7} {verb} \u{201C}{}\u{201D}", state.title)
        }
    };
    let text = match state.status {
        PlayerStatus::Playing => quoted("Reading"),
        PlayerStatus::Paused => quoted("Paused"),
        PlayerStatus::Loading => format!("{app} \u{00B7} Preparing audio\u{2026}"),
        _ => app.to_string(),
    };
    if utf16_len(&text) <= TOOLTIP_MAX {
        return text;
    }
    if text.ends_with('\u{201D}') {
        format!("{}\u{2026}\u{201D}", prefix16(&text, TOOLTIP_MAX - 2))
    } else {
        format!("{}\u{2026}", prefix16(&text, TOOLTIP_MAX - 1))
    }
}

/// A 0..1 meter tone: low credits use the warning tone.
pub fn is_low_share(v: f64) -> bool {
    v < LOW_CREDITS_SHARE
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, Utc};

    fn ms(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> f64 {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap().timestamp_millis() as f64
    }

    #[test]
    fn clocks_and_durations() {
        assert_eq!(format_clock(263.0), "4:23");
        assert_eq!(format_clock(3723.0), "1:02:03");
        assert_eq!(format_clock(0.0), "0:00");
        assert_eq!(remaining_clock(63.0, 263.0, false), "-3:20");
        assert_eq!(remaining_clock(63.0, 263.0, true), "~-3:20");
        assert_eq!(remaining_clock(0.0, 3700.0, false), "-1:01:40");
        assert_eq!(dot_duration(15600.0), "4H20M");
        assert_eq!(dot_duration(3900.0), "1H05M");
        assert_eq!(dot_duration(2280.0), "38M");
        assert_eq!(dot_duration(42.0), "42S");
        assert_eq!(dot_duration(0.0), "0S");
        assert_eq!(dot_duration(60.0), "1M");
        assert_eq!(format_hm_dot(152280.0), "42H18M");
        assert_eq!(format_hm_dot(1080.0), "18M");
        assert_eq!(format_hm_dot(45.0), "45S");
        assert_eq!(format_hm_dot(0.0), "0M");
        assert_eq!(format_hm_dot(360000.0), "100H");
        assert_eq!(duration_speech(15600.0), "4 hours 20 minutes");
        assert_eq!(duration_speech(3600.0), "1 hour");
        assert_eq!(duration_speech(61.0), "1 minute");
        assert_eq!(duration_speech(1.0), "1 second");
        assert_eq!(format_duration(15600.0), "4h 20m");
        assert_eq!(format_duration(300.0), "5m");
        assert_eq!(format_duration(20.0), "20s");
        assert_eq!(format_duration(3600.0), "1h");
        assert_eq!(format_duration(4320.0), "1h 12m");
        assert_eq!(hour_range(21), "21:00\u{2013}22:00");
        assert_eq!(hour_range(23), "23:00\u{2013}00:00");
    }

    #[test]
    fn widget_time() {
        let mut s = PlayerState { status: PlayerStatus::Playing, position_sec: 63.0, duration_sec: 263.0, duration_is_estimate: false, rate: 2.0, ..PlayerState::idle() };
        assert_eq!(widget_time_label(&s), ("\u{2212}1:40".to_string(), false));
        s.duration_is_estimate = true;
        assert_eq!(widget_time_label(&s).0, "\u{2248}1:40");
        s.status = PlayerStatus::Ended;
        assert_eq!(widget_time_label(&s).0, "2:11");
        // The progress bar's accessible value uses media seconds.
        assert_eq!(progress_accessible(&s), "1:03 of about 4:23");
        s.duration_is_estimate = false;
        assert_eq!(progress_accessible(&s), "1:03 of 4:23");
    }

    /// ui-main.md §13 vectors that the first pass did not carry (ARCHITECTURE.md §9).
    #[test]
    fn ui_main_section_13_vectors() {
        let tz = Utc;
        let now = ms(2026, 10, 17, 12, 0);
        assert_eq!(format_relative(&tz, now - 3_585_000.0, now), "60 minutes ago");
        assert_eq!(format_relative(&tz, now - 85_800_000.0, now), "24 hours ago");
        assert_eq!(format_relative(&tz, now + 90_000.0, now), "in 2 minutes");
        assert_eq!(format_compact(999_950.0), "1M");
        assert_eq!(format_compact(123_456.0), "123.5K");
        assert_eq!(format_compact(2.5e9), "2.5B");
        assert_eq!(format_compact(999.6), "999.6");
        assert_eq!(remaining_clock(63.4, 263.0, false), "-3:20");
        assert_eq!(format_clock(199.6), "3:19");
        assert_eq!(rate_label(1.333), "1.33\u{d7}");
        assert_eq!(rate_label(0.0), "1\u{d7}");
        assert_eq!(format_number(2.5), "3");
        assert_eq!(format_hm_dot(59.6), "1M");
        assert_eq!(speak_duration(59.6), "1 minute");
        assert_eq!(day_label(&tz, ms(2026, 10, 16, 23, 59), ms(2026, 10, 17, 0, 1)), "Yesterday");
    }

    #[test]
    fn numbers() {
        assert_eq!(format_number(38766.0), "38,766");
        assert_eq!(format_number(1234567.6), "1,234,568");
        assert_eq!(format_number(999.0), "999");
        assert_eq!(format_number(-1500.0), "-1,500");
        assert_eq!(format_compact(12400.0), "12.4K");
        assert_eq!(format_compact(100000.0), "100K");
        assert_eq!(format_compact(999.0), "999");
        assert_eq!(format_compact(1_500_000.0), "1.5M");
        assert_eq!(format_compact(38766.0), "38.8K");
        assert_eq!(format_compact(999_960.0), "1M");
        assert_eq!(format_compact(0.0), "0");
        assert_eq!(rate_label(1.0), "1\u{d7}");
        assert_eq!(rate_label(1.25), "1.25\u{d7}");
        assert_eq!(rate_label(1.5), "1.5\u{d7}");
        assert_eq!(count_label(1, "read", "reads"), "1 read");
        assert_eq!(count_label(1204, "read", "reads"), "1,204 reads");
        assert_eq!(percent(0.38766), "39%");
    }

    #[test]
    fn text_helpers() {
        assert_eq!(truncate("  hello   world  ", 20), "hello world");
        assert_eq!(truncate("hello world", 6), "hello\u{2026}");
        assert_eq!(display_url("https://www.example.com/a/b?q=1"), "example.com/a/b");
        assert_eq!(display_url("https://example.com/"), "example.com");
        assert_eq!(display_url("not a url"), "not a url");
        assert_eq!(clean_error("TypeError: boom"), "boom");
        assert_eq!(clean_error("  "), "Something went wrong");
        assert_eq!(clean_error("Network: down"), "Network: down");
    }

    #[test]
    fn dates() {
        let tz = Utc;
        let now = ms(2026, 10, 17, 1, 0);
        assert_eq!(format_relative(&tz, now - 30_000.0, now), "just now");
        assert_eq!(format_relative(&tz, now - 300_000.0, now), "5 minutes ago");
        assert_eq!(format_relative(&tz, now - 90_000.0, now), "1 minute ago"); // round(-1.5) = -1
        assert_eq!(format_relative(&tz, now - 7_200_000.0, now), "2 hours ago");
        assert_eq!(format_relative(&tz, ms(2026, 10, 16, 23, 0), now), "2 hours ago");
        assert_eq!(format_relative(&tz, ms(2026, 10, 15, 19, 0), ms(2026, 10, 17, 1, 0)), "2 days ago");
        assert_eq!(format_relative(&tz, ms(2026, 10, 16, 0, 30), ms(2026, 10, 17, 6, 30)), "yesterday"); // 30 h, one midnight
        assert_eq!(format_relative(&tz, ms(2026, 10, 8, 12, 0), ms(2026, 10, 17, 12, 0)), "Oct 8, 2026");
        let now = ms(2026, 10, 17, 21, 5);
        assert_eq!(day_label(&tz, now, now), "Today");
        assert_eq!(day_label(&tz, now + 86_400_000.0, now), "Today");
        assert_eq!(day_label(&tz, ms(2026, 10, 16, 3, 0), now), "Yesterday");
        assert_eq!(day_label(&tz, ms(2026, 10, 12, 3, 0), now), "Monday");
        assert_eq!(day_label(&tz, ms(2026, 9, 14, 3, 0), now), "Mon, Sep 14");
        assert_eq!(day_label(&tz, ms(2025, 9, 14, 3, 0), now), "Sun, Sep 14, 2025");
        assert_eq!(full_date_time(&tz, now), "Saturday, October 17, 2026 at 9:05 PM");
        assert_eq!(date_line(&from_ms(&tz, now)), "Saturday, October 17");
        assert_eq!(row_time(&tz, ms(2026, 10, 1, 9, 5), now), "9:05 AM");
        assert_eq!(greeting(&from_ms(&tz, ms(2026, 10, 17, 4, 59))), "Good evening");
        assert_eq!(greeting(&from_ms(&tz, ms(2026, 10, 17, 5, 0))), "Good morning");
        assert_eq!(weekday_initial("2026-10-17"), "S");
        assert_eq!(weekday_initial("bad"), "\u{b7}");
        assert_eq!(weekday_name("2026-10-12"), "Monday");
        let plus2 = FixedOffset::east_opt(2 * 3600).unwrap();
        assert_eq!(time_of_day(&from_ms(&plus2, ms(2026, 10, 17, 22, 30))), "12:30 AM");
    }

    #[test]
    fn credits_and_tiers() {
        assert_eq!(credits_left_fraction(61234.0, 100000.0), 0.38766);
        assert_eq!(credits_left_fraction(1.0, 0.0), 0.0);
        let now = 1_000_000_000_000.0;
        assert_eq!(reset_label(Some((now + 3.0 * 3_600_000.0) / 1000.0), now).unwrap(), "Resets tomorrow");
        assert_eq!(reset_label(Some((now + 12.0 * DAY_MS) / 1000.0), now).unwrap(), "Resets in 12 days");
        assert_eq!(reset_label(Some((now - 1000.0) / 1000.0), now).unwrap(), "Resets today");
        assert_eq!(reset_label(None, now), None);
        assert_eq!(credits_facts(38766.0, 100000.0), "38.8K of 100K left");
        assert_eq!(credits_detail(38766.0, 100000.0, Some("Resets in 12 days")), "38,766 of 100,000 characters left. Resets in 12 days");
        assert_eq!(tier_label("pro_plus"), "Pro Plus");
        assert_eq!(tier_label(""), "Plan");
        assert_eq!(title_case_words("creator"), "Creator");
        assert_eq!(title_case_words("pro_plus"), "Pro Plus");
    }

    #[test]
    fn models() {
        assert_eq!(pretty_model_id("eleven_flash_v2_5"), "Flash v2.5");
        assert_eq!(pretty_model_id("eleven_v3"), "Eleven v3");
        assert_eq!(pretty_model_id("eleven_multilingual_v2"), "Multilingual v2");
        assert_eq!(pretty_model_id("eleven_english_sts_v2"), "English Sts v2");
        assert_eq!(pretty_model_id("foo"), "foo");
        assert_eq!(pretty_model_id_settings("eleven_turbo_v2_5"), "Eleven Turbo v2.5");
        assert_eq!(pretty_model_id_settings("eleven_v3"), "Eleven v3");
        assert_eq!(model_label_history(""), "Unknown model");
        assert_eq!(model_label_history("eleven_flash_v2_5"), "Flash v2.5");
        assert_eq!(model_label_history("eleven_v3"), "Eleven v3");
        assert_eq!(model_label_history("my_custom_model"), "My Custom Model");
        assert_eq!(model_name("eleven_multilingual_v2", &[]), "Multilingual v2");
        let models = vec![
            TtsModel { model_id: "eleven_flash_v2_5".into(), name: "Eleven Flash v2.5".into(), description: None, languages: vec![], max_chars: None, cost_factor: None },
            TtsModel { model_id: "eleven_v3".into(), name: "Eleven v3".into(), description: None, languages: vec![], max_chars: None, cost_factor: None },
        ];
        assert_eq!(model_label("eleven_flash_v2_5", &models), "Flash v2.5");
        assert_eq!(model_label("eleven_v3", &models), "Eleven v3");
        assert_eq!(model_label("eleven_turbo_v2_5", &models), "Turbo v2.5");
    }

    #[test]
    fn day_first_24h_locale() {
        use crate::locale::{with_locale, DateOrder, Locale};
        let de = Locale::new(",", ".", false, DateOrder::DayFirst);
        let tz = Utc;
        let now = ms(2026, 10, 17, 21, 5);
        with_locale(de, || {
            assert_eq!(format_number(38766.0), "38.766");
            assert_eq!(format_compact(38766.0), "38,8K");
            assert_eq!(credits_facts(38766.0, 100000.0), "38,8K of 100K left");
            assert_eq!(format_bytes(1_600_000_000), "1,6 GB");
            assert_eq!(full_date_time(&tz, now), "Saturday 17 October 2026 at 21:05");
            assert_eq!(date_line(&from_ms(&tz, now)), "Saturday 17 October");
            assert_eq!(day_label(&tz, ms(2026, 9, 14, 3, 0), now), "Mon 14 Sep");
            assert_eq!(day_label(&tz, ms(2025, 9, 14, 3, 0), now), "Sun 14 Sep 2025");
            assert_eq!(format_relative(&tz, ms(2026, 10, 8, 12, 0), ms(2026, 10, 17, 12, 0)), "8 Oct 2026");
            assert_eq!(time_of_day(&from_ms(&tz, ms(2026, 10, 1, 9, 5))), "09:05");
            assert_eq!(format_number_en(20_000.0), "20,000");
        });
    }

    #[test]
    fn bytes() {
        assert_eq!(format_bytes(187_400_000), "187 MB");
        assert_eq!(format_bytes(1_600_000_000), "1.6 GB");
        assert_eq!(format_bytes(0), "0 MB");
        assert_eq!(format_bytes(400), "1 KB");
        assert_eq!(format_bytes(5_250_000), "5.3 MB");
        assert_eq!(format_megabytes(500), "500 MB");
        assert_eq!(format_megabytes(2000), "2 GB");
        assert_eq!(format_megabytes(1500), "1.5 GB");
        assert_eq!(cache_badge(187_400_000), "187MB");
    }

    #[test]
    fn tooltip() {
        let mut s = PlayerState { status: PlayerStatus::Playing, title: "Hello".into(), ..PlayerState::idle() };
        assert_eq!(tray_tooltip(&s), "Vox \u{b7} Reading \u{201C}Hello\u{201D}");
        s.title = "x".repeat(200);
        let t = tray_tooltip(&s);
        assert_eq!(utf16_len(&t), 127);
        assert!(t.ends_with("\u{2026}\u{201D}"));
        s.status = PlayerStatus::Loading;
        assert_eq!(tray_tooltip(&s), "Vox \u{b7} Preparing audio\u{2026}");
        s.status = PlayerStatus::Ended;
        assert_eq!(tray_tooltip(&s), "Vox");
    }
}
