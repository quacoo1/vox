//! Analytics summary types (contracts.md §3.7, persistence.md §7.4). The computation is `vox_store::analytics`.

use serde::Serialize;

use crate::history::ReadSource;
use crate::json::js_f64;

/// Analytics range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Default)]
pub enum AnalyticsRange {
    #[serde(rename = "7d")]
    D7,
    #[serde(rename = "30d")]
    #[default]
    D30,
    #[serde(rename = "90d")]
    D90,
    #[serde(rename = "365d")]
    D365,
    #[serde(rename = "all")]
    All,
}

impl AnalyticsRange {
    pub const ALL: [AnalyticsRange; 5] = [Self::D7, Self::D30, Self::D90, Self::D365, Self::All];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::D7 => "7d",
            Self::D30 => "30d",
            Self::D90 => "90d",
            Self::D365 => "365d",
            Self::All => "all",
        }
    }
    /// Invalid → `None` (callers default to 30d).
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.as_str() == s)
    }
    /// Days in the range; `None` for `all`.
    pub fn days(self) -> Option<u32> {
        match self {
            Self::D7 => Some(7),
            Self::D30 => Some(30),
            Self::D90 => Some(90),
            Self::D365 => Some(365),
            Self::All => None,
        }
    }
    /// Segmented-control button text.
    pub fn button(self) -> &'static str {
        match self {
            Self::D7 => "7D",
            Self::D30 => "30D",
            Self::D90 => "90D",
            Self::D365 => "1Y",
            Self::All => "ALL",
        }
    }
    /// `RANGE_LABELS`.
    pub fn label(self) -> &'static str {
        match self {
            Self::D7 => "Last 7 days",
            Self::D30 => "Last 30 days",
            Self::D90 => "Last 90 days",
            Self::D365 => "Last 12 months",
            Self::All => "All time",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    /// Listening that happened in the range (ledger), round1.
    #[serde(serialize_with = "js_f64")]
    pub listened_sec: f64,
    pub reads: u64,
    pub words: u64,
    pub chars: u64,
    #[serde(serialize_with = "js_f64")]
    pub audio_sec: f64,
    pub completed_reads: u64,
    pub characters_billed: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TodayStats {
    #[serde(serialize_with = "js_f64")]
    pub listened_sec: f64,
    pub reads: u64,
    pub words: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
pub struct Streak {
    pub current: u32,
    pub longest: u32,
}

/// One local calendar day.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyEntry {
    /// `YYYY-MM-DD`.
    pub date: String,
    #[serde(serialize_with = "js_f64")]
    pub listened_sec: f64,
    pub reads: u64,
    pub words: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TopApp {
    pub name: String,
    pub reads: u64,
    #[serde(serialize_with = "js_f64")]
    pub listened_sec: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TopVoice {
    /// Group key: `voiceId || voiceName || "unknown"`.
    pub voice_id: String,
    pub name: String,
    pub reads: u64,
    #[serde(serialize_with = "js_f64")]
    pub listened_sec: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SourceCount {
    pub source: ReadSource,
    pub reads: u64,
}

/// The analytics summary for one range (all calendar maths in local time).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalyticsSummary {
    pub range: AnalyticsRange,
    #[serde(serialize_with = "js_f64")]
    pub generated_at: f64,
    pub totals: Totals,
    pub today: TodayStats,
    pub streak: Streak,
    /// One entry per local calendar day of the range window, oldest → newest. Finite ranges: exactly N days
    /// (7/30/90/365). `all` (decision D-AN-2, BRIEF.md "All time uses one consistent window"): every day from the
    /// range start to today, **not** capped at 365, where the start is `max(local day of the oldest valid record,
    /// today − (MAX_ALL_DAYS − 1) calendar days)` (`constants::MAX_ALL_DAYS` = 3 653, about ten years) — records older
    /// than the start fall outside `inRange` everywhere, so totals, averages, "Days active x/N" (N = `daily.len()`)
    /// and the chart all describe the same window, and a malformed `createdAt` (0, −1e14) cannot blow the series up.
    /// The chart buckets long series into weeks/months (`charts::bucket_daily`).
    pub daily: Vec<DailyEntry>,
    /// `[weekday 0=Mon..6=Sun][hour 0..23]` listened seconds, round1.
    pub heatmap: [[f64; 24]; 7],
    pub top_apps: Vec<TopApp>,
    pub top_voices: Vec<TopVoice>,
    pub sources: Vec<SourceCount>,
    pub avg_words_per_read: u64,
    pub avg_listen_wpm: u64,
    pub reading_time_equivalent_sec: u64,
}

impl AnalyticsSummary {
    /// An all-zero summary (used before the first computation).
    pub fn empty(range: AnalyticsRange, now_ms: f64) -> Self {
        Self {
            range,
            generated_at: now_ms,
            totals: Totals::default(),
            today: TodayStats::default(),
            streak: Streak::default(),
            daily: Vec::new(),
            heatmap: [[0.0; 24]; 7],
            top_apps: Vec::new(),
            top_voices: Vec::new(),
            sources: Vec::new(),
            avg_words_per_read: 0,
            avg_listen_wpm: 0,
            reading_time_equivalent_sec: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(AnalyticsRange::parse("365d"), Some(AnalyticsRange::D365));
        assert_eq!(AnalyticsRange::parse("1y"), None);
        assert_eq!(AnalyticsRange::default(), AnalyticsRange::D30);
        assert_eq!(AnalyticsRange::D365.label(), "Last 12 months");
        assert_eq!(AnalyticsRange::All.days(), None);
    }
}
