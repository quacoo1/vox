//! Home controller (ui-main.md §3; DESIGN.md Home). Requests: `QueryHome` **immediately** on enter, Retry, day
//! rollover (`Refresh(Day)`) and wake; a **trailing 700 ms debounce** only for background triggers
//! (`UiUpdate::HistoryChanged`, player status / read changes; each restarts the timer); only the latest `req` applies
//! and leaving the page invalidates in-flight requests. Builds `HomeData`: greeting + date line (60 s tick),
//! now-playing (from the player snapshot; idle copy with the read-selection keycaps), Today vs the 15-minute goal,
//! credits (`convert::credits_data(.., Home, ..)`), "Last 7 days" (7 bars; the rolling window, labelled as such),
//! Quick read (word count, `quick-read-can-read = !js_trim(text).is_empty()`, busy flags; Ctrl+Enter), Recent (5 rows:
//! live → Pause / **paused → Resume** (`Command::Player(Play)`) / replay / failed / cached / favourite), Ways to read,
//! voice block, and every accessible string of `HomeData` (`voice-accessible`, the stat blocks' spoken values, the week
//! chart's description).
//! Rulings: a failed **first** load sets `state = error` with `Couldn’t load your reading stats` / the cleaned error
//! and a Retry (`home-retry` → `QueryHome` now; `retrying = true` until that request answers); a failure after a success
//! keeps the last data. A `ReadStarted` failure for Quick read / Read clipboard (also the core's panic answer) shows the
//! `read-failed` toast and ends the spinner.
//! Transport: the main button always sends `player-action(toggle)`; the controller maps it from the snapshot —
//! playing/paused → `Command::Player(Toggle)`; ended or error **with** a read id → `Command::Replay{req, read_id}` (+
//! the §1.6 failure toasts); otherwise nothing. `replay-last` replays `recent[0].id` and owns `replay-last-busy`; Recent
//! rows own their `busy` marker (a second row supersedes it). Seek keyboard: the design ProgressLine handles Left/Right
//! ±5 % and Home/End; `home.slint` wraps it in a FocusScope adding Up/Down ±2 % and PageUp/PageDown ±10 % (D-UM-31).
//! Tests: the Today caption at 600 s → `5m to your daily goal`, at 899.5 s → `1m to your daily goal`.
//!
//! Implementation notes:
//! - `read-failed`: the core already toasts every ordinary read failure (missing key, nothing to read, empty
//!   clipboard, engine errors), so Home toasts only the core's **panic answer** (`ReadResult::internal_error()`, the
//!   equivalent of the old IPC call throwing, ui-main.md §1.6) — a second toast for the same failure would be noise.
//! - The data of the last successful load is kept while the page is not shown, so re-entering Home does not flash
//!   skeletons (it still queries at once on entry).
//! - The background debounce runs on a `slint::Timer` that owns a clone of the `CoreHandle` and the shared "latest
//!   request" cell, so it needs no access to `AppUi` when it fires.
//! - `AppUi` must route `ReadStarted{req}` and `Home{req}` answers to this controller even when Home is not active.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use chrono::TimeZone;
use slint::{ModelRc, SharedString, VecModel};
use vox_core::analytics::AnalyticsSummary;
use vox_core::app::{AppPage, AppRoute, Toast};
use vox_core::constants::{DAILY_GOAL_SEC, PAGE_REFRESH_DEBOUNCE_MS, READ_WITH_LABEL};
use vox_core::format;
use vox_core::history::{ReadRecord, ReadSource};
use vox_core::js::{js_round, js_trim, number_to_string};
use vox_core::player::{PlayerCommand, PlayerSnapshot, PlayerState, PlayerStatus};
use vox_core::read::{ReadErrorCode, ReadResult, ReadTextOptions};
use vox_core::settings::Settings;
use vox_core::voices::TtsModel;

use super::art::ArtSize;
use super::convert::{credits_data, CreditsSurface};
use super::generated::{
    BarData, HomeData, KeyGroup, LoadState, MainAction, NowPlayingData, PageId, PlayerAction, RecentRow, StatBlockData,
    WayRow, WeekData,
};
use super::shell::{app_page, guarded, join_dot, phase, read_selection_hotkey, string_model};
use super::{NavIntent, PageController, UiCtx};
use crate::command::{Command, CoreHandle, RequestId};
use crate::update::{RefreshReason, UiUpdate};

/// Home page state.
#[derive(Default)]
pub struct HomeController {
    /// The latest `QueryHome` (shared with the debounce timer; an answer with another id is dropped).
    pending: Rc<Cell<Option<RequestId>>>,
    /// The trailing 700 ms debounce for background refreshes.
    debounce: slint::Timer,
    /// Home is the page on screen.
    active: bool,
    /// A load has succeeded at least once (afterwards background triggers are debounced).
    loaded: bool,
    summary: Option<Arc<AnalyticsSummary>>,
    recent: Option<Vec<ReadRecord>>,
    /// The cleaned error of a failed load (shown only while nothing has loaded yet).
    error: Option<String>,
    /// Retry after a failed first load is in flight.
    retrying: bool,
    // Quick read
    words: u64,
    can_read: bool,
    read_req: Option<RequestId>,
    clipboard_req: Option<RequestId>,
    // Replays: the Recent row with the spinner, "Replay last", and every replay request (→ its read id).
    replay_row: Option<(RequestId, String)>,
    replay_last: Option<RequestId>,
    replays: Vec<(RequestId, String)>,
    /// `(status, read id)` of the last player report (a change triggers a background refresh).
    last_player: Option<(PlayerStatus, Option<String>)>,
}

