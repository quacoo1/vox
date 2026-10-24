//! The listen-log ledger (contracts.md §6, reader-playback.md §11, persistence.md §5.5): seconds listened per local
//! hour, so analytics credit listening to when it happened rather than to the day the read was created.
//!
//! Decision (ARCHITECTURE.md §8, contracts.md §8 #5/#15): keys are validated **strictly** — an impossible calendar
//! date (`2025-02-31T10`) or a year below 100 is rejected instead of rolling over like JS `Date`. Only hand-edited
//! files can contain such keys. A DST-gap hour resolves forward. The quirk that a value rounding to 0.0 is kept
//! (positivity is tested before rounding, §8 #6) is preserved for parity.

use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Timelike, Datelike};
use serde::{Serialize, Serializer};
use serde_json::Value;

use crate::js::round1;
use crate::json::JsNumber;
use crate::time::{from_ms, resolve_local};

pub use crate::constants::LISTEN_LOG_MAX_KEYS;

/// `{"YYYY-MM-DDTHH": seconds}`; a `BTreeMap` so keys serialize ascending (lexicographic = chronological).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ListenLog(pub BTreeMap<String, f64>);

impl ListenLog {
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn get(&self, key: &str) -> Option<f64> {
        self.0.get(key).copied()
    }
    pub fn iter(&self) -> impl Iterator<Item = (&String, &f64)> {
        self.0.iter()
    }
    /// Sum of all entries.
    pub fn total(&self) -> f64 {
        self.0.values().sum()
    }
}

impl Serialize for ListenLog {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut m = s.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            m.serialize_entry(k, &JsNumber(*v))?;
        }
        m.end()
    }
}

/// `"{year}-{MM}-{DD}T{HH}"` of a local date-time.
pub fn hour_key<Tz: TimeZone>(dt: &DateTime<Tz>) -> String {
    format!("{:04}-{:02}-{:02}T{:02}", dt.year(), dt.month(), dt.day(), dt.hour())
}

/// Hour key of epoch milliseconds in `tz`.
pub fn hour_key_ms<Tz: TimeZone>(tz: &Tz, ms: f64) -> String {
    hour_key(&from_ms(tz, ms))
}

/// Parses `^(\d{4})-(\d{2})-(\d{2})T(\d{2})$` (ASCII digits) into a local wall-clock hour. Rejects month/day/hour out
/// of range, impossible dates and years below 100. Time-zone independent.
pub fn parse_hour_key(key: &str) -> Option<NaiveDateTime> {
    let b = key.as_bytes();
    if b.len() != 13 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<u32> {
        let s = &key[r];
        if s.bytes().all(|c| c.is_ascii_digit()) {
            s.parse().ok()
        } else {
            None
        }
    };
    let (y, mo, d, h) = (num(0..4)?, num(5..7)?, num(8..10)?, num(11..13)?);
    if y < 100 || h > 23 {
        return None;
    }
    let date = NaiveDate::from_ymd_opt(y as i32, mo, d)?;
    Some(date.and_time(NaiveTime::from_hms_opt(h, 0, 0)?))
}

/// The local start of an hour key, or `None` for a malformed key.
pub fn hour_key_date<Tz: TimeZone>(key: &str, tz: &Tz) -> Option<DateTime<Tz>> {
    parse_hour_key(key).map(|n| resolve_local(tz, n))
}

/// Normalises an untrusted ledger: must be a JSON object; keeps entries whose value is a finite number > 0 and whose
/// key parses; values rounded to 0.1; at most [`LISTEN_LOG_MAX_KEYS`] (newest kept). `None` when nothing is left.
pub fn sanitize_listen_log(v: &Value) -> Option<ListenLog> {
    let obj = v.as_object()?;
    let mut map = BTreeMap::new();
    for (k, val) in obj {
        let Some(sec) = val.as_f64().filter(|x| val.is_number() && x.is_finite()) else { continue };
        if sec <= 0.0 || parse_hour_key(k).is_none() {
            continue;
        }
        map.insert(k.clone(), round1(sec));
    }
    trim_to_max(map)
}

/// Same rules for an in-memory ledger.
pub fn sanitize_log(log: &ListenLog) -> Option<ListenLog> {
    let mut map = BTreeMap::new();
    for (k, &sec) in &log.0 {
        if !sec.is_finite() || sec <= 0.0 || parse_hour_key(k).is_none() {
            continue;
        }
        map.insert(k.clone(), round1(sec));
    }
    trim_to_max(map)
}

fn trim_to_max(mut map: BTreeMap<String, f64>) -> Option<ListenLog> {
    if map.is_empty() {
        return None;
    }
    while map.len() > LISTEN_LOG_MAX_KEYS {
        let first = map.keys().next().cloned().expect("non-empty");
        map.remove(&first);
    }
    Some(ListenLog(map))
}

/// A copy of `log` with `sec` more seconds at `key` (a plain copy for `sec <= 0`); never `None` (empty map instead).
/// The key is not validated before insertion; an invalid key is dropped by the sanitize step.
pub fn add_to_listen_log(log: Option<&ListenLog>, key: &str, sec: f64) -> ListenLog {
    let mut next = log.cloned().unwrap_or_default();
    if sec.is_finite() && sec > 0.0 {
        let prev = next.0.get(key).copied().unwrap_or(0.0);
        next.0.insert(key.to_string(), round1(prev + sec));
    }
    sanitize_log(&next).unwrap_or_default()
}

