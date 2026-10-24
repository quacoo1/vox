//! Analytics controller (ui-main.md §5): the range comes from `UiInit::analytics_range` (read by `app::run` from
//! `ui-state.json`, default 30d) and is kept in memory (a re-created page reuses it); every change sends
//! `Command::SaveAnalyticsRange`. Requests (R-Analytics; ARCHITECTURE.md §3.2): an **immediate** `QueryAnalytics` on
//! mount, range change, Retry, `Refresh(Day)` and wake; a **trailing 700 ms debounce** only for
//! `UiUpdate::HistoryChanged` (each one restarts the timer); only the latest `req` applies; leaving the page invalidates
//! in-flight requests; keeps the previous summary while loading. `retrying` = true from Retry until that request
//! answers (the error card's button spins). Empty banner when `totals.reads == 0 && totals.listenedSec == 0` (ruling:
//! listening counts too). `Days active x/N` with N = `daily.len()` (the consistent `all` window, D-AN-2).
//!
//! Charts: the daily chart needs the plot width, which only Slint knows: `analytics-daily-width(float)` (logical px)
//! → [`AnalyticsController::set_plot_width`] stores it; every build re-runs `charts::bucket_daily` with
//! `max_columns = max(1, floor(w / 6))`, `pick_ticks` (cap `width / 40` or `/ 64`) and the axis labels from the cached
//! summary, so a width change needs no new query. While the width is unknown it sends `daily: []` and keeps
//! `daily-empty`. Heatmap (`charts::heat_cells`, now cell, peak): Rust always sends all 24 `heat-hour-labels`; the page
//! shows every 3rd or 6th from its own pitch (`3 × pitch ≥ 34`). Sources, top apps / voices (`charts::top_items`, art
//! per app / voice), the credits block (`convert::credits_data(.., Analytics, ..)`), and every accessible string:
//! per-bar / per-cell `accessible` (the polite live text of the keyboard-selected mark, D-UM-30), the KPI spoken values,
//! the summaries of each chart and list.
//!
//! Implementation notes:
//! - Every build re-buckets from the cached summary (≤ 3 653 days, cheap), so a width change needs no extra state.
//! - Durations shown as values (hero, Today, Avg/Peak, tooltip values) use the redesign's `format_duration` style
//!   ("6h 24m", "38m") with zero shown as `0m` (the old `formatHMDot` zero); spoken twins use `speak_duration`, and the
//!   `shortDuration` places of the spec (audio generated, live texts, top-list values) use `format_duration`
//!   (`analytics.slint` header; the previews in `ui/previews/analytics*.json`).
//! - Axis and weekday labels are title case (`Sep 20`, `Sat`, `Jan 26`): Slint cannot re-case and the redesign never
//!   shows the dot-matrix capitals.
//! - The 700 ms debounce runs on a `slint::Timer` that goes through the app when it fires (`ui::with_app` →
//!   `AppUi::analytics_cb` → [`AnalyticsController::debounce_due`]), so the page is rebuilt with the query in flight
//!   and the dashboard dims at once (§5.2 "while loading … dimmed/busy").
//! - `AppUi` must route `Analytics{req}` answers to this controller; answers that arrive after the page was left are
//!   dropped here anyway (the pending id is cleared on leave).
//! - The last summary is kept across a leave (re-entering shows it dimmed while the mount query runs instead of a
//!   skeleton flash), but the error is not: a re-created page starts with `error: null` (§5.2), so re-entering after a
//!   failed first load shows the skeleton, not the old error card, until the new answer arrives.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::{Datelike, TimeZone};
use slint::{Image, ModelRc, SharedString, VecModel};
use vox_core::analytics::{AnalyticsRange, AnalyticsSummary, DailyEntry};
use vox_core::app::AppRoute;
use vox_core::charts::{self, BucketUnit, ColumnBucket};
use vox_core::constants::{PAGE_REFRESH_DEBOUNCE_MS, READ_WITH_LABEL};
use vox_core::format::{
    clean_error, count_label, format_compact, format_duration, format_number, hour_range, speak_duration, MONTHS_SHORT,
    WEEKDAYS_MON_FIRST, WEEKDAYS_SHORT,
};
use vox_core::js::js_round;
use vox_core::time::{day_key_ms, from_ms, local_hour, monday_index, parse_day_key};

use super::art::ArtSize;
use super::convert::{credits_data, CreditsSurface};
use super::generated::{AnalyticsData, BarData, ChoiceItem, CreditsData, HeatCellData, KpiData, LoadState, ShareRow};
use super::{NavIntent, PageController, UiCtx};
use crate::command::{Command, CoreHandle, RequestId};
use crate::update::{RefreshReason, UiUpdate};

/// `MIN_COL_PITCH`: the narrowest daily column (logical px).
const MIN_COL_PITCH: f32 = 6.0;
/// Axis label pitch with ≤ 7 day columns (weekday names) / otherwise.
const TICK_PITCH_FEW_DAYS: f32 = 40.0;
const TICK_PITCH: f32 = 64.0;
/// Rows of "How you start reads".
const SOURCES_SHOWN: usize = 5;
/// Days in the KPI sparks and the streak dots.
const RECENT_DAYS: usize = 7;

// ───────────────────────────── controller ─────────────────────────────

/// Analytics page state.
#[derive(Default)]
pub struct AnalyticsController {
    /// The selected range (the debounce queries the range current when it fires).
    range: AnalyticsRange,
    /// The daily plot's width in logical px (`None` until reported); every build buckets the cached summary into
    /// `max(1, floor(width / 6))` columns.
    plot_width: Option<f32>,
    /// The latest `QueryAnalytics` (an answer with another id is dropped).
    pending: Option<RequestId>,
    /// The trailing 700 ms debounce for `HistoryChanged`.
    debounce: slint::Timer,
    /// Analytics is the page on screen.
    active: bool,
    /// The last summary that loaded (kept while loading and after a failed refresh).
    summary: Option<Arc<AnalyticsSummary>>,
    /// The cleaned error of the last failed fetch (cleared by the next success).
    error: Option<String>,
    /// Retry after a failed first load is in flight.
    retrying: bool,
}

impl AnalyticsController {
    /// Seeded from `UiInit::analytics_range`.
    pub fn new(range: vox_core::analytics::AnalyticsRange) -> Self {
        Self { range, ..Self::default() }
    }

    /// The range segmented control (MainWindow's header): an unknown value is ignored.
    pub fn set_range(&mut self, cx: &mut UiCtx, value: &str) {
        let Some(range) = AnalyticsRange::parse(value) else {
            tracing::debug!(target: "ui", "analytics: unknown range {value:?} ignored");
            return;
        };
        if range == self.range {
            return;
        }
        self.range = range;
        cx.core.send(Command::SaveAnalyticsRange(range));
        self.query_now(cx.core);
    }

    /// "Try again" on the first-load error card.
    pub fn retry(&mut self, cx: &mut UiCtx) {
        self.retrying = self.summary.is_none() && self.error.is_some();
        self.query_now(cx.core);
    }

    /// Credits "Refresh balance" and the credits error's "Retry": a forced refresh.
    pub fn refresh_credits(&mut self, cx: &mut UiCtx) {
        cx.core.send(Command::RefreshCredits { force: true });
    }

    /// Credits "Add API key" → Settings → Account.
    pub fn add_key(&mut self, cx: &mut UiCtx) {
        cx.nav.navigate(AppRoute::settings("account"));
    }