impl HomeController {
    pub fn quick_read(&mut self, cx: &mut UiCtx, text: &str) {
        if self.read_req.is_some() || self.clipboard_req.is_some() || js_trim(text).is_empty() {
            return;
        }
        let req = cx.core.request_id();
        self.read_req = Some(req);
        cx.core.send(Command::ReadText {
            req: Some(req),
            text: text.to_string(),
            options: ReadTextOptions { source: Some(ReadSource::Manual), ..ReadTextOptions::default() },
        });
    }
    pub fn quick_read_edited(&mut self, _cx: &mut UiCtx, text: &str) {
        self.words = vox_core::history::count_tokens(text);
        self.can_read = !js_trim(text).is_empty();
    }
    pub fn read_clipboard(&mut self, cx: &mut UiCtx) {
        if self.read_req.is_some() || self.clipboard_req.is_some() {
            return;
        }
        let req = cx.core.request_id();
        self.clipboard_req = Some(req);
        cx.core.send(Command::ReadClipboard { req: Some(req) });
    }
    pub fn player_action(&mut self, cx: &mut UiCtx, action: PlayerAction) {
        let state = &cx.player.state;
        let status = state.status;
        let live = matches!(status, PlayerStatus::Playing | PlayerStatus::Paused);
        match action {
            PlayerAction::Toggle => {
                if live {
                    cx.core.send(Command::Player(PlayerCommand::Toggle));
                } else if matches!(status, PlayerStatus::Ended | PlayerStatus::Error) {
                    if let Some(id) = state.read_id.clone().filter(|id| !id.is_empty()) {
                        let req = cx.core.request_id();
                        self.replays.push((req, id.clone()));
                        cx.core.send(Command::Replay { req: Some(req), read_id: id });
                    }
                }
            }
            PlayerAction::Play if status == PlayerStatus::Paused => cx.core.send(Command::Player(PlayerCommand::Play)),
            PlayerAction::Pause if status == PlayerStatus::Playing => cx.core.send(Command::Player(PlayerCommand::Pause)),
            PlayerAction::Stop | PlayerAction::Close if live || status == PlayerStatus::Loading => {
                cx.core.send(Command::Player(PlayerCommand::Stop))
            }
            PlayerAction::Previous if live => cx.core.send(Command::Player(PlayerCommand::previous_sentence())),
            PlayerAction::Next if live => cx.core.send(Command::Player(PlayerCommand::next_sentence())),
            PlayerAction::CycleRate => cx.core.send(Command::CycleRate),
            _ => {}
        }
    }
    /// Seek bar committed at a 0..1 fraction (sent as `Command::Player(SeekFraction)`; the core sanitizes).
    pub fn seek(&mut self, cx: &mut UiCtx, fraction: f32) {
        if !matches!(cx.player.state.status, PlayerStatus::Playing | PlayerStatus::Paused) || !fraction.is_finite() {
            return;
        }
        let fraction = vox_core::settings::quantize(fraction, 0.0001).clamp(0.0, 1.0);
        cx.core.send(Command::Player(PlayerCommand::SeekFraction { fraction }));
    }
    /// A Recent row's replay.
    pub fn replay(&mut self, cx: &mut UiCtx, read_id: &str) {
        if read_id.is_empty() {
            return;
        }
        let req = cx.core.request_id();
        self.replay_row = Some((req, read_id.to_string()));
        self.replays.push((req, read_id.to_string()));
        cx.core.send(Command::Replay { req: Some(req), read_id: read_id.to_string() });
    }
    /// "Replay last" (the newest read).
    pub fn replay_last(&mut self, cx: &mut UiCtx) {
        if self.replay_last.is_some() {
            return;
        }
        let Some(id) = self.recent.as_ref().and_then(|r| r.first()).map(|r| r.id.clone()) else { return };
        let req = cx.core.request_id();
        self.replay_last = Some(req);
        self.replays.push((req, id.clone()));
        cx.core.send(Command::Replay { req: Some(req), read_id: id });
    }
    pub fn pause_live(&mut self, cx: &mut UiCtx) {
        cx.core.send(Command::Player(PlayerCommand::Pause));
    }
    pub fn resume_live(&mut self, cx: &mut UiCtx) {
        cx.core.send(Command::Player(PlayerCommand::Play));
    }
    pub fn retry(&mut self, cx: &mut UiCtx) {
        self.retrying = self.error.is_some() && !self.loaded;
        self.query_now(cx.core);
    }
    pub fn credits_retry(&mut self, cx: &mut UiCtx) {
        cx.core.send(Command::RefreshCredits { force: true });
    }
    pub fn open_page(&mut self, cx: &mut UiCtx, page: PageId) {
        cx.nav.navigate(AppRoute::page(app_page(page)));
    }
    pub fn open_settings(&mut self, cx: &mut UiCtx, section: &str) {
        cx.nav.navigate(AppRoute::validated(AppPage::Settings, Some(section)));
    }

    // ── requests ──

    /// `QueryHome` now (cancels a pending debounce; the new id is the only one that applies).
    fn query_now(&mut self, core: &CoreHandle) {
        self.debounce.stop();
        send_query(core, &self.pending);
    }
    /// A background trigger: at once until something has loaded, else (re)start the 700 ms trailing debounce.
    fn schedule(&mut self, core: &CoreHandle) {
        if !self.active {
            return;
        }
        if !self.loaded {
            self.query_now(core);
            return;
        }
        let core = core.clone();
        let pending = Rc::clone(&self.pending);
        self.debounce.start(slint::TimerMode::SingleShot, Duration::from_millis(PAGE_REFRESH_DEBOUNCE_MS), move || {
            guarded("home refresh debounce", || send_query(&core, &pending));
        });
    }
    fn on_home(&mut self, req: RequestId, result: &Result<(Arc<AnalyticsSummary>, Vec<ReadRecord>), String>) -> bool {
        if self.pending.get() != Some(req) {
            return false;
        }
        self.pending.set(None);
        self.retrying = false;
        match result {
            Ok((summary, recent)) => {
                self.summary = Some(Arc::clone(summary));
                self.recent = Some(recent.iter().take(5).cloned().collect());
                self.loaded = true;
                self.error = None;
            }
            Err(e) => {
                tracing::warn!(target: "ui", "home refresh failed: {e}");
                if !self.loaded {
                    self.error = Some(format::clean_error(e));
                }
            }
        }
        true
    }
    /// A `ReadStarted` answer: ends the spinner it belongs to and toasts the failures the core stays silent about.
    fn on_read_started(&mut self, cx: &mut UiCtx, req: RequestId, result: &ReadResult) -> bool {
        if self.read_req == Some(req) || self.clipboard_req == Some(req) {
            if self.read_req == Some(req) {
                self.read_req = None;
            } else {
                self.clipboard_req = None;
            }
            if *result == ReadResult::internal_error() {
                let msg = result.error.clone().filter(|e| !e.is_empty()).unwrap_or_else(|| vox_core::copy::SOMETHING_WENT_WRONG.to_string());
                cx.toasts.push(Toast::error(msg).with_id("read-failed"), None);
            }
            return true;
        }
        let Some(i) = self.replays.iter().position(|(r, _)| *r == req) else { return false };
        let (_, read_id) = self.replays.remove(i);
        if self.replay_row.as_ref().is_some_and(|(r, _)| *r == req) {
            self.replay_row = None;
        }
        if self.replay_last == Some(req) {
            self.replay_last = None;
        }
        if let Some(toast) = replay_failure_toast(&read_id, result) {
            cx.toasts.push(toast, None);
        }
        true
    }
}