/// Same key count and every value equal; absent and empty compare equal.
pub fn same_listen_log(a: Option<&ListenLog>, b: Option<&ListenLog>) -> bool {
    let ea = BTreeMap::new();
    let ma = a.map(|l| &l.0).unwrap_or(&ea);
    let mb = b.map(|l| &l.0).unwrap_or(&ea);
    ma.len() == mb.len() && ma.iter().all(|(k, v)| mb.get(k) == Some(v))
}

/// When a record's listening happened: one entry per ledger hour (scaled down if the ledger says more than
/// `listened_sec`), plus the uncovered remainder (> 0.05 s) at `created_at_ms`.
pub fn listening_entries<Tz: TimeZone>(
    tz: &Tz,
    created_at_ms: f64,
    listened_sec: f64,
    log: Option<&ListenLog>,
) -> Vec<(DateTime<Tz>, f64)> {
    let total = if listened_sec.is_finite() && listened_sec > 0.0 { listened_sec } else { 0.0 };
    let mut out: Vec<(DateTime<Tz>, f64)> = Vec::new();
    let mut logged = 0.0;
    if let Some(l) = log {
        for (k, &sec) in &l.0 {
            let Some(at) = hour_key_date(k, tz) else { continue };
            if !sec.is_finite() || sec <= 0.0 {
                continue;
            }
            out.push((at, sec));
            logged += sec;
        }
    }
    if logged > total && logged > 0.0 {
        let k = total / logged;
        for e in &mut out {
            e.1 *= k;
        }
        logged = total;
    }
    let rest = total - logged;
    if rest > 0.05 {
        out.push((from_ms(tz, created_at_ms), rest));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Timelike, Utc};
    use serde_json::json;

    fn ms(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> f64 {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap().timestamp_millis() as f64
    }

    #[test]
    fn keys() {
        let dt = Utc.with_ymd_and_hms(2026, 10, 17, 9, 15, 0).unwrap();
        assert_eq!(hour_key(&dt), "2026-10-17T09");
        assert!(parse_hour_key("2026-10-17T09").is_some());
        assert!(parse_hour_key("2026-10-17T24").is_none());
        assert!(parse_hour_key("2026-13-17T09").is_none());
        assert!(parse_hour_key("2025-02-31T10").is_none()); // strict (decision)
        assert!(parse_hour_key("0099-01-01T00").is_none());
        assert!(parse_hour_key("２026-10-17T09").is_none());
    }

    #[test]
    fn worked_example_from_spec() {
        let log = sanitize_listen_log(&json!({"2026-10-17T08": 30, "2026-10-17T09": 50})).unwrap();
        let e = listening_entries(&Utc, ms(2026, 10, 10, 9, 15), 100.0, Some(&log));
        assert_eq!(e.len(), 3);
        assert_eq!((e[0].0.hour(), e[0].1), (8, 30.0));
        assert_eq!((e[1].0.hour(), e[1].1), (9, 50.0));
        assert_eq!(e[2].0.timestamp_millis() as f64, ms(2026, 10, 10, 9, 15));
        assert_eq!(e[2].1, 20.0);

        let scaled = listening_entries(&Utc, ms(2026, 10, 10, 9, 15), 40.0, Some(&log));
        assert_eq!(scaled.len(), 2);
        assert_eq!(scaled[0].1, 15.0);
        assert_eq!(scaled[1].1, 25.0);

        let added = add_to_listen_log(Some(&ListenLog([("2026-10-17T09".to_string(), 50.0)].into())), "2026-10-17T09", 0.06);
        assert_eq!(added.get("2026-10-17T09"), Some(50.1));
    }

    #[test]
    fn sanitize_rules() {
        assert!(sanitize_listen_log(&json!(null)).is_none());
        assert!(sanitize_listen_log(&json!([])).is_none());
        assert!(sanitize_listen_log(&json!({})).is_none());
        let l = sanitize_listen_log(&json!({"2026-10-17T09": "12", "bad": 4, "2026-10-17T10": 0.04, "2026-10-17T11": -1}))
            .unwrap();
        // "12" (a string) dropped, 0.04 kept as 0 (parity quirk), negative dropped
        assert_eq!(l.len(), 1);
        assert_eq!(l.get("2026-10-17T10"), Some(0.0));
        let mut many = serde_json::Map::new();
        for i in 0..450 {
            let d = NaiveDate::from_ymd_opt(2026, 1, 1).unwrap().and_hms_opt(0, 0, 0).unwrap() + chrono::Duration::hours(i);
            many.insert(format!("{}", d.format("%Y-%m-%dT%H")), json!(1));
        }
        let trimmed = sanitize_listen_log(&Value::Object(many)).unwrap();
        assert_eq!(trimmed.len(), 400);
        assert_eq!(trimmed.0.keys().next().unwrap(), "2026-01-03T02"); // the 50 oldest dropped
    }

    #[test]
    fn same_log() {
        let empty = ListenLog::new();
        assert!(same_listen_log(None, Some(&empty)));
        let a = ListenLog([("2026-10-17T09".to_string(), 1.0)].into());
        assert!(!same_listen_log(Some(&a), None));
        assert!(same_listen_log(Some(&a), Some(&a.clone())));
    }

    #[test]
    fn serializes_ascending_with_js_numbers() {
        let l = sanitize_listen_log(&json!({"2026-10-17T09": 50.1, "2026-10-17T08": 30})).unwrap();
        assert_eq!(crate::json::to_js_compact(&l), r#"{"2026-10-17T08":30,"2026-10-17T09":50.1}"#);
    }
}