    /// The daily plot's width in logical px (module docs).
    pub fn set_plot_width(&mut self, cx: &mut UiCtx, width: f32) {
        let _ = cx;
        self.plot_width = (width.is_finite() && width > 0.0).then_some(width);
    }

    // ── requests ──

    /// `QueryAnalytics` now (cancels a pending debounce; the new id is the only one that applies).
    fn query_now(&mut self, core: &CoreHandle) {
        self.debounce.stop();
        self.send_query(core);
    }

    /// Sends a fresh `QueryAnalytics` for the current range and makes it the only answer that applies.
    fn send_query(&mut self, core: &CoreHandle) {
        let req = core.request_id();
        self.pending = Some(req);
        core.send(Command::QueryAnalytics { req, range: self.range });
    }

    /// `HistoryChanged`: at once until something has loaded, else (re)start the 700 ms trailing debounce, which fires
    /// through `AppUi::analytics_cb` (module docs).
    fn schedule(&mut self, core: &CoreHandle) {
        if !self.active {
            return;
        }
        if self.summary.is_none() {
            self.query_now(core);
            return;
        }
        self.debounce.start(slint::TimerMode::SingleShot, Duration::from_millis(PAGE_REFRESH_DEBOUNCE_MS), || {
            super::with_app("analytics refresh debounce", |app| app.analytics_cb(|c, cx| c.debounce_due(cx)));
        });
    }

    /// The `HistoryChanged` debounce elapsed: query the current range (inert once the page was left). The caller,
    /// `AppUi::analytics_cb`, rebuilds the page, so the dashboard dims while the query runs.
    pub(crate) fn debounce_due(&mut self, cx: &mut UiCtx) {
        if self.active {
            self.send_query(cx.core);
        }
    }

    fn on_analytics(&mut self, req: RequestId, result: &Result<Arc<AnalyticsSummary>, String>) -> bool {
        if self.pending != Some(req) {
            return false;
        }
        self.pending = None;
        self.retrying = false;
        match result {
            Ok(summary) => {
                self.summary = Some(Arc::clone(summary));
                self.error = None;
            }
            Err(e) => {
                tracing::warn!(target: "ui", "analytics refresh failed: {e}");
                self.error = Some(clean_error(e));
            }
        }
        true
    }

    /// The page state the pure builder needs.
    fn view(&self) -> View<'_> {
        View {
            range: self.range,
            summary: self.summary.as_deref(),
            loading: self.pending.is_some(),
            error: self.error.as_deref(),
            retrying: self.retrying,
            plot_width: self.plot_width,
        }
    }
}

/// `max(1, floor(width / MIN_COL_PITCH))`.
fn max_columns(width: f32) -> usize {
    ((width / MIN_COL_PITCH).floor() as usize).max(1)
}

impl PageController for AnalyticsController {
    type Data = AnalyticsData;

    fn on_enter(&mut self, cx: &mut UiCtx, route: &AppRoute, intent: &NavIntent) {
        let _ = (route, intent);
        self.active = true;
        self.retrying = false;
        // A re-created page starts with `error: null` (ui-main.md §5.2): no summary → skeleton while loading, not the
        // previous visit's error card.
        self.error = None;
        self.query_now(cx.core);
        // The credits card's mount refresh (the core's 60 s cache answers most of them).
        cx.core.send(Command::RefreshCredits { force: false });
    }

    fn on_leave(&mut self, cx: &mut UiCtx) {
        let _ = cx;
        self.active = false;
        self.debounce.stop();
        // In-flight answers are dropped.
        self.pending = None;
        self.retrying = false;
    }

    fn on_update(&mut self, cx: &mut UiCtx, update: &UiUpdate) -> bool {
        match update {
            UiUpdate::Analytics { req, result } => self.on_analytics(*req, result),
            UiUpdate::HistoryChanged => {
                self.schedule(cx.core);
                false
            }
            UiUpdate::Credits(_) => true,
            UiUpdate::Refresh(reason) => {
                if *reason == RefreshReason::Day && self.active {
                    self.query_now(cx.core);
                }
                true
            }
            _ => false,
        }
    }

    fn on_tick(&mut self, cx: &mut UiCtx) {
        // The heatmap's now ring and the today column are rebuilt from `cx.now_ms` by `build`.
        let _ = cx;
    }

    fn on_wake(&mut self, cx: &mut UiCtx) {
        if self.active {
            self.query_now(cx.core);
        }
    }

    fn build(&self, cx: &mut UiCtx) -> AnalyticsData {
        let credits = credits_data(cx.credits, CreditsSurface::Analytics, cx.demo, cx.now_ms);
        let view = self.view();
        let art = &mut *cx.art;
        build_data(&chrono::Local, &view, cx.now_ms, credits, &mut |seed: &str| art.image(seed, ArtSize::Compact))
    }
}

// ───────────────────────────── pure view building ─────────────────────────────

/// What the page shows, independent of Slint timers and the art cache.
#[derive(Debug, Clone, Copy)]
pub(crate) struct View<'a> {
    pub range: AnalyticsRange,
    pub summary: Option<&'a AnalyticsSummary>,
    /// A query is in flight.
    pub loading: bool,
    pub error: Option<&'a str>,
    pub retrying: bool,
    pub plot_width: Option<f32>,
}

fn model<T: Clone + 'static>(items: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(items))
}

/// Durations shown as values: `format_duration` style, zero as `0m`.
pub(crate) fn hm(sec: f64) -> String {
    let rounded = if sec.is_finite() { js_round(sec.max(0.0)) } else { 0.0 };
    if rounded <= 0.0 {
        "0m".to_string()
    } else {
        format_duration(sec)
    }
}

/// `x > 0` (false for NaN).
fn positive(x: f64) -> bool {
    x > 0.0
}

/// `"{n} {singular|plural}"` with a plain integer (no grouping).
fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The Days-active meter's spoken value, `{active} of {total} days` (ui-main.md §5.6; `day` when the window is one
/// day; D-UM-30): `AnalyticsData.days-active-accessible`, the meter's `accessible-value`.
pub(crate) fn days_active_accessible(active: usize, total: usize) -> String {
    format!("{active} of {total} {}", if total == 1 { "day" } else { "days" })
}

/// The range buttons of the header's segmented control.
fn range_choices() -> ModelRc<ChoiceItem> {
    model(
        AnalyticsRange::ALL
            .iter()
            .map(|r| ChoiceItem { value: r.as_str().into(), label: r.button().into(), accessible_label: r.label().into() })
            .collect(),
    )
}

/// `Tue, Sep 30` for a day key (the raw key when it does not parse).
fn day_title(key: &str) -> String {
    match parse_day_key(key) {
        Some(d) => format!(
            "{}, {} {}",
            WEEKDAYS_SHORT[d.weekday().num_days_from_sunday() as usize],
            MONTHS_SHORT[d.month0() as usize],
            d.day()
        ),
        None => key.to_string(),
    }
}

/// Axis label in title case: month `Jan 26` (January with the 2-digit year) / `Sep`; ≤ 7 day columns: `Sun`…;
/// otherwise `Sep 14` (day not zero-padded).
pub(crate) fn axis_label(b: &ColumnBucket, unit: BucketUnit, total: usize) -> String {
    let m = MONTHS_SHORT[b.start.month0() as usize];
    match unit {
        BucketUnit::Month if b.start.month() == 1 => format!("{m} {:02}", b.start.year().rem_euclid(100)),
        BucketUnit::Month => m.to_string(),
        BucketUnit::Day if total <= 7 => WEEKDAYS_SHORT[b.start.weekday().num_days_from_sunday() as usize].to_string(),
        _ => format!("{m} {}", b.start.day()),
    }
}