/// Sends a fresh `QueryHome` and makes it the only answer that applies.
fn send_query(core: &CoreHandle, pending: &Cell<Option<RequestId>>) {
    let req = core.request_id();
    pending.set(Some(req));
    core.send(Command::QueryHome { req });
}

/// `replayRead`'s extra failure toast (ui-main.md §1.6): `not-cached`, or `unknown` without a read id (the record
/// vanished, or the core's panic answer) → error toast `res.error ?? "That read can’t be replayed"`, id
/// `replay-{readId}`. A failed re-synthesis carries its new read id and the core toasts it.
pub(crate) fn replay_failure_toast(read_id: &str, result: &ReadResult) -> Option<Toast> {
    if result.ok {
        return None;
    }
    let silent = match result.code {
        Some(ReadErrorCode::NotCached) => true,
        Some(ReadErrorCode::Unknown) => result.read_id.is_none(),
        _ => false,
    };
    silent.then(|| {
        let msg = result.error.clone().filter(|e| !e.is_empty()).unwrap_or_else(|| "That read can\u{2019}t be replayed".to_string());
        Toast::error(msg).with_id(format!("replay-{read_id}"))
    })
}

impl PageController for HomeController {
    type Data = HomeData;

    fn on_enter(&mut self, cx: &mut UiCtx, _route: &AppRoute, _intent: &NavIntent) {
        self.active = true;
        // The page is re-created: the Quick read box starts empty.
        self.words = 0;
        self.can_read = false;
        self.last_player = Some((cx.player.state.status, cx.player.state.read_id.clone()));
        self.query_now(cx.core);
    }

    fn on_leave(&mut self, _cx: &mut UiCtx) {
        self.active = false;
        self.debounce.stop();
        // In-flight answers are dropped.
        self.pending.set(None);
        self.retrying = false;
    }

    fn on_update(&mut self, cx: &mut UiCtx, update: &UiUpdate) -> bool {
        match update {
            UiUpdate::Home { req, result } => self.on_home(*req, result),
            UiUpdate::ReadStarted { req, result } => self.on_read_started(cx, *req, result),
            UiUpdate::HistoryChanged => {
                self.schedule(cx.core);
                false
            }
            UiUpdate::Player(snapshot) => {
                let key = (snapshot.state.status, snapshot.state.read_id.clone());
                if self.last_player.as_ref() != Some(&key) {
                    self.last_player = Some(key);
                    self.schedule(cx.core);
                }
                self.active
            }
            UiUpdate::Refresh(reason) => {
                if *reason == RefreshReason::Day && self.active {
                    self.query_now(cx.core);
                }
                true
            }
            UiUpdate::Settings(_) | UiUpdate::Credits(_) | UiUpdate::Models { .. } | UiUpdate::AppInfo(_) => self.active,
            _ => false,
        }
    }

    fn on_wake(&mut self, cx: &mut UiCtx) {
        if self.active {
            self.query_now(cx.core);
        }
    }

    fn build(&self, cx: &mut UiCtx) -> HomeData {
        let settings: &Settings = cx.settings;
        let now = chrono::Local.timestamp_millis_opt(cx.now_ms as i64).earliest().unwrap_or_else(chrono::Local::now);
        let state = &cx.player.state;

        // Now playing: the session's voice art, else the configured voice's.
        let voice_id = cx.player.session.as_ref().map(|s| s.voice_id.clone()).unwrap_or_else(|| settings.voice_id.clone());
        let now_art = cx.art.image(&voice_id, ArtSize::Large);
        let last = self.recent.as_ref().and_then(|r| r.first());
        let now_playing = now_playing(cx.player, settings, last, self.replay_last.is_some(), now_art);

        let recent: Vec<RecentRow> = match &self.recent {
            Some(rows) => rows
                .iter()
                .map(|r| {
                    let art = cx.art.image(&r.voice_id, ArtSize::Compact);
                    let busy = self.replay_row.as_ref().is_some_and(|(_, id)| *id == r.id);
                    recent_row(r, state, &chrono::Local, cx.now_ms, busy, art)
                })
                .collect(),
            None => Vec::new(),
        };

        let first_loading = self.summary.is_none() && self.error.is_none();
        let failed = self.summary.is_none() && self.error.is_some();
        let voice = voice_block(settings, cx.models);
        HomeData {
            state: if self.summary.is_some() {
                LoadState::Ready
            } else if failed {
                LoadState::Error
            } else {
                LoadState::Loading
            },
            error_title: if failed { "Couldn\u{2019}t load your reading stats".into() } else { SharedString::new() },
            error_body: self.error.as_deref().filter(|_| failed).unwrap_or("").into(),
            retrying: self.retrying,
            greeting: format::greeting(&now).into(),
            date_line: format::date_line(&now).into(),
            now: now_playing,
            today: today_block(self.summary.as_deref()),
            credits: credits_data(cx.credits, CreditsSurface::Home, cx.demo, cx.now_ms),
            week: week_block(self.summary.as_deref()),
            quick_read_words: words_label(self.words).into(),
            quick_read_can_read: self.can_read,
            read_busy: self.read_req.is_some(),
            clipboard_busy: self.clipboard_req.is_some(),
            recent_loading: self.recent.is_none() && !failed,
            recent: ModelRc::new(VecModel::from(recent)),
            ways: ways(settings),
            voice_name: voice.name.into(),
            voice_meta: voice.meta.into(),
            voice_art: cx.art.image(&settings.voice_id, ArtSize::Compact),
            voice_accessible: voice.accessible.into(),
        }
        .with_first_loading(first_loading)
    }
}

/// Small builder helper so `build` reads top-down.
trait WithFirstLoading {
    fn with_first_loading(self, loading: bool) -> Self;
}

impl WithFirstLoading for HomeData {
    fn with_first_loading(mut self, loading: bool) -> Self {
        if loading {
            self.recent_loading = true;
        }
        self
    }
}

// ───────────────────────────── pure builders ─────────────────────────────

/// The live word count label (`{n} word|words`, empty for 0).
pub(crate) fn words_label(n: u64) -> String {
    if n == 0 {
        String::new()
    } else {
        format::count_label(n, "word", "words")
    }
}

