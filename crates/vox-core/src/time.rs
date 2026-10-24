//! Local-calendar helpers, generic over [`chrono::TimeZone`] (production passes [`chrono::Local`]; tests pass fixed
//! offsets). All day arithmetic is calendar-based, never `± 86 400 000 ms`, so DST days are neither skipped nor
//! repeated (persistence.md §7). Non-existent local times (spring-forward gaps) resolve **forward** like JS `Date`;
//! ambiguous ones (fall-back) resolve to the **earlier** instant.

use chrono::{DateTime, Datelike, Duration, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Timelike};

/// Epoch milliseconds → zoned date-time. Fractional milliseconds are floored; out-of-range or non-finite → epoch.
pub fn from_ms<Tz: TimeZone>(tz: &Tz, ms: f64) -> DateTime<Tz> {
    let m = if ms.is_finite() { ms.floor() as i64 } else { 0 };
    match tz.timestamp_millis_opt(m) {
        LocalResult::Single(t) => t,
        LocalResult::Ambiguous(a, _) => a,
        LocalResult::None => tz.timestamp_millis_opt(0).earliest().expect("epoch is representable"),
    }
}

/// JS `new Date(ms).toISOString()`: UTC, millisecond precision, `Z` — `2026-10-17T09:44:00.000Z` (contracts.md §3.9
/// `exportedAt`; persistence.md §9 log lines). Fractional milliseconds are floored; non-finite → the epoch.
pub fn iso_utc_ms(ms: f64) -> String {
    let t = from_ms(&chrono::Utc, ms);
    t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// Zoned date-time → epoch milliseconds.
pub fn to_ms<Tz: TimeZone>(dt: &DateTime<Tz>) -> f64 {
    dt.timestamp_millis() as f64
}

/// A local wall-clock time as an instant: gaps move forward (by 1 h, else 30 min, else 2 h), ambiguity takes the
/// earlier instant.
pub fn resolve_local<Tz: TimeZone>(tz: &Tz, naive: NaiveDateTime) -> DateTime<Tz> {
    match tz.from_local_datetime(&naive) {
        LocalResult::Single(t) => t,
        LocalResult::Ambiguous(a, b) => {
            if a <= b {
                a
            } else {
                b
            }
        }
        LocalResult::None => {
            for step in [Duration::hours(1), Duration::minutes(30), Duration::hours(2)] {
                if let Some(t) = tz.from_local_datetime(&(naive + step)).earliest() {
                    return t;
                }
            }
            tz.from_utc_datetime(&naive)
        }
    }
}

/// Local midnight (start of day) of `date`.
pub fn local_midnight<Tz: TimeZone>(tz: &Tz, date: NaiveDate) -> DateTime<Tz> {
    resolve_local(tz, date.and_time(NaiveTime::MIN))
}

/// `YYYY-MM-DD`.
pub fn day_key_of_date(date: NaiveDate) -> String {
    format!("{:04}-{:02}-{:02}", date.year(), date.month(), date.day())
}

/// Local `YYYY-MM-DD` of `dt`.
pub fn day_key<Tz: TimeZone>(dt: &DateTime<Tz>) -> String {
    day_key_of_date(dt.date_naive())
}

/// Local `YYYY-MM-DD` of epoch milliseconds.
pub fn day_key_ms<Tz: TimeZone>(tz: &Tz, ms: f64) -> String {
    day_key(&from_ms(tz, ms))
}

/// Parses `^\d{4}-\d{2}-\d{2}$` into a calendar date (invalid dates → `None`).
pub fn parse_day_key(key: &str) -> Option<NaiveDate> {
    let b = key.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let digits = |r: std::ops::Range<usize>| -> Option<u32> {
        let s = &key[r];
        s.bytes().all(|c| c.is_ascii_digit()).then(|| s.parse().ok()).flatten()
    };
    NaiveDate::from_ymd_opt(digits(0..4)? as i32, digits(5..7)?, digits(8..10)?)
}

/// `date + n` calendar days.
pub fn add_days(date: NaiveDate, n: i64) -> NaiveDate {
    date.checked_add_signed(Duration::days(n)).unwrap_or(date)
}

/// Whole calendar days from `earlier` to `later` (negative when `later` is before).
pub fn calendar_days_between(earlier: NaiveDate, later: NaiveDate) -> i64 {
    (later - earlier).num_days()
}

/// Epoch ms of the local midnight that starts the day containing `ms`.
pub fn start_of_day_ms<Tz: TimeZone>(tz: &Tz, ms: f64) -> f64 {
    to_ms(&local_midnight(tz, from_ms(tz, ms).date_naive()))
}

/// Local hour (0–23) of `dt`.
pub fn local_hour<Tz: TimeZone>(dt: &DateTime<Tz>) -> u32 {
    dt.hour()
}

/// Monday-first weekday index (0 = Monday … 6 = Sunday), JS `(getDay() + 6) % 7`.
pub fn monday_index<Tz: TimeZone>(dt: &DateTime<Tz>) -> usize {
    dt.weekday().num_days_from_monday() as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, Utc};

    #[test]
    fn day_keys() {
        let tz = FixedOffset::east_opt(2 * 3600).unwrap();
        // 2026-10-16T23:30Z is 2026-10-17 01:30 at +02:00
        let ms = Utc.with_ymd_and_hms(2026, 10, 16, 23, 30, 0).unwrap().timestamp_millis() as f64;
        assert_eq!(day_key_ms(&tz, ms), "2026-10-17");
        assert_eq!(day_key_ms(&Utc, ms), "2026-10-16");
        assert_eq!(parse_day_key("2026-02-31"), None);
        assert_eq!(parse_day_key("2026-10-17"), NaiveDate::from_ymd_opt(2026, 10, 17));
        assert_eq!(parse_day_key("2026-1-17"), None);
    }

    #[test]
    fn iso_strings() {
        let ms = Utc.with_ymd_and_hms(2026, 10, 17, 9, 44, 0).unwrap().timestamp_millis() as f64;
        assert_eq!(iso_utc_ms(ms), "2026-10-17T09:44:00.000Z");
        let log = Utc.with_ymd_and_hms(2026, 10, 9, 0, 20, 30).unwrap().timestamp_millis() as f64 + 735.0;
        assert_eq!(iso_utc_ms(log), "2026-10-09T00:20:30.735Z");
        assert_eq!(iso_utc_ms(log + 0.9), "2026-10-09T00:20:30.735Z");
        assert_eq!(iso_utc_ms(f64::NAN), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn midnight_and_days() {
        let d = NaiveDate::from_ymd_opt(2026, 3, 1).unwrap();
        assert_eq!(add_days(d, -1), NaiveDate::from_ymd_opt(2026, 2, 28).unwrap());
        assert_eq!(calendar_days_between(add_days(d, -3), d), 3);
        let m = local_midnight(&Utc, d);
        assert_eq!(m.hour(), 0);
        assert_eq!(monday_index(&m), 6); // 2026-03-01 is a Sunday
    }
}