/// The first day with the strictly largest positive listening.
fn peak_day(daily: &[DailyEntry]) -> Option<&DailyEntry> {
    let mut best: Option<&DailyEntry> = None;
    for d in daily {
        if d.listened_sec > 0.0 && best.is_none_or(|b| d.listened_sec > b.listened_sec) {
            best = Some(d);
        }
    }
    best
}

/// The daily chart's columns for a plot `width` px wide (§5.8 bucketing, ticks, tooltips, live texts).
pub(crate) fn daily_bars(daily: &[DailyEntry], width: f32, today_key: &str) -> Vec<BarData> {
    let (unit, buckets) = charts::bucket_daily(daily, max_columns(width), today_key);
    let n = buckets.len();
    let pitch = if unit == BucketUnit::Day && n <= 7 { TICK_PITCH_FEW_DAYS } else { TICK_PITCH };
    let cap = ((width / pitch).floor() as usize).max(1);
    let ticks: HashSet<usize> = charts::pick_ticks(n, cap).into_iter().collect();
    let listened: Vec<f64> = buckets.iter().map(|b| b.listened_sec).collect();
    let levels = charts::bar_fractions(&listened);
    buckets
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let title = charts::bucket_title(b, unit);
            BarData {
                level: levels[i] as f32,
                label: if ticks.contains(&i) { axis_label(b, unit, n).into() } else { SharedString::new() },
                today: b.is_today,
                zero: !positive(b.listened_sec),
                tooltip_title: title.as_str().into(),
                tooltip_value: hm(b.listened_sec).into(),
                tooltip_sub: format!(
                    "{} \u{00B7} {} words",
                    count_label(b.reads, "read", "reads"),
                    format_number(b.words as f64)
                )
                .into(),
                accessible: format!("{title}: {}, {} reads", format_duration(b.listened_sec), b.reads).into(),
            }
        })
        .collect()
}

/// `dailySummary` (computed from the raw series; an active day here means listening > 0).
pub(crate) fn daily_summary(daily: &[DailyEntry], range: AnalyticsRange) -> String {
    let lower = range.label().to_lowercase();
    let active = daily.iter().filter(|d| d.listened_sec > 0.0).count() as u64;
    let Some(peak) = peak_day(daily).filter(|_| active > 0) else {
        return format!("Daily listening, {lower}: nothing listened yet.");
    };
    let total: f64 = daily.iter().map(|d| d.listened_sec.max(0.0)).sum();
    format!(
        "Daily listening, {lower}: {} over {} active {}. Busiest day {} with {}.",
        speak_duration(total),
        format_number(active as f64),
        if active == 1 { "day" } else { "days" },
        day_title(&peak.date),
        speak_duration(peak.listened_sec)
    )
}

/// The heatmap cells (Monday→Sunday × 00→23), the `Peak Sat 21:00` meta and the summary.
pub(crate) fn heat<Tz: TimeZone>(tz: &Tz, summary: &AnalyticsSummary, now_ms: f64) -> (Vec<HeatCellData>, String, String) {
    let (cells, _max, peak, _total) = charts::heat_cells(&summary.heatmap);
    let now = from_ms(tz, now_ms);
    let (now_day, now_hour) = (monday_index(&now), local_hour(&now) as usize);
    let short = |day: usize| WEEKDAYS_MON_FIRST[day.min(6)].get(..3).unwrap_or("").to_string();
    let data = cells
        .iter()
        .map(|c| {
            let range = hour_range(c.hour as u32);
            HeatCellData {
                day: c.day as i32,
                hour: c.hour as i32,
                level: c.level as f32,
                peak: peak.is_some_and(|p| p.day == c.day && p.hour == c.hour),
                now: c.day == now_day && c.hour == now_hour,
                tooltip_title: format!("{} \u{00B7} {range}", short(c.day)).into(),
                tooltip_value: hm(c.value).into(),
                accessible: format!("{} {range}: {}", WEEKDAYS_MON_FIRST[c.day.min(6)], format_duration(c.value)).into(),
            }
        })
        .collect();
    let (meta, spoken) = match peak {
        Some(p) => (
            format!("Peak {} {:02}:00", short(p.day), p.hour),
            format!(
                "Listening by weekday and hour. Busiest time: {}s, {}, {}.",
                WEEKDAYS_MON_FIRST[p.day.min(6)],
                hour_range(p.hour as u32),
                speak_duration(p.value)
            ),
        ),
        None => (String::new(), "Listening by weekday and hour: no listening yet.".to_string()),
    };
    (data, meta, spoken)
}

/// "How you start reads": sources with reads, most first (stable), top 5; shares of **all** reads.
pub(crate) fn source_rows(summary: &AnalyticsSummary) -> (Vec<ShareRow>, String) {
    let total: u64 = summary.sources.iter().map(|s| s.reads).sum();
    let mut shown: Vec<_> = summary.sources.iter().filter(|s| s.reads > 0).collect();
    shown.sort_by_key(|s| std::cmp::Reverse(s.reads));
    shown.truncate(SOURCES_SHOWN);
    let mut spoken = Vec::new();
    let rows = shown
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let share = if total > 0 { s.reads as f64 / total as f64 } else { 0.0 };
            let pct = js_round(share * 100.0) as i64;
            let label = s.source.label();
            spoken.push(format!("{label} {} ({pct}%)", s.reads));
            ShareRow {
                key: s.source.as_str().into(),
                rank: SharedString::new(),
                icon: s.source.icon().into(),
                art: Image::default(),
                label: label.into(),
                sub: SharedString::new(),
                value: format_number(s.reads as f64).into(),
                percent: format!("{pct}%").into(),
                fraction: share as f32,
                first: i == 0,
                accessible: format!("{label}, {}, {pct}%", count_label(s.reads, "read", "reads")).into(),
            }
        })
        .collect();
    let summary_text = if spoken.is_empty() {
        "Reads by source: no reads yet.".to_string()
    } else {
        format!("Reads by source: {}.", spoken.join(", "))
    };
    (rows, summary_text)
}

/// A top list (`topItems` + `TopList`): `(key, name, reads, listenedSec)` rows in main's order; the art seed is the
/// key (app name / voice id).
pub(crate) fn top_rows(
    title: &str,
    rows: &[(String, String, u64, f64)],
    art: &mut dyn FnMut(&str) -> Image,
) -> (Vec<ShareRow>, String) {
    let by_listening = rows.iter().any(|r| r.3 > 0.0);
    let items = charts::top_items(rows);
    let max = items.iter().map(|t| t.value).fold(0.0, f64::max);
    let mut spoken = Vec::new();
    let out = items
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let text = if by_listening { format_duration(t.value) } else { plural(t.value as u64, "read", "reads") };
            spoken.push(format!("{} {text}", t.name));
            let accessible =
                if by_listening { format!("{}, {text}, {}", t.name, t.sub) } else { format!("{}, {}", t.name, t.sub) };
            ShareRow {
                key: t.key.as_str().into(),
                rank: format!("{:02}", i + 1).into(),
                icon: SharedString::new(),
                art: art(&t.key),
                label: t.name.as_str().into(),
                sub: t.sub.as_str().into(),
                value: t.display.as_str().into(),
                percent: SharedString::new(),
                fraction: if max > 0.0 { (t.value / max) as f32 } else { 0.0 },
                first: i == 0,
                accessible: accessible.into(),
            }
        })
        .collect();
    let summary_text =
        if spoken.is_empty() { format!("{title}: no data yet.") } else { format!("{title}: {}.", spoken.join(", ")) };
    (out, summary_text)
}