/// The Now playing card (ui-main.md §3.4).
pub(crate) fn now_playing(snapshot: &PlayerSnapshot, settings: &Settings, last: Option<&ReadRecord>, replay_last_busy: bool, art: slint::Image) -> NowPlayingData {
    let state = &snapshot.state;
    let status = state.status;
    let idle = status == PlayerStatus::Idle || (status == PlayerStatus::Ended && state.title.is_empty());
    let ended = status == PlayerStatus::Ended;
    let live = matches!(status, PlayerStatus::Playing | PlayerStatus::Paused);
    let has_duration = state.duration_sec > 0.0;

    let meta = match status {
        PlayerStatus::Error => state.error.clone().filter(|e| !e.is_empty()).unwrap_or_else(|| "Playback stopped.".into()),
        PlayerStatus::Loading => "Preparing audio\u{2026}".into(),
        _ => join_dot([state.voice_name.as_str(), state.source_app.as_deref().unwrap_or("")]),
    };
    let mut badges: Vec<String> = Vec::new();
    if let Some(app) = state.source_app.as_deref().filter(|a| !a.is_empty()) {
        badges.push(app.to_string());
    }
    match status {
        PlayerStatus::Playing => badges.push("Live".into()),
        PlayerStatus::Paused => badges.push("Paused".into()),
        PlayerStatus::Ended => badges.push("Finished".into()),
        _ => {}
    }
    let (time, time_accessible) = if !has_duration {
        (String::new(), String::new())
    } else if ended {
        let clock = format::format_clock(state.duration_sec);
        (clock.clone(), format!("{clock} total"))
    } else {
        (format::remaining_clock(state.position_sec, state.duration_sec, state.duration_is_estimate), super::shell::time_left_spoken(state))
    };
    let progress = if ended {
        1.0
    } else if has_duration {
        (state.position_sec / state.duration_sec).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let main_action = match status {
        PlayerStatus::Playing => MainAction::Pause,
        PlayerStatus::Ended => MainAction::PlayAgain,
        PlayerStatus::Error => MainAction::TryAgain,
        _ => MainAction::Play,
    };
    let accel = read_selection_hotkey(settings);
    let keys = vox_core::hotkey::display_labels(accel);
    NowPlayingData {
        idle,
        art,
        title: if state.title.is_empty() { "Untitled selection".into() } else { state.title.as_str().into() },
        meta: meta.into(),
        meta_is_error: status == PlayerStatus::Error,
        badges: string_model(badges),
        phase: phase(status),
        time: time.into(),
        time_estimate: has_duration && !ended && state.duration_is_estimate,
        time_accessible: time_accessible.into(),
        progress: progress as f32,
        buffered: state.buffered_fraction.clamp(0.0, 1.0) as f32,
        seekable: live,
        can_stop: live || status == PlayerStatus::Loading,
        can_previous: live,
        can_next: live,
        main_action,
        main_enabled: status != PlayerStatus::Loading,
        title_tooltip: state.title.as_str().into(),
        idle_title: "Select text, then right-click".into(),
        idle_sub: if keys.is_empty() {
            format!("Choose \u{201C}{READ_WITH_LABEL}\u{201D}").into()
        } else {
            format!("Choose \u{201C}{READ_WITH_LABEL}\u{201D} or press").into()
        },
        idle_keys_spoken: if keys.is_empty() { SharedString::new() } else { vox_core::hotkey::spoken(accel).into() },
        idle_keys: string_model(keys),
        replay_last_visible: idle && last.is_some(),
        replay_last_tooltip: last.map(|r| format::truncate(&r.text, 120)).unwrap_or_default().into(),
        replay_last_busy,
    }
}

/// `{max(1, ceil((900 − listened) / 60))}m to your daily goal`, or `Daily goal reached`.
pub(crate) fn goal_caption(listened_sec: f64) -> String {
    if listened_sec >= DAILY_GOAL_SEC {
        "Daily goal reached".into()
    } else {
        let mins = ((DAILY_GOAL_SEC - listened_sec) / 60.0).ceil().max(1.0) as u64;
        format!("{mins}m to your daily goal")
    }
}

/// The Today block (ui-main.md §3.5).
pub(crate) fn today_block(summary: Option<&AnalyticsSummary>) -> StatBlockData {
    let Some(s) = summary else {
        return StatBlockData { loading: true, label: "Today".into(), badge: "Goal 15m".into(), ..StatBlockData::default() };
    };
    let listened = s.today.listened_sec.max(0.0);
    let share = (listened / DAILY_GOAL_SEC).min(1.0);
    let done = listened >= DAILY_GOAL_SEC;
    let reads = s.today.reads;
    let words = s.today.words;
    StatBlockData {
        loading: false,
        label: "Today".into(),
        badge: "Goal 15m".into(),
        value: format::format_duration(listened).into(),
        value_accessible: format!("{} listened today", format::duration_speech(listened)).into(),
        caption: format!(
            "{} {} \u{00B7} {} {}",
            format::format_number(reads as f64),
            if reads == 1 { "read" } else { "reads" },
            format::format_compact(words as f64),
            if words == 1 { "word" } else { "words" }
        )
        .into(),
        sub: goal_caption(listened).into(),
        fraction: share as f32,
        accessible_value: if done {
            format!("Goal reached: {} listened", format::duration_speech(listened)).into()
        } else {
            format!("{}% of the daily goal", js_round(share * 100.0) as i64).into()
        },
        done,
    }
}

/// The "Last 7 days" block (ui-main.md §3.8; R-Analytics "rolling, labelled as such").
pub(crate) fn week_block(summary: Option<&AnalyticsSummary>) -> WeekData {
    let Some(s) = summary else {
        return WeekData { loading: true, title: "Last 7 days".into(), ..WeekData::default() };
    };
    let days = &s.daily[s.daily.len().saturating_sub(7)..];
    let top = days.iter().map(|d| d.listened_sec).fold(0.0_f64, f64::max);
    let total: f64 = days.iter().map(|d| d.listened_sec.max(0.0)).sum();
    let last = days.len().saturating_sub(1);
    let bars: Vec<BarData> = days
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let v = d.listened_sec.max(0.0);
            let name = format::weekday_name(&d.date);
            BarData {
                level: if top > 0.0 { (v / top).clamp(0.0, 1.0) as f32 } else { 0.0 },
                label: format::weekday_initial(&d.date).into(),
                today: i == last,
                zero: v <= 0.0,
                tooltip_title: name.as_str().into(),
                tooltip_value: format::format_duration(v).into(),
                tooltip_sub: SharedString::new(),
                accessible: format!("{name}: {}", format::duration_speech(v)).into(),
            }
        })
        .collect();
    let description = days
        .iter()
        .map(|d| format!("{}: {}", format::weekday_name(&d.date), format::duration_speech(d.listened_sec.max(0.0))))
        .collect::<Vec<_>>()
        .join(", ");
    WeekData {
        loading: false,
        title: "Last 7 days".into(),
        total: format::format_duration(total).into(),
        bars: ModelRc::new(VecModel::from(bars)),
        streak: format!("{} day streak", s.streak.current).into(),
        best: format!("best {}", s.streak.longest).into(),
        streak_active: s.streak.current > 0,
        accessible_description: format!("Listening per day. {description}").into(),
    }
}