/// The four KPI tiles (§5.7).
pub(crate) fn kpis(summary: &AnalyticsSummary) -> Vec<KpiData> {
    let t = &summary.totals;
    let last_week = &summary.daily[summary.daily.len().saturating_sub(RECENT_DAYS)..];
    let spark = |values: Vec<f64>| model(charts::bar_fractions(&values).into_iter().map(|v| v as f32).collect());
    let finished_pct = if t.reads > 0 { js_round(t.completed_reads as f64 / t.reads as f64 * 100.0) as i64 } else { 0 };
    let streak = summary.streak;
    let days = |n: u32| if n == 1 { "day" } else { "days" };
    vec![
        KpiData {
            icon: "type".into(),
            label: "Words read".into(),
            value: format_compact(t.words as f64).into(),
            value_accessible: format!("{} words", format_number(t.words as f64)).into(),
            spark_label: "Words read, last 7 days".into(),
            unit: SharedString::new(),
            sub: if t.reads > 0 {
                format!("{} per read on average", format_number(summary.avg_words_per_read as f64))
            } else {
                "Across every read".to_string()
            }
            .into(),
            spark: spark(last_week.iter().map(|d| d.words as f64).collect()),
            dots: model(Vec::new()),
        },
        KpiData {
            icon: "play".into(),
            label: "Reads".into(),
            value: format_number(t.reads as f64).into(),
            value_accessible: format_number(t.reads as f64).into(),
            spark_label: "Reads, last 7 days".into(),
            unit: SharedString::new(),
            sub: if t.reads > 0 { format!("{finished_pct}% listened to the end") } else { "No reads yet".to_string() }.into(),
            spark: spark(last_week.iter().map(|d| d.reads as f64).collect()),
            dots: model(Vec::new()),
        },
        KpiData {
            icon: "zap".into(),
            label: "Streak".into(),
            value: format_number(streak.current as f64).into(),
            value_accessible: format!("{} {}", format_number(streak.current as f64), days(streak.current)).into(),
            spark_label: SharedString::new(),
            unit: days(streak.current).into(),
            sub: format!("Longest {} {}", format_number(streak.longest as f64), days(streak.longest)).into(),
            spark: model(Vec::new()),
            dots: model(charts::recent_activity(&summary.daily, RECENT_DAYS)),
        },
        KpiData {
            icon: "gauge".into(),
            label: "Pace".into(),
            value: format_number(summary.avg_listen_wpm as f64).into(),
            value_accessible: format!("{} words per minute", format_number(summary.avg_listen_wpm as f64)).into(),
            spark_label: SharedString::new(),
            unit: "wpm".into(),
            sub: if summary.reading_time_equivalent_sec > 0 {
                format!("\u{2248} {} of silent reading", format_duration(summary.reading_time_equivalent_sec as f64))
            } else {
                "Words per minute heard".to_string()
            }
            .into(),
            spark: model(Vec::new()),
            dots: model(Vec::new()),
        },
    ]
}

/// The whole page struct (`art` renders the top lists' art for a seed).
pub(crate) fn build_data<Tz: TimeZone>(
    tz: &Tz,
    view: &View,
    now_ms: f64,
    credits: CreditsData,
    art: &mut dyn FnMut(&str) -> Image,
) -> AnalyticsData {
    let range = view.range;
    let empty_title = match range {
        AnalyticsRange::All => "Nothing listened yet".to_string(),
        r => format!("Nothing listened in the {}", r.label().to_lowercase()),
    };
    let mut d = AnalyticsData {
        state: LoadState::Loading,
        dimmed: false,
        retrying: view.retrying,
        range: range.as_str().into(),
        ranges: range_choices(),
        range_label: range.label().into(),
        error: view.error.unwrap_or("").into(),
        empty_title: empty_title.into(),
        empty_body: format!(
            "Select text anywhere, right-click and choose \u{201C}{READ_WITH_LABEL}\u{201D}. Your stats fill in as you listen."
        )
        .into(),
        sources_empty: "No reads in this range yet".into(),
        credits,
        ..AnalyticsData::default()
    };
    let Some(s) = view.summary else {
        d.state = if view.error.is_some() { LoadState::Error } else { LoadState::Loading };
        return d;
    };
    let t = &s.totals;
    d.state = LoadState::Ready;
    d.dimmed = view.loading;
    d.refresh_failed = !view.loading && view.error.is_some();
    d.empty = t.reads == 0 && !positive(t.listened_sec);

    // Hero.
    let (active, total_days) = charts::active_days(&s.daily);
    d.listened = hm(t.listened_sec).into();
    d.listened_accessible = speak_duration(t.listened_sec).into();
    d.today = hm(s.today.listened_sec).into();
    d.today_accessible = format!("Today, {}", speak_duration(s.today.listened_sec)).into();
    d.days_active = format!("{active}/{total_days}").into();
    d.days_active_accessible = days_active_accessible(active, total_days).into();
    d.days_active_fraction = if total_days > 0 { active as f32 / total_days as f32 } else { 0.0 };
    d.audio_generated = format!("{} of audio generated", format_duration(t.audio_sec)).into();
    d.kpis = model(kpis(s));

    // Daily listening.
    let avg_active = if active > 0 { t.listened_sec / active as f64 } else { 0.0 };
    d.daily_avg = format!("Avg {}", hm(avg_active)).into();
    d.daily_avg_accessible = format!("Average per active day, {}", speak_duration(avg_active)).into();
    if let Some(peak) = peak_day(&s.daily) {
        d.daily_peak = format!("Peak {}", hm(peak.listened_sec)).into();
        d.daily_peak_accessible = format!("Busiest day, {}", speak_duration(peak.listened_sec)).into();
    }
    if !s.daily.iter().any(|day| day.listened_sec > 0.0) {
        d.daily_empty = "No listening in this range yet".into();
    }
    d.daily_summary = daily_summary(&s.daily, range).into();
    d.daily = match view.plot_width {
        Some(width) if width > 0.0 => model(daily_bars(&s.daily, width, &day_key_ms(tz, now_ms))),
        _ => model(Vec::new()),
    };

    // When you listen.
    let (cells, peak, spoken) = heat(tz, s, now_ms);
    d.heat = model(cells);
    d.heat_peak = peak.into();
    d.heat_summary = spoken.into();
    d.heat_day_labels =
        model(WEEKDAYS_MON_FIRST.iter().map(|w| SharedString::from(w.get(..3).unwrap_or(*w))).collect());
    d.heat_hour_labels = model((0..24).map(|h| SharedString::from(format!("{h:02}"))).collect());

    // Sources and top lists.
    let (sources, sources_summary) = source_rows(s);
    d.sources = model(sources);
    d.sources_summary = sources_summary.into();
    let apps: Vec<(String, String, u64, f64)> =
        s.top_apps.iter().map(|a| (a.name.clone(), a.name.clone(), a.reads, a.listened_sec)).collect();
    let (top_apps, apps_summary) = top_rows("Top apps by listening time", &apps, art);
    d.top_apps = model(top_apps);
    d.top_apps_summary = apps_summary.into();
    let voices: Vec<(String, String, u64, f64)> =
        s.top_voices.iter().map(|v| (v.voice_id.clone(), v.name.clone(), v.reads, v.listened_sec)).collect();
    let (top_voices, voices_summary) = top_rows("Top voices by listening time", &voices, art);
    d.top_voices = model(top_voices);
    d.top_voices_summary = voices_summary.into();

    d.billed = format!("{} billed", format_compact(t.characters_billed as f64)).into();
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, NaiveDate};
    use slint::Model;
    use vox_core::analytics::{SourceCount, Streak, TodayStats, TopApp, TopVoice, Totals};
    use vox_core::history::ReadSource;
    use vox_core::locale::{with_locale, Locale};
    use vox_core::time::{add_days, day_key_of_date};

    /// 2026-10-17 16:30 UTC, a Saturday.
    const NOW: f64 = 1_792_254_600_000.0;

    fn tz() -> FixedOffset {
        FixedOffset::east_opt(0).unwrap()
    }

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 17).unwrap()
    }

    fn days(n: usize, listened: impl Fn(usize) -> f64) -> Vec<DailyEntry> {
        (0..n)
            .map(|i| DailyEntry {
                date: day_key_of_date(add_days(today(), i as i64 - (n as i64 - 1))),
                listened_sec: listened(i),
                reads: if listened(i) > 0.0 { 2 } else { 0 },
                words: if listened(i) > 0.0 { 300 } else { 0 },
            })
            .collect()
    }

    fn summary(range: AnalyticsRange, daily: Vec<DailyEntry>) -> AnalyticsSummary {
        let mut s = AnalyticsSummary::empty(range, NOW);
        s.daily = daily;
        s
    }

    fn no_art(_: &str) -> Image {
        Image::default()
    }

    fn view(s: Option<&AnalyticsSummary>, range: AnalyticsRange) -> View<'_> {
        View { range, summary: s, loading: false, error: None, retrying: false, plot_width: Some(731.0) }
    }

    fn credits() -> CreditsData {
        CreditsData::default()
    }

    fn build(v: &View) -> AnalyticsData {
        build_data(&tz(), v, NOW, credits(), &mut no_art)
    }

    #[test]
    fn hm_style_and_zero() {
        assert_eq!(hm(0.0), "0m");
        assert_eq!(hm(0.4), "0m");
        assert_eq!(hm(54.0), "54s");
        assert_eq!(hm(23_040.0), "6h 24m");
        assert_eq!(hm(2280.0), "38m");
        assert_eq!(hm(f64::NAN), "0m");
    }

    #[test]
    fn max_columns_and_width() {
        assert_eq!(max_columns(731.0), 121);
        assert_eq!(max_columns(3.0), 1);
        assert_eq!(max_columns(640.0), 106);
    }

    #[test]
    fn loading_error_and_retry_states() {
        with_locale(Locale::EN_US, || {
            let d = build(&view(None, AnalyticsRange::D30));
            assert_eq!(d.state, LoadState::Loading);
            assert_eq!(d.range, "30d");
            assert_eq!(d.range_label, "Last 30 days");
            assert_eq!(d.ranges.row_count(), 5);
            let r = d.ranges.row_data(3).unwrap();
            assert_eq!((r.value.as_str(), r.label.as_str(), r.accessible_label.as_str()), ("365d", "1Y", "Last 12 months"));
            let mut v = view(None, AnalyticsRange::D30);
            v.error = Some("disk on fire");
            v.retrying = true;
            let d = build(&v);
            assert_eq!(d.state, LoadState::Error);
            assert_eq!(d.error, "disk on fire");
            assert!(d.retrying);
        });
    }

    #[test]
    fn empty_summary_matches_the_preview() {
        with_locale(Locale::EN_US, || {
            let s = summary(AnalyticsRange::D30, days(30, |_| 0.0));
            let d = build(&view(Some(&s), AnalyticsRange::D30));
            assert_eq!(d.state, LoadState::Ready);
            assert!(d.empty);
            assert_eq!(d.empty_title, "Nothing listened in the last 30 days");
            assert_eq!(
                d.empty_body,
                "Select text anywhere, right-click and choose \u{201C}Read with Vox\u{201D}. Your stats fill in as you listen."
            );
            assert_eq!(d.listened, "0m");
            assert_eq!(d.listened_accessible, "0 seconds");
            assert_eq!(d.today_accessible, "Today, 0 seconds");
            assert_eq!(d.days_active, "0/30");
            assert_eq!(d.days_active_accessible, "0 of 30 days");
            assert_eq!(d.audio_generated, "0s of audio generated");
            assert_eq!(d.daily_avg, "Avg 0m");
            assert_eq!(d.daily_peak, "");
            assert_eq!(d.daily_empty, "No listening in this range yet");
            assert_eq!(d.daily_summary, "Daily listening, last 30 days: nothing listened yet.");
            assert_eq!(d.heat.row_count(), 168);
            assert_eq!(d.heat_peak, "");
            assert_eq!(d.heat_summary, "Listening by weekday and hour: no listening yet.");
            assert_eq!(d.sources_summary, "Reads by source: no reads yet.");
            assert_eq!(d.top_apps_summary, "Top apps by listening time: no data yet.");
            assert_eq!(d.top_voices_summary, "Top voices by listening time: no data yet.");
            assert_eq!(d.billed, "0 billed");
            let words = d.kpis.row_data(0).unwrap();
            assert_eq!((words.value.as_str(), words.sub.as_str()), ("0", "Across every read"));
            let reads = d.kpis.row_data(1).unwrap();
            assert_eq!(reads.sub, "No reads yet");
            let pace = d.kpis.row_data(3).unwrap();
            assert_eq!((pace.value_accessible.as_str(), pace.sub.as_str()), ("0 words per minute", "Words per minute heard"));
            // The all-time title.
            let all = summary(AnalyticsRange::All, days(12, |_| 0.0));
            assert_eq!(build(&view(Some(&all), AnalyticsRange::All)).empty_title, "Nothing listened yet");
        });
    }

    #[test]
    fn empty_banner_counts_listening_too() {
        let mut s = summary(AnalyticsRange::D7, days(7, |_| 0.0));
        s.totals.listened_sec = 120.0; // a replay heard in the range, no new read
        assert!(!build(&view(Some(&s), AnalyticsRange::D7)).empty);
    }

    #[test]
    fn hero_kpis_and_billed() {
        with_locale(Locale::EN_US, || {
            let mut s = summary(AnalyticsRange::D30, days(30, |i| if i % 5 == 0 { 0.0 } else { 600.0 + i as f64 }));
            s.totals = Totals {
                listened_sec: 23_040.0,
                reads: 107,
                words: 72_187,
                chars: 400_000,
                audio_sec: 25_140.0,
                completed_reads: 87,
                characters_billed: 368_200,
            };
            s.today = TodayStats { listened_sec: 2280.0, reads: 7, words: 6890 };
            s.streak = Streak { current: 13, longest: 21 };
            s.avg_words_per_read = 675;
            s.avg_listen_wpm = 166;
            s.reading_time_equivalent_sec = 18_180;
            let mut v = view(Some(&s), AnalyticsRange::D30);
            v.loading = true;
            let d = build(&v);
            assert!(d.dimmed);
            assert!(!d.refresh_failed);
            assert_eq!(d.listened, "6h 24m");
            assert_eq!(d.listened_accessible, "6 hours 24 minutes");
            assert_eq!(d.today, "38m");
            assert_eq!(d.today_accessible, "Today, 38 minutes");
            assert_eq!(d.days_active, "24/30");
            assert_eq!(d.days_active_accessible, "24 of 30 days");
            assert!((d.days_active_fraction - 0.8).abs() < 1e-6);
            assert_eq!(d.audio_generated, "6h 59m of audio generated");
            assert_eq!(d.billed, "368.2K billed");
            let k: Vec<KpiData> = d.kpis.iter().collect();
            assert_eq!(k.len(), 4);
            assert_eq!((k[0].value.as_str(), k[0].value_accessible.as_str()), ("72.2K", "72,187 words"));
            assert_eq!(k[0].sub, "675 per read on average");
            assert_eq!(k[0].spark.row_count(), 7);
            assert_eq!((k[1].value.as_str(), k[1].sub.as_str()), ("107", "81% listened to the end"));
            assert_eq!((k[2].value.as_str(), k[2].unit.as_str(), k[2].value_accessible.as_str()), ("13", "days", "13 days"));
            assert_eq!(k[2].sub, "Longest 21 days");
            assert_eq!(k[2].dots.row_count(), 7);
            assert_eq!((k[3].value.as_str(), k[3].unit.as_str()), ("166", "wpm"));
            assert_eq!(k[3].sub, "\u{2248} 5h 3m of silent reading");
            // A failed background refresh keeps the dashboard and flags the eyebrow.
            let mut v = view(Some(&s), AnalyticsRange::D30);
            v.error = Some("offline");
            let d = build(&v);
            assert!(d.refresh_failed && !d.dimmed);
            assert_eq!(d.state, LoadState::Ready);
            // Singular streak.
            s.streak = Streak { current: 1, longest: 1 };
            let k = kpis(&s);
            assert_eq!((k[2].unit.as_str(), k[2].sub.as_str()), ("day", "Longest 1 day"));
        });
    }

    #[test]
    fn daily_chart_buckets_ticks_and_strings() {
        with_locale(Locale::EN_US, || {
            let daily = days(30, |i| if i == 1 { 0.0 } else { 300.0 + 60.0 * i as f64 });
            let bars = daily_bars(&daily, 731.0, &day_key_of_date(today()));
            assert_eq!(bars.len(), 30);
            let labelled: Vec<(usize, String)> =
                bars.iter().enumerate().filter(|(_, b)| !b.label.is_empty()).map(|(i, b)| (i, b.label.to_string())).collect();
            assert_eq!(labelled.first().unwrap(), &(2, "Sep 20".to_string()));
            assert_eq!(labelled.last().unwrap(), &(29, "Oct 17".to_string()));
            assert!(bars[29].today && !bars[28].today);
            assert!(bars[1].zero && bars[1].level == 0.0);
            assert_eq!(bars[1].tooltip_value, "0m");
            assert_eq!(bars[1].accessible, "Sat, Sep 19: 0s, 0 reads");
            assert_eq!(bars[0].tooltip_title, "Fri, Sep 18");
            assert_eq!(bars[0].tooltip_value, "5m");
            assert_eq!(bars[0].tooltip_sub, "2 reads \u{00B7} 300 words");
            assert_eq!(bars[29].level, 1.0);
            // 7 days: weekday labels on every column.
            let week = daily_bars(&days(7, |_| 60.0), 731.0, &day_key_of_date(today()));
            assert_eq!(week.iter().map(|b| b.label.to_string()).collect::<Vec<_>>(), ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"]);
            // 365 days at 640 px → 53 weekly columns, the newest ending today.
            let year = daily_bars(&days(365, |_| 60.0), 640.0, &day_key_of_date(today()));
            assert_eq!(year.len(), 53);
            assert_eq!(year[0].tooltip_title, "Oct 18");
            assert_eq!(year[52].tooltip_title, "Oct 11 \u{2013} Oct 17");
            assert!(year[52].today);
            assert_eq!(year[52].label, "Oct 11");
            // At 300 px → months, January with its year.
            let months = daily_bars(&days(365, |_| 60.0), 300.0, &day_key_of_date(today()));
            assert_eq!(months.len(), 13);
            assert_eq!(months[3].tooltip_title, "January 2026");
            let (unit, buckets) = charts::bucket_daily(&days(365, |_| 60.0), 50, &day_key_of_date(today()));
            assert_eq!(unit, BucketUnit::Month);
            assert_eq!(axis_label(&buckets[3], unit, 13), "Jan 26");
            assert_eq!(axis_label(&buckets[4], unit, 13), "Feb");
            assert_eq!(months[12].label, "Oct");
        });
    }

    #[test]
    fn daily_waits_for_the_plot_width_and_summarises() {
        with_locale(Locale::EN_US, || {
            let daily = days(30, |i| if i == 3 { 2460.0 } else if i % 2 == 0 { 600.0 } else { 0.0 });
            let s = summary(AnalyticsRange::D30, daily.clone());
            let mut v = view(Some(&s), AnalyticsRange::D30);
            v.plot_width = None;
            let d = build(&v);
            assert_eq!(d.daily.row_count(), 0);
            assert_eq!(d.daily_empty, "");
            assert_eq!(d.daily_peak, "Peak 41m");
            assert_eq!(d.daily_peak_accessible, "Busiest day, 41 minutes");
            assert_eq!(
                d.daily_summary,
                "Daily listening, last 30 days: 3 hours 11 minutes over 16 active days. Busiest day Mon, Sep 21 with 41 minutes."
            );
            assert_eq!(daily_summary(&days(3, |i| if i == 2 { 60.0 } else { 0.0 }), AnalyticsRange::All).split(':').next(), Some("Daily listening, all time"));
            assert!(daily_summary(&days(3, |i| if i == 2 { 60.0 } else { 0.0 }), AnalyticsRange::D7).contains("over 1 active day."));
        });
    }

    #[test]
    fn heatmap_cells_peak_and_now() {
        with_locale(Locale::EN_US, || {
            let mut s = summary(AnalyticsRange::D30, days(30, |_| 0.0));
            s.heatmap[5][21] = 660.0;
            s.heatmap[0][6] = 54.0;
            s.heatmap[6][1] = 660.0; // a later tie is not the peak
            let (cells, meta, spoken) = heat(&tz(), &s, NOW);
            assert_eq!(cells.len(), 168);
            assert_eq!(meta, "Peak Sat 21:00");
            assert_eq!(spoken, "Listening by weekday and hour. Busiest time: Saturdays, 21:00\u{2013}22:00, 11 minutes.");
            let peak = &cells[5 * 24 + 21];
            assert!(peak.peak && peak.level == 1.0);
            assert_eq!(peak.tooltip_title, "Sat \u{00B7} 21:00\u{2013}22:00");
            assert_eq!(peak.tooltip_value, "11m");
            assert_eq!(peak.accessible, "Saturday 21:00\u{2013}22:00: 11m");
            assert!(!cells[6 * 24 + 1].peak);
            let early = &cells[6];
            assert_eq!(early.accessible, "Monday 06:00\u{2013}07:00: 54s");
            assert_eq!(cells[0].tooltip_value, "0m");
            assert_eq!(cells[0].accessible, "Monday 00:00\u{2013}01:00: 0s");
            // Saturday 16:xx is now.
            let now: Vec<(i32, i32)> = cells.iter().filter(|c| c.now).map(|c| (c.day, c.hour)).collect();
            assert_eq!(now, vec![(5, 16)]);
            let d = build(&view(Some(&s), AnalyticsRange::D30));
            assert_eq!(d.heat_day_labels.iter().map(|s| s.to_string()).collect::<Vec<_>>(), ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]);
            assert_eq!(d.heat_hour_labels.row_count(), 24);
            assert_eq!(d.heat_hour_labels.row_data(9).unwrap(), "09");
        });
    }

    #[test]
    fn sources_top_five_with_shares_of_all_reads() {
        with_locale(Locale::EN_US, || {
            let mut s = summary(AnalyticsRange::D30, days(30, |_| 0.0));
            let counts = [
                (ReadSource::Hotkey, 26),
                (ReadSource::ContextChip, 30),
                (ReadSource::SelectionPopup, 18),
                (ReadSource::Clipboard, 11),
                (ReadSource::BrowserExtension, 16),
                (ReadSource::Manual, 6),
                (ReadSource::Cli, 0),
            ];
            s.sources = counts.iter().map(|(source, reads)| SourceCount { source: *source, reads: *reads }).collect();
            let (rows, spoken) = source_rows(&s);
            assert_eq!(rows.len(), 5);
            assert_eq!(rows[0].key, "context-chip");
            assert_eq!(rows[0].icon, "mouse-pointer-click");
            assert!(rows[0].first && !rows[1].first);
            assert_eq!((rows[0].value.as_str(), rows[0].percent.as_str()), ("30", "28%"));
            assert_eq!(rows[0].accessible, "Right-click chip, 30 reads, 28%");
            assert!((rows[0].fraction - 30.0 / 107.0).abs() < 1e-6);
            assert_eq!(rows[4].label, "Clipboard");
            assert_eq!(
                spoken,
                "Reads by source: Right-click chip 30 (28%), Hotkey 26 (24%), Selection popup 18 (17%), Browser extension 16 (15%), Clipboard 11 (10%)."
            );
        });
    }

    #[test]
    fn top_lists_by_listening_or_reads() {
        with_locale(Locale::EN_US, || {
            let mut seeds = Vec::new();
            let mut art = |seed: &str| {
                seeds.push(seed.to_string());
                Image::default()
            };
            let rows = vec![
                ("Chrome".to_string(), "Chrome".to_string(), 33, 7140.0),
                ("Outlook".to_string(), "Outlook".to_string(), 24, 5040.0),
                ("Word".to_string(), "Word".to_string(), 1, 0.0),
            ];
            let (out, spoken) = top_rows("Top apps by listening time", &rows, &mut art);
            assert_eq!(out.len(), 2, "rows without listening drop out in listening mode");
            assert_eq!((out[0].rank.as_str(), out[0].value.as_str(), out[0].sub.as_str()), ("01", "1h 59m", "33 reads"));
            assert_eq!(out[0].accessible, "Chrome, 1h 59m, 33 reads");
            assert!(out[0].first && out[0].fraction == 1.0);
            assert!((out[1].fraction - 5040.0 / 7140.0).abs() < 1e-6);
            assert_eq!(spoken, "Top apps by listening time: Chrome 1h 59m, Outlook 1h 24m.");
            assert_eq!(seeds, ["Chrome", "Outlook"]);
            let reads_only = vec![("v1".to_string(), "George".to_string(), 3, 0.0), ("v2".to_string(), "".to_string(), 1, 0.0)];
            let (out, spoken) = top_rows("Top voices by listening time", &reads_only, &mut no_art);
            assert_eq!((out[0].value.as_str(), out[0].sub.as_str()), ("3", "3 reads"));
            assert_eq!(out[0].accessible, "George, 3 reads");
            assert_eq!(out[1].label, "Unknown");
            assert_eq!(spoken, "Top voices by listening time: George 3 reads, Unknown 1 read.");
        });
    }

    #[test]
    fn top_lists_come_from_the_summary() {
        with_locale(Locale::EN_US, || {
            let mut s = summary(AnalyticsRange::D7, days(7, |_| 60.0));
            s.top_apps = vec![TopApp { name: "Chrome".into(), reads: 10, listened_sec: 2460.0 }];
            s.top_voices = vec![TopVoice { voice_id: "JBFq".into(), name: "George".into(), reads: 14, listened_sec: 3540.0 }];
            let mut seeds = Vec::new();
            let d = build_data(&tz(), &view(Some(&s), AnalyticsRange::D7), NOW, credits(), &mut |seed: &str| {
                seeds.push(seed.to_string());
                Image::default()
            });
            assert_eq!(d.top_apps.row_data(0).unwrap().key, "Chrome");
            assert_eq!(d.top_voices.row_data(0).unwrap().key, "JBFq");
            assert_eq!(d.top_voices.row_data(0).unwrap().value, "59m");
            assert_eq!(seeds, ["Chrome", "JBFq"]);
        });
    }

    #[test]
    fn controller_requests_and_stale_answers() {
        use crossbeam_channel::unbounded;
        let (tx, rx) = unbounded();
        let core = CoreHandle::new(tx);
        let mut c = AnalyticsController::new(AnalyticsRange::D90);
        c.active = true;
        c.query_now(&core);
        let first = match rx.try_recv() {
            Ok(Command::QueryAnalytics { req, range }) => {
                assert_eq!(range, AnalyticsRange::D90);
                req
            }
            other => panic!("unexpected {other:?}"),
        };
        // A newer query supersedes the first; the first answer is dropped.
        c.query_now(&core);
        let second = match rx.try_recv() {
            Ok(Command::QueryAnalytics { req, .. }) => req,
            other => panic!("unexpected {other:?}"),
        };
        let s = Arc::new(summary(AnalyticsRange::D90, days(90, |_| 0.0)));
        assert!(!c.on_analytics(first, &Ok(Arc::clone(&s))));
        assert!(c.view().loading);
        // A failure before anything loaded → error card; Retry spins until the answer.
        assert!(c.on_analytics(second, &Err("Error: disk on fire".into())));
        assert_eq!(c.error.as_deref(), Some("disk on fire"));
        assert!(c.view().summary.is_none());
        c.retrying = c.summary.is_none() && c.error.is_some();
        c.query_now(&core);
        let third = match rx.try_recv() {
            Ok(Command::QueryAnalytics { req, .. }) => req,
            other => panic!("unexpected {other:?}"),
        };
        assert!(c.view().retrying);
        assert!(c.on_analytics(third, &Ok(Arc::clone(&s))));
        assert!(!c.retrying && c.error.is_none() && c.summary.is_some());
        // Leaving drops answers in flight.
        c.query_now(&core);
        let fourth = match rx.try_recv() {
            Ok(Command::QueryAnalytics { req, .. }) => req,
            other => panic!("unexpected {other:?}"),
        };
        c.active = false;
        c.pending = None;
        assert!(!c.on_analytics(fourth, &Ok(s)));
    }

    #[test]
    fn days_active_spoken_value() {
        assert_eq!(days_active_accessible(24, 30), "24 of 30 days");
        assert_eq!(days_active_accessible(0, 30), "0 of 30 days");
        assert_eq!(days_active_accessible(1, 1), "1 of 1 day");
        assert_eq!(days_active_accessible(0, 0), "0 of 0 days");
    }

    #[test]
    fn reentry_after_a_failed_first_load_shows_the_skeleton() {
        let mut w = test_world::World::new();
        let mut c = AnalyticsController::new(AnalyticsRange::D30);
        let route = AppRoute::page(vox_core::app::AppPage::Analytics);
        c.on_enter(&mut w.cx(), &route, &NavIntent::None);
        let req = match w.sent().first() {
            Some(Command::QueryAnalytics { req, .. }) => *req,
            other => panic!("unexpected {other:?}"),
        };
        assert!(c.on_update(&mut w.cx(), &UiUpdate::Analytics { req, result: Err("Error: disk on fire".into()) }));
        assert_eq!(c.build(&mut w.cx()).state, LoadState::Error);
        c.on_leave(&mut w.cx());
        // A fresh entry: `{summary: null, loading: true, error: null}` → skeleton until the new answer.
        c.on_enter(&mut w.cx(), &route, &NavIntent::None);
        let d = c.build(&mut w.cx());
        assert_eq!(d.state, LoadState::Loading);
        assert_eq!(d.error, "");
        assert!(!d.retrying);
        let req = match w.sent().first() {
            Some(Command::QueryAnalytics { req, .. }) => *req,
            other => panic!("unexpected {other:?}"),
        };
        // Failing again brings the card back.
        assert!(c.on_update(&mut w.cx(), &UiUpdate::Analytics { req, result: Err("Error: still on fire".into()) }));
        let d = c.build(&mut w.cx());
        assert_eq!((d.state, d.error.as_str()), (LoadState::Error, "still on fire"));
    }

    #[test]
    fn history_debounce_fires_through_the_controller_and_dims() {
        let mut w = test_world::World::new();
        let mut c = AnalyticsController::new(AnalyticsRange::D7);
        let route = AppRoute::page(vox_core::app::AppPage::Analytics);
        c.on_enter(&mut w.cx(), &route, &NavIntent::None);
        let req = match w.sent().first() {
            Some(Command::QueryAnalytics { req, .. }) => *req,
            other => panic!("unexpected {other:?}"),
        };
        let s = Arc::new(summary(AnalyticsRange::D7, days(7, |_| 60.0)));
        assert!(c.on_update(&mut w.cx(), &UiUpdate::Analytics { req, result: Ok(s) }));
        assert!(!c.build(&mut w.cx()).dimmed);
        // Loaded: a history change only (re)starts the debounce.
        assert!(!c.on_update(&mut w.cx(), &UiUpdate::HistoryChanged));
        assert!(w.sent().is_empty());
        // What the timer runs through `AppUi::analytics_cb` (which then rebuilds the page).
        c.debounce_due(&mut w.cx());
        assert!(matches!(w.sent().as_slice(), [Command::QueryAnalytics { range: AnalyticsRange::D7, .. }]));
        let d = c.build(&mut w.cx());
        assert_eq!(d.state, LoadState::Ready);
        assert!(d.dimmed, "the previous summary stays, dimmed, while the query runs");
        // Inert once the page was left.
        c.on_leave(&mut w.cx());
        c.debounce_due(&mut w.cx());
        assert!(w.sent().is_empty());
    }

    #[test]
    fn controller_entry_range_credits_and_width() {
        let mut w = test_world::World::new();
        let mut c = AnalyticsController::new(AnalyticsRange::D30);
        c.on_enter(&mut w.cx(), &AppRoute::page(vox_core::app::AppPage::Analytics), &NavIntent::None);
        let sent = w.sent();
        assert!(matches!(sent[0], Command::QueryAnalytics { range: AnalyticsRange::D30, .. }));
        assert!(matches!(sent[1], Command::RefreshCredits { force: false }));
        assert_eq!(sent.len(), 2);
        // A range change persists and queries at once; the same value or an unknown one does nothing.
        c.set_range(&mut w.cx(), "90d");
        let sent = w.sent();
        assert!(matches!(sent[0], Command::SaveAnalyticsRange(AnalyticsRange::D90)));
        assert!(matches!(sent[1], Command::QueryAnalytics { range: AnalyticsRange::D90, .. }));
        c.set_range(&mut w.cx(), "90d");
        c.set_range(&mut w.cx(), "1y");
        assert!(w.sent().is_empty());
        assert_eq!(c.view().range, AnalyticsRange::D90);
        // Credits buttons.
        c.refresh_credits(&mut w.cx());
        assert!(matches!(w.sent().as_slice(), [Command::RefreshCredits { force: true }]));
        c.add_key(&mut w.cx());
        assert_eq!(w.nav.target, Some((AppRoute::settings("account"), NavIntent::None)));
        // Plot width.
        c.set_plot_width(&mut w.cx(), 731.0);
        assert_eq!(c.view().plot_width, Some(731.0));
        c.set_plot_width(&mut w.cx(), f32::NAN);
        assert_eq!(c.view().plot_width, None);
        // Day rollover re-queries; credits rebuild; history changes before the first load query at once.
        assert!(c.on_update(&mut w.cx(), &UiUpdate::Refresh(RefreshReason::Day)));
        assert!(matches!(w.sent().as_slice(), [Command::QueryAnalytics { range: AnalyticsRange::D90, .. }]));
        assert!(!c.on_update(&mut w.cx(), &UiUpdate::HistoryChanged));
        assert!(matches!(w.sent().as_slice(), [Command::QueryAnalytics { .. }]));
        let credits = UiUpdate::Credits(w.credits.clone());
        assert!(c.on_update(&mut w.cx(), &credits));
        c.on_wake(&mut w.cx());
        assert!(matches!(w.sent().as_slice(), [Command::QueryAnalytics { .. }]));
        // Leaving drops the request in flight and stops reacting.
        c.on_leave(&mut w.cx());
        assert!(!c.view().loading);
        c.on_wake(&mut w.cx());
        assert!(!c.on_update(&mut w.cx(), &UiUpdate::HistoryChanged));
        assert!(c.on_update(&mut w.cx(), &UiUpdate::Refresh(RefreshReason::Day)));
        assert!(w.sent().is_empty());
    }
}