/// One Recent row (ui-main.md §3.10; Resume for the paused live read, R-Home).
pub(crate) fn recent_row<Tz: TimeZone>(r: &ReadRecord, state: &PlayerState, tz: &Tz, now_ms: f64, busy: bool, art: slint::Image) -> RecentRow {
    let in_player = state.read_id.as_deref() == Some(r.id.as_str());
    let live = in_player && state.status == PlayerStatus::Playing;
    let resumable = in_player && state.status == PlayerStatus::Paused;
    let failed = r.error.is_some() && r.audio_duration_sec <= 0.0 && r.listened_sec <= 0.0;
    let duration = if r.audio_duration_sec > 0.0 { r.audio_duration_sec } else { r.listened_sec };
    let relative = format::format_relative(tz, r.created_at, now_ms);
    let label = if live {
        "Pause".to_string()
    } else if resumable {
        format!("Resume: {}", format::truncate(&r.text, 60))
    } else {
        format!("Replay: {}", format::truncate(&r.text, 60))
    };
    RecentRow {
        id: r.id.as_str().into(),
        art,
        title: format::truncate(&r.text, 140).into(),
        meta: join_dot([r.source_app.as_deref().unwrap_or(""), r.voice_name.as_str(), relative.as_str()]).into(),
        duration: format::format_clock(duration).into(),
        failed,
        failed_tooltip: if failed { r.error.as_deref().unwrap_or("").into() } else { SharedString::new() },
        cached: r.cached,
        favorite: r.favorite,
        live,
        resumable,
        busy,
        accessible_label: label.into(),
    }
}

/// The Ways to read rows (ui-main.md §3.11).
pub(crate) fn ways(settings: &Settings) -> ModelRc<WayRow> {
    let t = &settings.triggers;
    let group = |label: &str, accel: &str| KeyGroup { label: label.into(), keys: string_model(vox_core::hotkey::display_labels(accel)) };
    let empty_groups = || ModelRc::new(VecModel::from(Vec::<KeyGroup>::new()));
    let rows = vec![
        WayRow {
            title: "Right-click chip".into(),
            body: format!("Select text, right-click, then pick \u{201C}{READ_WITH_LABEL}\u{201D} beside the menu.").into(),
            key_groups: empty_groups(),
            icon: "mouse-pointer-click".into(),
            section: "triggers".into(),
            off: !t.right_click_chip,
        },
        WayRow {
            title: "Hotkeys".into(),
            body: if t.hotkeys_enabled { SharedString::new() } else { "Global shortcuts are turned off.".into() },
            key_groups: if t.hotkeys_enabled {
                ModelRc::new(VecModel::from(vec![group("Selection", &t.hotkeys.read_selection), group("Clipboard", &t.hotkeys.read_clipboard)]))
            } else {
                empty_groups()
            },
            icon: "keyboard".into(),
            section: "triggers".into(),
            off: !t.hotkeys_enabled,
        },
        WayRow {
            title: "Browser extension".into(),
            body: format!("Adds \u{201C}{READ_WITH_LABEL}\u{201D} to the Chrome and Edge right-click menu.").into(),
            key_groups: empty_groups(),
            icon: "globe".into(),
            section: "integrations".into(),
            off: !t.browser_extension,
        },
    ];
    ModelRc::new(VecModel::from(rows))
}

/// The voice block's strings (ui-main.md §3.9).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct VoiceBlock {
    pub name: String,
    pub meta: String,
    pub accessible: String,
}

/// `Flash v2.5 · 1×`; accessible `Voice: {name}. {model}, speed {round(rate·100)/100} times. Change voice`.
pub(crate) fn voice_block(settings: &Settings, models: &[TtsModel]) -> VoiceBlock {
    let name = if settings.voice_name.is_empty() { "No voice".to_string() } else { settings.voice_name.clone() };
    let model = format::model_name(&settings.model_id, models);
    let rate = if settings.playback_rate.is_finite() && settings.playback_rate > 0.0 { settings.playback_rate } else { 1.0 };
    let spoken_rate = number_to_string(js_round(rate * 100.0) / 100.0);
    VoiceBlock {
        meta: join_dot([model.as_str(), format::rate_label(rate).as_str()]),
        accessible: format!("Voice: {name}. {model}, speed {spoken_rate} times. Change voice"),
        name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use vox_core::analytics::{AnalyticsRange, DailyEntry, Streak, TodayStats};
    use vox_core::player::SessionInfo;

    fn summary(today_sec: f64, daily: &[(&str, f64)]) -> AnalyticsSummary {
        let mut s = AnalyticsSummary::empty(AnalyticsRange::D7, 0.0);
        s.today = TodayStats { listened_sec: today_sec, reads: 5, words: 1_900 };
        s.daily = daily.iter().map(|(d, sec)| DailyEntry { date: d.to_string(), listened_sec: *sec, reads: 1, words: 10 }).collect();
        s.streak = Streak { current: 3, longest: 6 };
        s
    }

    #[test]
    fn today_caption_vectors() {
        assert_eq!(goal_caption(600.0), "5m to your daily goal");
        assert_eq!(goal_caption(899.5), "1m to your daily goal");
        assert_eq!(goal_caption(0.0), "15m to your daily goal");
        assert_eq!(goal_caption(900.0), "Daily goal reached");
    }

    #[test]
    fn today_block_values() {
        let s = summary(840.0, &[]);
        let t = today_block(Some(&s));
        assert!(!t.loading && !t.done);
        assert_eq!(t.value, "14m");
        assert_eq!(t.value_accessible, "14 minutes listened today");
        assert_eq!(t.caption, "5 reads \u{b7} 1.9K words");
        assert_eq!(t.sub, "1m to your daily goal");
        assert_eq!(t.accessible_value, "93% of the daily goal");
        let t = today_block(Some(&summary(1_200.0, &[])));
        assert!(t.done);
        assert_eq!(t.fraction, 1.0);
        assert_eq!(t.accessible_value, "Goal reached: 20 minutes listened");
        assert!(today_block(None).loading);
    }

    #[test]
    fn week_block_values() {
        let days = [
            ("2026-10-10", 540.0),
            ("2026-10-11", 0.0),
            ("2026-10-12", 960.0),
            ("2026-10-13", 1560.0),
            ("2026-10-14", 360.0),
            ("2026-10-15", 720.0),
            ("2026-10-16", 840.0),
        ];
        let w = week_block(Some(&summary(840.0, &days)));
        assert_eq!(w.title, "Last 7 days");
        assert_eq!(w.total, "1h 23m");
        assert_eq!(w.streak, "3 day streak");
        assert_eq!(w.best, "best 6");
        assert!(w.streak_active);
        use slint::Model;
        assert_eq!(w.bars.row_count(), 7);
        let b0 = w.bars.row_data(0).expect("bar");
        assert_eq!(b0.label, "S");
        assert!((b0.level - 540.0 / 1560.0).abs() < 1e-6);
        assert_eq!(b0.accessible, "Saturday: 9 minutes");
        let b1 = w.bars.row_data(1).expect("bar");
        assert!(b1.zero && b1.level == 0.0);
        assert_eq!(b1.tooltip_value, "0s");
        assert!(w.bars.row_data(6).expect("bar").today);
        assert!(w.accessible_description.starts_with("Listening per day. Saturday: 9 minutes, Sunday: 0 seconds"));
        assert!(week_block(None).loading);
    }

    fn record(id: &str) -> ReadRecord {
        let mut r = ReadRecord::new(id.into(), 1_000_000.0, ReadSource::ContextChip);
        r.text = "Quarterly planning notes: we move the launch review to Thursday.".into();
        r.voice_name = "George".into();
        r.source_app = Some("Outlook".into());
        r.audio_duration_sec = 42.0;
        r
    }

    #[test]
    fn recent_rows() {
        let r = record("r1");
        let mut st = PlayerState { status: PlayerStatus::Playing, read_id: Some("r1".into()), ..PlayerState::idle() };
        let now = 1_000_000.0 + 600_000.0;
        let row = recent_row(&r, &st, &Utc, now, false, slint::Image::default());
        assert!(row.live && !row.resumable);
        assert_eq!(row.accessible_label, "Pause");
        assert_eq!(row.meta, "Outlook \u{b7} George \u{b7} 10 minutes ago");
        assert_eq!(row.duration, "0:42");
        st.status = PlayerStatus::Paused;
        let row = recent_row(&r, &st, &Utc, now, true, slint::Image::default());
        assert!(row.resumable && !row.live && row.busy);
        assert!(row.accessible_label.starts_with("Resume: Quarterly planning"));
        st.read_id = Some("other".into());
        let row = recent_row(&r, &st, &Utc, now, false, slint::Image::default());
        assert!(!row.resumable && !row.live);
        assert!(row.accessible_label.starts_with("Replay: "));
        assert!(row.accessible_label.chars().count() <= "Replay: ".len() + 60);
        // Failed only without audio and listening.
        let mut f = record("f");
        f.error = Some("Out of credits".into());
        assert!(!recent_row(&f, &st, &Utc, now, false, slint::Image::default()).failed);
        f.audio_duration_sec = 0.0;
        let row = recent_row(&f, &st, &Utc, now, false, slint::Image::default());
        assert!(row.failed);
        assert_eq!(row.failed_tooltip, "Out of credits");
        assert_eq!(row.duration, "0:00");
    }

    #[test]
    fn now_playing_card() {
        let settings = Settings::default();
        let mut snap = PlayerSnapshot::default();
        let n = now_playing(&snap, &settings, None, false, slint::Image::default());
        assert!(n.idle && !n.replay_last_visible);
        assert_eq!(n.idle_sub, "Choose \u{201C}Read with Vox\u{201D} or press");
        assert_eq!(n.idle_keys_spoken, "Ctrl plus Alt plus R");
        let last = record("r1");
        let n = now_playing(&snap, &settings, Some(&last), true, slint::Image::default());
        assert!(n.replay_last_visible && n.replay_last_busy);
        assert_eq!(n.replay_last_tooltip, format::truncate(&last.text, 120));

        snap.state = PlayerState {
            status: PlayerStatus::Playing,
            read_id: Some("r1".into()),
            title: "The quiet revolution".into(),
            voice_name: "George".into(),
            source_app: Some("Outlook".into()),
            position_sec: 63.0,
            duration_sec: 263.0,
            duration_is_estimate: false,
            buffered_fraction: 0.64,
            ..PlayerState::idle()
        };
        snap.session = Some(std::sync::Arc::new(SessionInfo {
            id: "s".into(),
            read_id: "r1".into(),
            text: "The quiet revolution".into(),
            title: "The quiet revolution".into(),
            voice_name: "George".into(),
            voice_id: "v".into(),
            source_app: Some("Outlook".into()),
            from_cache: false,
            highlight_words: true,
            sentences: vec![0].into(),
        }));
        let n = now_playing(&snap, &settings, Some(&last), false, slint::Image::default());
        assert!(!n.idle && n.seekable && n.can_stop && n.can_next && n.main_enabled);
        assert_eq!(n.main_action, MainAction::Pause);
        assert_eq!(n.meta, "George \u{b7} Outlook");
        assert_eq!(n.time, "-3:20");
        assert_eq!(n.time_accessible, "3:20 left");
        assert!(!n.replay_last_visible);
        use slint::Model;
        assert_eq!(n.badges.row_count(), 2);
        assert_eq!(n.badges.row_data(1).as_deref(), Some("Live"));

        snap.state.status = PlayerStatus::Ended;
        let n = now_playing(&snap, &settings, None, false, slint::Image::default());
        assert_eq!(n.main_action, MainAction::PlayAgain);
        assert_eq!((n.time.as_str(), n.time_accessible.as_str(), n.progress), ("4:23", "4:23 total", 1.0));
        snap.state.status = PlayerStatus::Loading;
        let n = now_playing(&snap, &settings, None, false, slint::Image::default());
        assert!(!n.main_enabled && n.can_stop && !n.seekable);
        assert_eq!(n.meta, "Preparing audio\u{2026}");
        snap.state.status = PlayerStatus::Error;
        snap.state.error = Some("Couldn’t reach ElevenLabs".into());
        let n = now_playing(&snap, &settings, None, false, slint::Image::default());
        assert!(n.meta_is_error);
        assert_eq!(n.main_action, MainAction::TryAgain);
        assert_eq!(n.meta, "Couldn’t reach ElevenLabs");
    }

    #[test]
    fn ways_and_voice() {
        let mut settings = Settings::default();
        use slint::Model;
        let w = ways(&settings);
        assert_eq!(w.row_count(), 3);
        let hk = w.row_data(1).expect("row");
        assert!(!hk.off && hk.body.is_empty());
        assert_eq!(hk.key_groups.row_count(), 2);
        settings.triggers.hotkeys_enabled = false;
        settings.triggers.browser_extension = false;
        let w = ways(&settings);
        assert_eq!(w.row_data(1).expect("row").body, "Global shortcuts are turned off.");
        assert!(w.row_data(2).expect("row").off);

        settings.voice_name = "Charlotte".into();
        settings.model_id = "eleven_multilingual_v2".into();
        settings.playback_rate = 1.25;
        let v = voice_block(&settings, &[]);
        assert_eq!(v.meta, "Multilingual v2 \u{b7} 1.25\u{d7}");
        assert_eq!(v.accessible, "Voice: Charlotte. Multilingual v2, speed 1.25 times. Change voice");
        settings.voice_name.clear();
        settings.playback_rate = 1.0;
        assert_eq!(voice_block(&settings, &[]).accessible, "Voice: No voice. Multilingual v2, speed 1 times. Change voice");
    }

    #[test]
    fn replay_failure_toasts() {
        assert_eq!(replay_failure_toast("a", &ReadResult::started("a")), None);
        let t = replay_failure_toast("a", &ReadResult::not_cached("a")).expect("toast");
        assert_eq!(t.id, "replay-a");
        assert_eq!(t.message, "The audio is no longer cached and the full text was not saved");
        assert!(replay_failure_toast("a", &ReadResult::not_in_history()).is_some());
        // A failed re-synthesis carries its new read id: the core toasts it.
        assert_eq!(replay_failure_toast("a", &ReadResult::failed_with_id("b", ReadErrorCode::Unknown, "x")), None);
        assert_eq!(replay_failure_toast("a", &ReadResult::no_api_key()), None);
        let t = replay_failure_toast("a", &ReadResult::failed(ReadErrorCode::NotCached, "")).expect("toast");
        assert_eq!(t.message, "That read can\u{2019}t be replayed");
    }

    #[test]
    fn words() {
        assert_eq!(words_label(0), "");
        assert_eq!(words_label(1), "1 word");
        assert_eq!(words_label(1204), "1,204 words");
        assert_eq!(vox_core::history::count_tokens("  a  b\nc "), 3);
    }

    // ── the controller (states and requests) ──

    use super::super::art::ArtCache;
    use super::super::{DialogSink, FieldWrites, NavSink, ToastSink};
    use crate::update::{CreditsUpdate, CreditsView};
    use crossbeam_channel::{unbounded, Receiver};
    use vox_core::app::AppInfo;

    const NOW: f64 = 1_792_243_200_000.0;

    /// Everything a `UiCtx` borrows.
    struct World {
        core: CoreHandle,
        rx: Receiver<Command>,
        settings: Arc<Settings>,
        player: PlayerSnapshot,
        credits: CreditsUpdate,
        app_info: Arc<AppInfo>,
        models: Arc<Vec<TtsModel>>,
        art: ArtCache,
        toasts: ToastSink,
        nav: NavSink,
        dialogs: DialogSink,
        fields: FieldWrites,
    }

    impl World {
        fn new() -> Self {
            let (tx, rx) = unbounded();
            World {
                core: CoreHandle::new(tx),
                rx,
                settings: Arc::new(Settings { has_api_key: true, onboarded: true, ..Settings::default() }),
                player: PlayerSnapshot::default(),
                credits: CreditsUpdate { view: CreditsView::Loading, refreshing: false },
                app_info: Arc::new(AppInfo::default()),
                models: Arc::new(Vec::new()),
                art: ArtCache::default(),
                toasts: ToastSink::default(),
                nav: NavSink::default(),
                dialogs: DialogSink::default(),
                fields: FieldWrites::default(),
            }
        }
        fn cx(&mut self) -> UiCtx<'_> {
            UiCtx {
                core: &self.core,
                settings: &self.settings,
                player: &self.player,
                credits: &self.credits,
                app_info: &self.app_info,
                models: &self.models,
                now_ms: NOW,
                demo: false,
                reduce_motion: false,
                art: &mut self.art,
                toasts: &mut self.toasts,
                nav: &mut self.nav,
                dialogs: &mut self.dialogs,
                fields: &mut self.fields,
            }
        }
        fn sent(&self) -> Vec<Command> {
            self.rx.try_iter().collect()
        }
        /// The id of the last `QueryHome` sent.
        fn last_query(&self) -> Option<RequestId> {
            self.sent().into_iter().filter_map(|c| if let Command::QueryHome { req } = c { Some(req) } else { None }).last()
        }
    }

    fn ok_answer() -> Result<(Arc<AnalyticsSummary>, Vec<ReadRecord>), String> {
        Ok((Arc::new(summary(600.0, &[("2026-10-17", 600.0)])), vec![record("r1"), record("r2")]))
    }

    #[test]
    fn first_load_error_retry_then_ready() {
        let mut w = World::new();
        let mut h = HomeController::default();
        h.on_enter(&mut w.cx(), &AppRoute::page(AppPage::Home), &NavIntent::None);
        let req = w.last_query().expect("queried on enter");
        let d = h.build(&mut w.cx());
        assert_eq!(d.state, LoadState::Loading);
        assert!(d.recent_loading && d.today.loading && d.week.loading);

        // A failed first load: the inline error with Retry.
        assert!(h.on_update(&mut w.cx(), &UiUpdate::Home { req, result: Err("HistoryError: The history file is locked".into()) }));
        let d = h.build(&mut w.cx());
        assert_eq!(d.state, LoadState::Error);
        assert_eq!(d.error_title, "Couldn\u{2019}t load your reading stats");
        assert_eq!(d.error_body, "The history file is locked");
        assert!(!d.retrying && !d.recent_loading);

        // Retry spins until its answer; a stale answer is dropped.
        h.retry(&mut w.cx());
        let retry = w.last_query().expect("retry queried at once");
        assert!(h.build(&mut w.cx()).retrying);
        assert!(!h.on_update(&mut w.cx(), &UiUpdate::Home { req, result: ok_answer() }), "stale id");
        assert!(h.on_update(&mut w.cx(), &UiUpdate::Home { req: retry, result: ok_answer() }));
        let d = h.build(&mut w.cx());
        assert_eq!(d.state, LoadState::Ready);
        assert!(!d.retrying && !d.recent_loading);
        assert_eq!(d.today.sub, "5m to your daily goal");
        use slint::Model;
        assert_eq!(d.recent.row_count(), 2);

        // A later failure keeps the last data.
        h.on_wake(&mut w.cx());
        let wake = w.last_query().expect("wake queried at once");
        assert!(h.on_update(&mut w.cx(), &UiUpdate::Home { req: wake, result: Err("boom".into()) }));
        assert_eq!(h.build(&mut w.cx()).state, LoadState::Ready);

        // Leaving drops in-flight answers; a day rollover while away does not query.
        h.on_update(&mut w.cx(), &UiUpdate::Refresh(RefreshReason::Day));
        let day = w.last_query().expect("day rollover queried at once");
        h.on_leave(&mut w.cx());
        assert!(!h.on_update(&mut w.cx(), &UiUpdate::Home { req: day, result: ok_answer() }));
        h.on_update(&mut w.cx(), &UiUpdate::Refresh(RefreshReason::Day));
        assert_eq!(w.last_query(), None);
    }

    #[test]
    fn quick_read_and_clipboard_busy_flags() {
        let mut w = World::new();
        let mut h = HomeController::default();
        h.on_enter(&mut w.cx(), &AppRoute::page(AppPage::Home), &NavIntent::None);
        w.sent();
        h.quick_read_edited(&mut w.cx(), "  ");
        let d = h.build(&mut w.cx());
        assert!(!d.quick_read_can_read && d.quick_read_words.is_empty());
        h.quick_read(&mut w.cx(), "  ");
        assert!(w.sent().is_empty(), "blank text is not read");
        h.quick_read_edited(&mut w.cx(), " Hello  there ");
        let d = h.build(&mut w.cx());
        assert!(d.quick_read_can_read);
        assert_eq!(d.quick_read_words, "2 words");
        h.quick_read(&mut w.cx(), " Hello  there ");
        let req = match w.sent().pop() {
            Some(Command::ReadText { req: Some(req), text, options }) => {
                assert_eq!(text, " Hello  there ", "the untrimmed text");
                assert_eq!(options.source, Some(ReadSource::Manual));
                req
            }
            _ => panic!("expected ReadText"),
        };
        assert!(h.build(&mut w.cx()).read_busy);
        // Only one of the two runs at a time.
        h.read_clipboard(&mut w.cx());
        h.quick_read(&mut w.cx(), "again");
        assert!(w.sent().is_empty());
        // The core's panic answer ends the spinner with the read-failed toast.
        assert!(h.on_update(&mut w.cx(), &UiUpdate::ReadStarted { req, result: ReadResult::internal_error() }));
        assert!(!h.build(&mut w.cx()).read_busy);
        assert_eq!(w.toasts.pushes.pop().map(|(t, _)| t.id), Some("read-failed".to_string()));
        // Clipboard: an ordinary failure is the core's toast, not Home's.
        h.read_clipboard(&mut w.cx());
        let req = match w.sent().pop() {
            Some(Command::ReadClipboard { req: Some(req) }) => req,
            _ => panic!("expected ReadClipboard"),
        };
        assert!(h.build(&mut w.cx()).clipboard_busy);
        assert!(h.on_update(&mut w.cx(), &UiUpdate::ReadStarted { req, result: ReadResult::clipboard_empty() }));
        assert!(!h.build(&mut w.cx()).clipboard_busy);
        assert!(w.toasts.pushes.is_empty());
    }

    #[test]
    fn transport_mapping_and_replays() {
        let mut w = World::new();
        let mut h = HomeController::default();
        h.on_enter(&mut w.cx(), &AppRoute::page(AppPage::Home), &NavIntent::None);
        let req = w.last_query().expect("query");
        h.on_update(&mut w.cx(), &UiUpdate::Home { req, result: ok_answer() });

        // Playing → toggle; ended with a read id → replay; idle → nothing.
        w.player.state = PlayerState { status: PlayerStatus::Playing, read_id: Some("r1".into()), ..PlayerState::idle() };
        h.player_action(&mut w.cx(), PlayerAction::Toggle);
        assert!(matches!(w.sent().pop(), Some(Command::Player(PlayerCommand::Toggle))));
        w.player.state.status = PlayerStatus::Ended;
        h.player_action(&mut w.cx(), PlayerAction::Toggle);
        assert!(matches!(w.sent().pop(), Some(Command::Replay { read_id, .. }) if read_id == "r1"));
        w.player.state = PlayerState::idle();
        h.player_action(&mut w.cx(), PlayerAction::Toggle);
        h.seek(&mut w.cx(), 0.5);
        assert!(w.sent().is_empty());

        // A Recent row's replay owns its busy marker; a not-cached answer toasts `replay-{id}`.
        h.replay(&mut w.cx(), "r2");
        let req = match w.sent().pop() {
            Some(Command::Replay { req: Some(req), read_id }) if read_id == "r2" => req,
            _ => panic!("expected a replay"),
        };
        let d = h.build(&mut w.cx());
        use slint::Model;
        assert!(d.recent.row_data(1).is_some_and(|r| r.busy));
        assert!(h.on_update(&mut w.cx(), &UiUpdate::ReadStarted { req, result: ReadResult::not_cached("r2") }));
        assert!(!h.build(&mut w.cx()).recent.row_data(1).is_some_and(|r| r.busy));
        assert_eq!(w.toasts.pushes.pop().map(|(t, _)| t.id), Some("replay-r2".to_string()));

        // "Replay last" replays the newest read and owns its own spinner.
        h.replay_last(&mut w.cx());
        let req = match w.sent().pop() {
            Some(Command::Replay { req: Some(req), read_id }) if read_id == "r1" => req,
            _ => panic!("expected replay-last"),
        };
        assert!(h.build(&mut w.cx()).now.replay_last_busy);
        h.replay_last(&mut w.cx());
        assert!(w.sent().is_empty(), "one at a time");
        h.on_update(&mut w.cx(), &UiUpdate::ReadStarted { req, result: ReadResult::started("r1") });
        assert!(!h.build(&mut w.cx()).now.replay_last_busy);

        // Paused live read: Resume; settings links validate the section.
        h.resume_live(&mut w.cx());
        assert!(matches!(w.sent().pop(), Some(Command::Player(PlayerCommand::Play))));
        h.open_settings(&mut w.cx(), "triggers");
        assert_eq!(w.nav.target.take().map(|(r, _)| r), Some(AppRoute::settings("triggers")));
    }
}