/// A hand-built `UiCtx` for controller tests (ARCHITECTURE.md §10: controllers are unit-tested through the
/// `PageController` interface).
#[cfg(test)]
pub(crate) mod test_world {
    use std::sync::Arc;

    use crossbeam_channel::{unbounded, Receiver};
    use vox_core::app::AppInfo;
    use vox_core::player::PlayerSnapshot;
    use vox_core::settings::Settings;
    use vox_core::voices::TtsModel;

    use crate::command::{Command, CoreHandle};
    use crate::ui::art::ArtCache;
    use crate::ui::{DialogSink, FieldWrites, NavSink, ToastSink, UiCtx};
    use crate::update::{CreditsUpdate, CreditsView};

    pub(crate) struct World {
        pub core: CoreHandle,
        pub rx: Receiver<Command>,
        pub settings: Arc<Settings>,
        pub player: PlayerSnapshot,
        pub credits: CreditsUpdate,
        pub app_info: Arc<AppInfo>,
        pub models: Arc<Vec<TtsModel>>,
        pub art: ArtCache,
        pub toasts: ToastSink,
        pub nav: NavSink,
        pub dialogs: DialogSink,
        pub fields: FieldWrites,
    }

    impl World {
        pub(crate) fn new() -> Self {
            let (tx, rx) = unbounded();
            World {
                core: CoreHandle::new(tx),
                rx,
                settings: Arc::new(Settings { has_api_key: true, ..Settings::default() }),
                player: PlayerSnapshot::default(),
                credits: CreditsUpdate { view: CreditsView::NoKey, refreshing: false },
                app_info: Arc::new(AppInfo::default()),
                models: Arc::new(Vec::new()),
                art: ArtCache::default(),
                toasts: ToastSink::default(),
                nav: NavSink::default(),
                dialogs: DialogSink::default(),
                fields: FieldWrites::default(),
            }
        }

        pub(crate) fn cx(&mut self) -> UiCtx<'_> {
            UiCtx {
                core: &self.core,
                settings: &self.settings,
                player: &self.player,
                credits: &self.credits,
                app_info: &self.app_info,
                models: &self.models,
                now_ms: 1_792_254_600_000.0,
                demo: false,
                reduce_motion: false,
                art: &mut self.art,
                toasts: &mut self.toasts,
                nav: &mut self.nav,
                dialogs: &mut self.dialogs,
                fields: &mut self.fields,
            }
        }

        /// Every command sent since the last call.
        pub(crate) fn sent(&self) -> Vec<Command> {
            self.rx.try_iter().collect()
        }
    }
}
