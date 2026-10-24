//! History controller (ui-main.md §4): filters (search debounced 220 ms, empty applies at once; favourites; source),
//! the fetch cycle (page size 50, windows of ≤ 500, unfiltered total when filtered, stale request ids dropped,
//! pending/deleted id sets, reconcile unchanged rows; background refreshes on `HistoryChanged` debounced 700 ms), load
//! more (`offset = items.len()`), local edits (favourite optimistic flip; `FavoriteToggled` `Ok(Some)` applies,
//! `Ok(None)` keeps, `Err` reverts + toast `Couldn’t update favorites: …` id `history-favorite`), delete-with-undo,
//! export (`ExportHistory{req, owner: main HWND}`), clear-all dialog (`history-clear`), day groups
//! (`format::day_label`), rows (`listened_fraction`, `audio_length` — `~0:20` in the row, `about 0:20` in details —,
//! badges, replay rules), expanded details, copy text (`Done{req}` → toast id `history-copy`: `Text copied` / `Copied
//! the saved preview`; failure `Couldn’t copy to the clipboard`), page links (`OpenExternal{req}` failure →
//! `copy::open_link_failed`), the state copy (`state-body` + keycaps + `state-body-after`).
//!
//! **Undo batch** (ui-main.md §4.10; ARCHITECTURE.md §7.4): deleting pushes `Removed from history` (id
//! `history-remove`, `UiToastAction::Page(History)` for `Undo`). After every stack change (`on_toast_removed`), if the
//! batch is non-empty and no toast with id `history-remove` is in the stack (a leaving toast still counts as present;
//! a same-id replacement never reports a removal), commit (`RemoveReads`). Also commit on page leave, before Export or
//! Clear all, and before the UI sends `Command::Quit` ([`HistoryController::flush`]). Undo restores the batch. A
//! `history-load` error toast has `Retry` (`UiToastAction::Page(History)`) → forced reload.
//!
//! Search: the field's text is the main window's in-out `history-search-text`; the controller writes it only through
//! `FieldWrites` (Clear filters, a sidebar search). `on_enter` with `NavIntent::Search(text)` applies the search at
//! once (no debounce) and writes the field. Ctrl+F / `/` focus the field (Slint-local nonce, §7.4).
//!
//! Implementation notes:
//! - The two debounce timers (search 220 ms, background refresh 700 ms) go through the app when they fire
//!   (`ui::with_app` → `AppUi::history_cb` → [`HistoryController::search_settled`] / [`HistoryController::refresh_due`]),
//!   so the sinks are flushed and the page is rebuilt at once: a settled search shows the dimmed, busy list and the
//!   applied `search` before its answer arrives (ui-main.md §4.2 step 1, §4.5 #5). `with_app` runs under the UI
//!   panic guard.
//! - Body state order (ui-main.md §4.5): "nothing yet" (`grandTotal == 0`, no undo batch pending — D-UM-12) is decided
//!   before "no matches", so a filter on an empty history still shows `Nothing read yet`.
//! - Player badges: `build` records the `(read, Playing/Paused)` pair it drew the rows for; a `UiUpdate::Player` is
//!   dirty only when that pair changed. Recording it at build time (not on the update) keeps it right across a leave
//!   and re-entry (only the shown page receives `Player`).
//! - Removals are sent one id per `RemoveReads` so each failure restores exactly its record (ui-main.md §4.10 step 3).
//! - Rows are kept in one `VecModel` updated in place (prefix/suffix diff), so Slint keeps row instances and focus
//!   (`history.slint` header).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use chrono::TimeZone;
use slint::{Image, Model, ModelRc, Timer, TimerMode, VecModel};
use vox_core::app::{AppPage, AppRoute, Toast};
use vox_core::constants::{HISTORY_PAGE_SIZE, HISTORY_SEARCH_DEBOUNCE_MS, PAGE_REFRESH_DEBOUNCE_MS, READ_WITH_LABEL};
use vox_core::copy::open_link_failed;
use vox_core::format::{
    clean_error, count_label, display_url, format_clock, format_duration, format_number, full_date_time, model_name,
    row_time, truncate,
};
use vox_core::history::{audio_length, can_replay, listened_fraction, HistoryPage, HistoryQuery, ReadRecord, ReadSource};
use vox_core::js::{js_round, js_trim};
use vox_core::player::{PlayerSnapshot, PlayerStatus};
use vox_core::read::{ReadErrorCode, ReadResult};
use vox_core::settings::Settings;
use vox_core::time::day_key_ms;
use vox_core::voices::TtsModel;

use super::art::ArtSize;
use super::generated::{ChoiceItem, Fact, HistoryData, HistoryRow, HistoryRowKind, HistoryState};
use super::shell::ToastRemoval;
use super::{DialogSpec, NavIntent, PageController, ToastSink, UiCtx, UiToastAction};
use crate::command::{Command, CoreHandle, RequestId};
use crate::update::UiUpdate;

/// Undo toast id (`UNDO_TOAST_ID`).
pub const UNDO_TOAST_ID: &str = "history-remove";
/// Background load error toast id (action `Retry`).
pub const LOAD_TOAST_ID: &str = "history-load";
/// The clear-all dialog id.
pub const CLEAR_DIALOG_ID: &str = "history-clear";
/// main's list clamp (`MAX_LIST_PAGE`).
const MAX_LIST_PAGE: usize = 500;
/// Characters of the collapsed row title.
const TITLE_MAX: usize = 320;
/// More rows than this end with the `End of history` divider.
const END_DIVIDER_AFTER: usize = 8;

// ───────────────────────────── shared helpers (also used by voices.rs) ─────────────────────────────

/// Updates `model` in place from `prev` to `next` (common prefix and suffix untouched; changed rows in the middle are
/// replaced, extra rows removed or inserted), so Slint keeps the row instances (and their focus) it can.
pub(super) fn sync_model<V: PartialEq, R: Clone + 'static>(
    model: &VecModel<R>,
    prev: &mut Vec<(V, Image)>,
    next: Vec<(V, Image)>,
    make: impl Fn(&V, &Image) -> R,
) {
    let same = |a: &(V, Image), b: &(V, Image)| a.0 == b.0 && a.1 == b.1;
    if model.row_count() != prev.len() {
        model.set_vec(next.iter().map(|(v, i)| make(v, i)).collect::<Vec<R>>());
        *prev = next;
        return;
    }
    let mut start = 0;
    while start < prev.len() && start < next.len() && same(&prev[start], &next[start]) {
        start += 1;
    }
    let (mut end_p, mut end_n) = (prev.len(), next.len());
    while end_p > start && end_n > start && same(&prev[end_p - 1], &next[end_n - 1]) {
        end_p -= 1;
        end_n -= 1;
    }
    let (old_mid, new_mid) = (end_p - start, end_n - start);
    let common = old_mid.min(new_mid);
    for k in 0..common {
        let i = start + k;
        if !same(&prev[i], &next[i]) {
            model.set_row_data(i, make(&next[i].0, &next[i].1));
        }
    }
    if old_mid > common {
        for _ in common..old_mid {
            model.remove(start + common);
        }
    } else {
        for k in common..new_mid {
            let i = start + k;
            model.insert(i, make(&next[i].0, &next[i].1));
        }
    }
    *prev = next;
}

/// `isWebUrl`: non-empty and `^https?://` (case-insensitive).
fn is_web_url(url: &str) -> bool {
    let low = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    low.starts_with("http://") || low.starts_with("https://")
}

fn fact(label: &str, value: String, tooltip: String) -> (String, String, String) {
    (label.to_string(), value, tooltip)
}

fn facts_model(facts: &[(String, String, String)]) -> ModelRc<Fact> {
    let v: Vec<Fact> = facts
        .iter()
        .map(|(l, v, t)| Fact { label: l.into(), value: v.into(), tooltip: t.into() })
        .collect();
    ModelRc::new(VecModel::from(v))
}

// ───────────────────────────── row view (pure) ─────────────────────────────

/// One row of the flat list as plain data (compared to update the Slint model in place).
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct RowView {
    pub header: bool,
    pub id: String,
    pub header_label: String,
    pub header_meta: String,
    /// Voice-art seed (voice id, else voice name, else app).
    pub art_seed: String,
    pub source_icon: String,
    pub source_label: String,
    pub title: String,
    pub meta: String,
    pub time_tooltip: String,
    pub favorite: bool,
    pub badges: Vec<String>,
    pub progress: f32,
    pub percent: String,
    pub complete: bool,
    pub can_replay: bool,
    pub replay_label: String,
    pub replay_tooltip: String,
    pub expanded: bool,
    pub full_text: String,
    pub notes: Vec<String>,
    pub facts: Vec<(String, String, String)>,
    pub details: Vec<(String, String, String)>,
    pub url_label: String,
}

fn row_to_slint(v: &RowView, art: &Image) -> HistoryRow {
    HistoryRow {
        kind: if v.header { HistoryRowKind::Header } else { HistoryRowKind::Read },
        id: v.id.as_str().into(),
        header_label: v.header_label.as_str().into(),
        header_meta: v.header_meta.as_str().into(),
        art: art.clone(),
        source_icon: v.source_icon.as_str().into(),
        source_label: v.source_label.as_str().into(),
        title: v.title.as_str().into(),
        meta: v.meta.as_str().into(),
        time_tooltip: v.time_tooltip.as_str().into(),
        favorite: v.favorite,
        badges: super::convert::strings(v.badges.iter().cloned()),
        progress: v.progress,
        percent: v.percent.as_str().into(),
        complete: v.complete,
        can_replay: v.can_replay,
        replay_label: v.replay_label.as_str().into(),
        replay_tooltip: v.replay_tooltip.as_str().into(),
        expanded: v.expanded,
        full_text: v.full_text.as_str().into(),
        notes: super::convert::strings(v.notes.iter().cloned()),
        facts: facts_model(&v.facts),
        details: facts_model(&v.details),
        url_label: v.url_label.as_str().into(),
    }
}

/// The listened percentage text and value of a record (`listenedFraction`, capped at 99 % unless completed).
fn heard(r: &ReadRecord) -> (f64, i64) {
    let f = listened_fraction(r);
    (f, js_round(f * 100.0) as i64)
}

/// Audio length for the row (`~0:20`) and the details (`about 0:20`).
pub(crate) fn audio_strings(r: &ReadRecord) -> (String, String) {
    let (sec, estimate) = audio_length(r);
    let clock = format_clock(sec);
    if estimate {
        (format!("~{clock}"), format!("about {clock}"))
    } else {
        (clock.clone(), clock)
    }
}

/// Which badge the player gives a row (`Playing` covers loading too).
fn player_badge(r: &ReadRecord, player: &PlayerSnapshot) -> Option<&'static str> {
    if player.state.read_id.as_deref() != Some(r.id.as_str()) {
        return None;
    }
    match player.state.status {
        PlayerStatus::Playing | PlayerStatus::Loading => Some("Playing"),
        PlayerStatus::Paused => Some("Paused"),
        _ => None,
    }
}

/// A collapsed (and, when `expanded`, detailed) read row.
pub(crate) fn read_row<Tz: TimeZone>(
    tz: &Tz,
    r: &ReadRecord,
    now_ms: f64,
    player: &PlayerSnapshot,
    expanded: bool,
    models: &[TtsModel],
) -> RowView {
    let app = r.source_app.clone().unwrap_or_else(|| r.source.label().to_string());
    let voice = if r.voice_name.is_empty() { "Unknown voice".to_string() } else { r.voice_name.clone() };
    let (row_len, detail_len) = audio_strings(r);
    let meta = [app, voice, row_len, row_time(tz, r.created_at, now_ms)].join(" \u{00B7} ");
    let in_player = player_badge(r, player);
    let mut badges: Vec<String> = Vec::new();
    if let Some(b) = in_player {
        badges.push(b.to_string());
    }
    if r.error.is_some() {
        badges.push("Failed".to_string());
    }
    if r.cached && in_player.is_none() {
        badges.push("Free replay".to_string());
    }
    let (fraction, pct) = heard(r);
    let replayable = can_replay(r);
    let replay_tooltip = if r.cached {
        "Free replay from saved audio"
    } else if replayable {
        "Replay \u{00B7} uses credits"
    } else {
        "Audio no longer cached and full text wasn\u{2019}t saved"
    };
    let title = if r.text.is_empty() { "Empty read".to_string() } else { truncate(&r.text, TITLE_MAX) };
    let art_seed = if !r.voice_id.is_empty() {
        r.voice_id.clone()
    } else if !r.voice_name.is_empty() {
        r.voice_name.clone()
    } else {
        r.source_app.clone().unwrap_or_else(|| r.source.label().to_string())
    };
    let mut row = RowView {
        header: false,
        id: r.id.clone(),
        art_seed,
        source_icon: r.source.icon().to_string(),
        source_label: r.source.label().to_string(),
        title,
        meta,
        time_tooltip: full_date_time(tz, r.created_at),
        favorite: r.favorite,
        badges,
        progress: fraction as f32,
        percent: format!("{pct}%"),
        complete: fraction >= 1.0,
        can_replay: replayable,
        replay_label: if r.cached { "Replay (free)" } else { "Replay" }.to_string(),
        replay_tooltip: replay_tooltip.to_string(),
        expanded,
        ..RowView::default()
    };
    if expanded {
        row.full_text = r.text.clone();
        if r.text_truncated {
            row.notes.push("Only a short preview was saved for this read (full text storage is off).".to_string());
        }
        if let Some(e) = &r.error {
            row.notes.push(e.clone());
        }
        let chars = format_number(r.char_count as f64);
        let words = format_number(r.word_count as f64);
        let billed = format_number(r.characters_billed as f64);
        row.facts = vec![
            fact("Chars", chars.clone(), format!("{chars} characters")),
            fact("Words", words.clone(), format!("{words} words")),
            fact("Credits", billed.clone(), format!("{billed} characters billed")),
            fact("Heard", format!("{pct}%"), format!("{pct}% listened")),
        ];
        let source = match &r.source_app {
            Some(app) => format!("{} \u{00B7} {app}", r.source.label()),
            None => r.source.label().to_string(),
        };
        row.details.push(fact("Source", source, String::new()));
        if let Some(t) = &r.source_title {
            row.details.push(fact("Window", t.clone(), t.clone()));
        }
        let model = if r.model_id.is_empty() { "Unknown model".to_string() } else { model_name(&r.model_id, models) };
        let voice = if r.voice_name.is_empty() { "Unknown" } else { r.voice_name.as_str() };
        row.details.push(fact("Voice", format!("{voice} \u{00B7} {model}"), String::new()));
        let audio = if r.cached { format!("{detail_len} \u{00B7} saved for free replay") } else { detail_len };
        row.details.push(fact("Audio", audio, String::new()));
        row.details.push(fact("Read", full_date_time(tz, r.created_at), String::new()));
        if let Some(url) = r.source_url.as_deref().filter(|u| is_web_url(u)) {
            row.url_label = display_url(url);
        }
    }
    row
}

/// Day groups (`groupByDay`) flattened into header + read rows.
pub(crate) fn row_views<Tz: TimeZone>(
    tz: &Tz,
    items: &[ReadRecord],
    now_ms: f64,
    player: &PlayerSnapshot,
    expanded: &HashSet<String>,
    models: &[TtsModel],
) -> Vec<RowView> {
    let mut out = Vec::with_capacity(items.len() + 8);
    let mut i = 0;
    while i < items.len() {
        let key = day_key_ms(tz, items[i].created_at);
        let mut j = i + 1;
        while j < items.len() && day_key_ms(tz, items[j].created_at) == key {
            j += 1;
        }
        let group = &items[i..j];
        let n = group.len();
        let listened: f64 = group.iter().map(|r| r.listened_sec.max(0.0)).sum();
        let mut meta = format!("{n} {}", if n == 1 { "read" } else { "reads" });
        if listened > 0.0 {
            meta.push_str(&format!(" \u{00B7} {}", format_duration(listened)));
        }
        out.push(RowView {
            header: true,
            id: format!("h-{key}"),
            header_label: vox_core::format::day_label(tz, items[i].created_at, now_ms),
            header_meta: meta,
            ..RowView::default()
        });
        for r in group {
            out.push(read_row(tz, r, now_ms, player, expanded.contains(&r.id), models));
        }
        i = j;
    }
    out
}

// ───────────────────────────── page state (pure, no Slint) ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Loading,
    Ready,
    Error,
}

#[derive(Debug)]
enum Fetch {
    /// `fetchWindow`: pages from offset 0 until `target` rows or a short page.
    Refresh { window: Vec<ReadRecord>, offset: usize, target: usize, filtered: bool, grand: Option<usize> },
    LoadMore,
}

/// What a history-version bump asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trigger {
    Now,
    Debounce,
}

#[derive(Debug)]
struct Inner {
    active: bool,
    // ── filters ──
    /// The trimmed field text (pending the 220 ms debounce).
    search_raw: String,
    /// The applied search.
    search: String,
    favorites_only: bool,
    source: Option<ReadSource>,
    expanded: HashSet<String>,
    revision: i32,
    // ── list ──
    items: Vec<ReadRecord>,
    total: usize,
    grand_total: usize,
    status: Status,
    error: Option<String>,
    loaded_once: bool,
    loading_more: bool,
    wanted: usize,
    fetch: Option<(RequestId, Fetch)>,
    /// Locally removed (in the undo window, or being deleted).
    pending: HashSet<String>,
    /// Deleted by the core (may still appear in a response already in flight).
    deleted: HashSet<String>,
    // ── undo ──
    batch: Vec<ReadRecord>,
    undo_toast: bool,
    /// The background error last toasted (`history-load`).
    toasted_error: Option<String>,
    // ── actions ──
    favorites: HashMap<RequestId, (String, bool)>,
    copies: HashMap<RequestId, bool>,
    removals: HashMap<RequestId, ReadRecord>,
    links: HashSet<RequestId>,
    replays: HashMap<RequestId, String>,
    export_req: Option<RequestId>,
    clear_req: Option<RequestId>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            active: false,
            search_raw: String::new(),
            search: String::new(),
            favorites_only: false,
            source: None,
            expanded: HashSet::new(),
            revision: 1,
            items: Vec::new(),
            total: 0,
            grand_total: 0,
            status: Status::Loading,
            error: None,
            loaded_once: false,
            loading_more: false,
            wanted: HISTORY_PAGE_SIZE,
            fetch: None,
            pending: HashSet::new(),
            deleted: HashSet::new(),
            batch: Vec::new(),
            undo_toast: false,
            toasted_error: None,
            favorites: HashMap::new(),
            copies: HashMap::new(),
            removals: HashMap::new(),
            links: HashSet::new(),
            replays: HashMap::new(),
            export_req: None,
            clear_req: None,
        }
    }
}

impl Inner {
    fn filtered(&self) -> bool {
        !self.search.is_empty() || self.favorites_only || self.source.is_some()
    }

    fn refreshing(&self) -> bool {
        matches!(self.fetch, Some((_, Fetch::Refresh { .. })))
    }

    fn query(&self, offset: usize, limit: usize) -> HistoryQuery {
        HistoryQuery {
            search: (!self.search.is_empty()).then(|| self.search.clone()),
            limit: Some(limit as f64),
            offset: Some(offset as f64),
            favorites_only: self.favorites_only,
            source: self.source,
        }
    }

    // ── fetch cycle ──

    /// A refresh (filters changed, forced reload, debounced background change): fetch the window now. Supersedes an
    /// in-flight load-more (its answer is dropped; `loading_more` ends with this refresh).
    fn start_refresh(&mut self, core: &CoreHandle) {
        let target = self.wanted.max(HISTORY_PAGE_SIZE);
        let filtered = self.filtered();
        let req = core.request_id();
        core.send(Command::QueryHistory { req, query: self.query(0, target.min(MAX_LIST_PAGE)), with_grand_total: filtered });
        self.fetch = Some((req, Fetch::Refresh { window: Vec::new(), offset: 0, target, filtered, grand: None }));
    }

    /// Filters changed: back to one page, `loading` over the current rows, fetch at once.
    fn filters_changed(&mut self, core: &CoreHandle) {
        self.wanted = HISTORY_PAGE_SIZE;
        self.status = Status::Loading;
        self.start_refresh(core);
    }

    /// A forced reload (Retry, after an undo or a failed delete, after Clear all).
    fn reload(&mut self, core: &CoreHandle) {
        if self.items.is_empty() {
            self.status = Status::Loading;
        }
        self.start_refresh(core);
    }

    /// The history version bumped: debounce while `ready`, else fetch now.
    fn history_changed(&mut self, core: &CoreHandle) -> Trigger {
        if self.status == Status::Ready {
            Trigger::Debounce
        } else {
            self.start_refresh(core);
            Trigger::Now
        }
    }

    /// Applies a refresh window (steps 5–7 of the fetch cycle).
    fn apply_window(&mut self, window: Vec<ReadRecord>, page_total: usize, filtered: bool, all_total: usize) {
        let before = window.len();
        let rows: Vec<ReadRecord> =
            window.into_iter().filter(|r| !self.pending.contains(&r.id) && !self.deleted.contains(&r.id)).collect();
        let removed = before - rows.len();
        self.total = page_total.saturating_sub(removed);
        self.grand_total = if filtered { all_total.saturating_sub(self.pending.len()) } else { self.total };
        self.items = rows;
        self.status = Status::Ready;
        self.error = None;
        self.loading_more = false;
        self.loaded_once = true;
    }

    /// `UiUpdate::HistoryPage`; true when the page must be rebuilt.
    fn on_page(&mut self, req: RequestId, result: &Result<(HistoryPage, usize), String>, core: &CoreHandle) -> bool {
        if self.fetch.as_ref().map(|(r, _)| *r) != Some(req) {
            return false;
        }
        let Some((_, fetch)) = self.fetch.take() else { return false };
        match fetch {
            Fetch::Refresh { mut window, offset, target, filtered, mut grand } => match result {
                Ok((page, all_total)) => {
                    if filtered && grand.is_none() {
                        grand = Some(*all_total);
                    }
                    let requested = target.saturating_sub(window.len()).clamp(1, MAX_LIST_PAGE);
                    let got = page.items.len();
                    let mut seen: HashSet<String> = window.iter().map(|r| r.id.clone()).collect();
                    for r in &page.items {
                        if seen.insert(r.id.clone()) {
                            window.push(r.clone());
                        }
                    }
                    let next_offset = offset + got;
                    if window.len() < target && got >= requested && got > 0 && next_offset < page.total {
                        let req2 = core.request_id();
                        let limit = target.saturating_sub(window.len()).clamp(1, MAX_LIST_PAGE);
                        core.send(Command::QueryHistory { req: req2, query: self.query(next_offset, limit), with_grand_total: false });
                        self.fetch = Some((req2, Fetch::Refresh { window, offset: next_offset, target, filtered, grand }));
                        return false;
                    }
                    let all = grand.unwrap_or(page.total);
                    self.apply_window(window, page.total, filtered, all);
                    true
                }
                Err(e) => {
                    self.status = Status::Error;
                    self.error = Some(clean_error(e));
                    self.loading_more = false;
                    true
                }
            },
            Fetch::LoadMore => {
                self.loading_more = false;
                match result {
                    Ok((page, _)) => {
                        let mut seen: HashSet<String> = self.items.iter().map(|r| r.id.clone()).collect();
                        for r in &page.items {
                            if !self.pending.contains(&r.id) && !self.deleted.contains(&r.id) && seen.insert(r.id.clone()) {
                                self.items.push(r.clone());
                            }
                        }
                        self.total = self.items.len().max(page.total.saturating_sub(self.pending.len()));
                    }
                    Err(e) => self.error = Some(clean_error(e)),
                }
                true
            }
        }
    }

    /// `Load more`; false when refused.
    fn load_more(&mut self, core: &CoreHandle) -> bool {
        if self.refreshing() || self.loading_more || self.status != Status::Ready || self.items.len() >= self.total {
            return false;
        }
        let offset = self.items.len();
        self.wanted = offset + HISTORY_PAGE_SIZE;
        self.loading_more = true;
        let req = core.request_id();
        core.send(Command::QueryHistory { req, query: self.query(offset, HISTORY_PAGE_SIZE), with_grand_total: false });
        self.fetch = Some((req, Fetch::LoadMore));
        true
    }

    /// (Re)shows `Couldn’t load history: …` when an error appears while rows are on screen (same text: once).
    fn check_error_toast(&mut self, toasts: &mut ToastSink) {
        match &self.error {
            None => self.toasted_error = None,
            Some(e) if !self.items.is_empty() && self.toasted_error.as_deref() != Some(e.as_str()) => {
                toasts.push(
                    Toast::error(format!("Couldn\u{2019}t load history: {e}")).with_id(LOAD_TOAST_ID).with_action("Retry", None),
                    Some(UiToastAction::Page(AppPage::History)),
                );
                self.toasted_error = Some(e.clone());
            }
            Some(_) => {}
        }
    }

    // ── local edits ──

    /// `removeItem`.
    fn remove_item(&mut self, id: &str) {
        self.pending.insert(id.to_string());
        if let Some(pos) = self.items.iter().position(|r| r.id == id) {
            self.items.remove(pos);
            self.total = self.total.saturating_sub(1);
            self.grand_total = self.grand_total.saturating_sub(1);
        }
    }

    /// `restoreItems`: re-inserts records still pending (date order) and forces a refetch.
    fn restore_items(&mut self, records: Vec<ReadRecord>, core: &CoreHandle) {
        let mut n = 0;
        for r in records {
            if !self.pending.remove(&r.id) {
                continue;
            }
            if self.items.iter().any(|x| x.id == r.id) {
                continue;
            }
            let at = self.items.iter().position(|x| x.created_at < r.created_at).unwrap_or(self.items.len());
            self.items.insert(at, r);
            n += 1;
        }
        self.total += n;
        self.grand_total += n;
        if self.active {
            self.start_refresh(core);
        }
    }

    /// Delete click: collapse, add to the batch, remove locally, show / extend the undo toast.
    fn delete(&mut self, id: &str, toasts: &mut ToastSink) {
        let Some(record) = self.items.iter().find(|r| r.id == id).cloned() else { return };
        self.expanded.remove(id);
        if !self.batch.iter().any(|r| r.id == id) {
            self.batch.push(record);
        }
        self.remove_item(id);
        let n = self.batch.len();
        let message = if n == 1 {
            "Removed from history".to_string()
        } else {
            format!("{} reads removed from history", format_number(n as f64))
        };
        toasts.push(
            Toast::success(message).with_id(UNDO_TOAST_ID).with_action("Undo", None),
            Some(UiToastAction::Page(AppPage::History)),
        );
        self.undo_toast = true;
    }

    /// Commits the batch: one `RemoveReads` per record.
    fn commit(&mut self, core: &CoreHandle) {
        for r in std::mem::take(&mut self.batch) {
            let req = core.request_id();
            core.send(Command::RemoveReads { req, ids: vec![r.id.clone()] });
            self.removals.insert(req, r);
        }
    }

    /// `Undo`.
    fn undo(&mut self, core: &CoreHandle) {
        let batch = std::mem::take(&mut self.batch);
        if !batch.is_empty() {
            self.restore_items(batch, core);
        }
    }

    /// Commit now + dismiss the undo toast at once.
    fn flush(&mut self, core: &CoreHandle, toasts: &mut ToastSink) {
        self.commit(core);
        if self.undo_toast {
            toasts.dismiss(UNDO_TOAST_ID);
            self.undo_toast = false;
        }
    }

    /// A toast left the stack: commit when no `history-remove` toast is present any more.
    fn toast_removed(&mut self, id: &str, core: &CoreHandle) {
        if id == UNDO_TOAST_ID {
            self.undo_toast = false;
        }
        if !self.batch.is_empty() && !self.undo_toast {
            self.commit(core);
        }
    }

    fn toggle_favorite(&mut self, id: &str, core: &CoreHandle) {
        let Some(r) = self.items.iter_mut().find(|r| r.id == id) else { return };
        let previous = r.favorite;
        r.favorite = !previous;
        let req = core.request_id();
        core.send(Command::ToggleFavorite { req, id: id.to_string() });
        self.favorites.insert(req, (id.to_string(), previous));
    }

    fn on_favorite(&mut self, req: RequestId, result: &Result<Option<ReadRecord>, String>, toasts: &mut ToastSink) -> bool {
        let Some((id, previous)) = self.favorites.remove(&req) else { return false };
        match result {
            Ok(Some(rec)) => {
                if let Some(r) = self.items.iter_mut().find(|r| r.id == id) {
                    r.favorite = rec.favorite;
                }
            }
            Ok(None) => {}
            Err(e) => {
                if let Some(r) = self.items.iter_mut().find(|r| r.id == id) {
                    r.favorite = previous;
                }
                toasts.push(
                    Toast::error(format!("Couldn\u{2019}t update favorites: {}", clean_error(e))).with_id("history-favorite"),
                    None,
                );
            }
        }
        true
    }

    /// `Done{req}` for one of this page's actions; true when the page must be rebuilt.
    fn on_done(&mut self, req: RequestId, result: &Result<(), String>, core: &CoreHandle, toasts: &mut ToastSink, dialogs: &mut super::DialogSink) -> bool {
        if let Some(r) = self.removals.remove(&req) {
            match result {
                Ok(()) => {
                    self.pending.remove(&r.id);
                    self.deleted.insert(r.id);
                }
                Err(e) => {
                    self.restore_items(vec![r], core);
                    toasts.push(Toast::error(format!("Couldn\u{2019}t delete: {}", clean_error(e))).with_id("history-remove-error"), None);
                }
            }
            return true;
        }
        if let Some(truncated) = self.copies.remove(&req) {
            let toast = match result {
                Ok(()) => Toast::success(if truncated { "Copied the saved preview" } else { "Text copied" }),
                Err(_) => Toast::error("Couldn\u{2019}t copy to the clipboard"),
            };
            toasts.push(toast.with_id("history-copy"), None);
            return false;
        }
        if self.links.remove(&req) {
            if let Err(e) = result {
                toasts.push(Toast::error(open_link_failed(e)).with_id("open-external"), None);
            }
            return false;
        }
        if self.clear_req == Some(req) {
            self.clear_req = None;
            match result {
                Ok(()) => {
                    dialogs.close(CLEAR_DIALOG_ID);
                    self.expanded.clear();
                    toasts.push(Toast::success("History cleared").with_id(CLEAR_DIALOG_ID), None);
                    if self.active {
                        self.reload(core);
                    }
                }
                Err(e) => {
                    dialogs.set_busy(CLEAR_DIALOG_ID, false);
                    toasts.push(Toast::error(format!("Couldn\u{2019}t clear history: {}", clean_error(e))).with_id(CLEAR_DIALOG_ID), None);
                }
            }
            return true;
        }
        false
    }

    fn on_read_started(&mut self, req: RequestId, result: &ReadResult, toasts: &mut ToastSink) {
        let Some(id) = self.replays.remove(&req) else { return };
        if result.ok {
            return;
        }
        let nothing_left = matches!(result.code, Some(ReadErrorCode::NotCached))
            || (matches!(result.code, Some(ReadErrorCode::Unknown) | None) && result.read_id.is_none());
        if nothing_left {
            let msg = result.error.clone().filter(|e| !e.is_empty()).unwrap_or_else(|| "That read can\u{2019}t be replayed".into());
            toasts.push(Toast::error(msg).with_id(format!("replay-{id}")), None);
        }
    }

    // ── view ──

    fn state(&self) -> HistoryState {
        let empty = self.items.is_empty();
        match self.status {
            Status::Loading if empty => HistoryState::Loading,
            Status::Error if empty => HistoryState::Error,
            Status::Ready if empty => {
                // ui-main.md §4.5: #3 "nothing yet" ignores the filters; #4 only otherwise.
                if self.grand_total == 0 && self.batch.is_empty() {
                    HistoryState::NothingYet
                } else if self.filtered() {
                    HistoryState::NoMatches
                } else {
                    // D-UM-12: keep the header and toolbar while an undo batch is pending (or rows are still
                    // unloaded): an empty list with its footer.
                    HistoryState::List
                }
            }
            _ => HistoryState::List,
        }
    }

    fn count_label(&self) -> String {
        if self.grand_total == 0 && (self.status == Status::Loading || !self.loaded_once) {
            return "Loading".into();
        }
        if self.filtered() {
            format!("{} of {}", format_number(self.total as f64), count_label(self.grand_total as u64, "read", "reads"))
        } else {
            count_label(self.grand_total as u64, "read", "reads")
        }
    }

    /// The page struct without its rows (`rows` = an empty model).
    fn data(&self, settings: &Settings) -> HistoryData {
        let state = self.state();
        let mut d = HistoryData {
            state,
            dimmed: state == HistoryState::List && self.status == Status::Loading,
            count_label: self.count_label().into(),
            actions_visible: state != HistoryState::NothingYet,
            exporting: self.export_req.is_some(),
            clearing: self.clear_req.is_some(),
            can_export: self.grand_total > 0,
            search: self.search.as_str().into(),
            favorites_only: self.favorites_only,
            source: self.source.map(|s| s.as_str()).unwrap_or("").into(),
            source_options: source_options(),
            revision: self.revision,
            ..HistoryData::default()
        };
        match state {
            HistoryState::Error => {
                d.state_icon = "circle-alert".into();
                d.state_title = "Couldn\u{2019}t load your history".into();
                d.state_body = self.error.clone().unwrap_or_default().into();
                d.state_button = "Try again".into();
            }
            HistoryState::NothingYet => {
                let t = &settings.triggers;
                let hotkey = if t.hotkeys_enabled { js_trim(&t.hotkeys.read_selection) } else { "" };
                let mut body = format!("Select text in any app, right-click and choose \u{201C}{READ_WITH_LABEL}\u{201D}");
                if !hotkey.is_empty() {
                    body.push_str(" \u{2014} or press");
                    d.state_keys = super::convert::keycaps(hotkey);
                }
                d.state_icon = "history".into();
                d.state_title = "Nothing read yet".into();
                d.state_body = body.into();
                d.state_body_after = ". Every read lands here.".into();
                d.state_button = "Paste & read".into();
            }
            HistoryState::NoMatches => {
                let (icon, title, body) = if !self.search.is_empty() {
                    let with = if self.favorites_only || self.source.is_some() { " with these filters" } else { "" };
                    ("search", "No matches", format!("Nothing matches \u{201C}{}\u{201D}{with}.", self.search))
                } else if self.favorites_only {
                    ("star", "No favorites yet", "Star a read to keep it here.".to_string())
                } else {
                    let body = match self.source {
                        Some(s) => format!("No reads came from {} yet.", s.label()),
                        None => "No reads came from this source yet.".to_string(),
                    };
                    ("mouse-pointer-click", "No reads here", body)
                };
                d.state_icon = icon.into();
                d.state_title = title.into();
                d.state_body = body.into();
                d.state_button = "Clear filters".into();
            }
            HistoryState::Loading | HistoryState::List => {}
        }
        if state == HistoryState::List {
            if self.items.len() < self.total {
                d.footer = format!("Showing {} of {}", format_number(self.items.len() as f64), format_number(self.total as f64)).into();
                d.can_load_more = self.status == Status::Ready && !self.loading_more;
                d.loading_more = self.loading_more;
            } else {
                d.end_of_history = self.items.len() > END_DIVIDER_AFTER;
            }
        }
        d
    }
}

/// `All sources` + `SOURCE_ORDER`.
fn source_options() -> ModelRc<ChoiceItem> {
    let mut items = vec![(String::new(), "All sources".to_string())];
    items.extend(ReadSource::FILTER_ORDER.iter().map(|s| (s.as_str().to_string(), s.label().to_string())));
    super::convert::choices(&items)
}

// ───────────────────────────── controller ─────────────────────────────

/// What the rows show of the player: the read in it and its badge.
type PlayerKey = (Option<String>, Option<&'static str>);

/// History page state.
pub struct HistoryController {
    st: Inner,
    search_timer: Timer,
    refresh_timer: Timer,
    rows_model: Rc<VecModel<HistoryRow>>,
    rows_prev: RefCell<Vec<(RowView, Image)>>,
    /// The player `(read id, badge)` the rows were last built for (recorded by `build`).
    player_key: RefCell<PlayerKey>,
}

impl Default for HistoryController {
    fn default() -> Self {
        Self {
            st: Inner::default(),
            search_timer: Timer::default(),
            refresh_timer: Timer::default(),
            rows_model: Rc::new(VecModel::default()),
            rows_prev: RefCell::new(Vec::new()),
            player_key: RefCell::new((None, None)),
        }
    }
}

/// What the rows show of the player: the read in it and its badge.
fn player_key(player: &PlayerSnapshot) -> PlayerKey {
    let badge = match player.state.status {
        PlayerStatus::Playing | PlayerStatus::Loading => Some("Playing"),
        PlayerStatus::Paused => Some("Paused"),
        _ => None,
    };
    (player.state.read_id.clone().filter(|_| badge.is_some()), badge)
}

impl HistoryController {
    /// Starts (or restarts) the 700 ms background-refresh debounce; it fires through `AppUi::history_cb` (module docs).
    fn debounce_refresh(&self) {
        self.refresh_timer.start(TimerMode::SingleShot, Duration::from_millis(PAGE_REFRESH_DEBOUNCE_MS), || {
            super::with_app("history refresh debounce", |app| app.history_cb(|c, cx| c.refresh_due(cx)));
        });
    }

    /// The background-refresh debounce elapsed: fetch the window (inert once the page was left).
    pub(crate) fn refresh_due(&mut self, cx: &mut UiCtx) {
        if self.st.active {
            self.st.start_refresh(cx.core);
        }
    }

    /// The 220 ms search debounce settled: apply the typed text (filters changed → `loading`; the caller,
    /// `AppUi::history_cb`, rebuilds the page so the list dims and `search` shows at once). Inert once the page was
    /// left or when the text is already applied.
    pub(crate) fn search_settled(&mut self, cx: &mut UiCtx) {
        let st = &mut self.st;
        if st.active && st.search_raw != st.search {
            st.search = st.search_raw.clone();
            self.refresh_timer.stop();
            st.filters_changed(cx.core);
        }
    }

    pub fn search_edited(&mut self, cx: &mut UiCtx, text: &str) {
        let trimmed = js_trim(text).to_string();
        let st = &mut self.st;
        st.search_raw = trimmed.clone();
        if trimmed.is_empty() {
            // Clearing applies at once and cancels the debounce (D-UM-22).
            self.search_timer.stop();
            if !st.search.is_empty() {
                st.search.clear();
                self.refresh_timer.stop();
                st.filters_changed(cx.core);
            }
            return;
        }
        self.search_timer.start(TimerMode::SingleShot, Duration::from_millis(HISTORY_SEARCH_DEBOUNCE_MS), || {
            super::with_app("history search debounce", |app| app.history_cb(|c, cx| c.search_settled(cx)));
        });
    }

    pub fn set_favorites_only(&mut self, cx: &mut UiCtx, on: bool) {
        let st = &mut self.st;
        if st.favorites_only != on {
            st.favorites_only = on;
            self.refresh_timer.stop();
            st.filters_changed(cx.core);
        }
    }

    pub fn set_source(&mut self, cx: &mut UiCtx, value: &str) {
        let source = ReadSource::parse(value);
        let st = &mut self.st;
        if source.is_none() && !value.is_empty() {
            // An unknown value: show "All sources" again.
            st.revision += 1;
        }
        if st.source != source {
            st.source = source;
            self.refresh_timer.stop();
            st.filters_changed(cx.core);
        }
    }

    pub fn clear_filters(&mut self, cx: &mut UiCtx) {
        self.search_timer.stop();
        self.refresh_timer.stop();
        cx.fields.history_search_text = Some(String::new());
        let st = &mut self.st;
        let changed = st.filtered();
        st.search_raw.clear();
        st.search.clear();
        st.favorites_only = false;
        st.source = None;
        st.revision += 1;
        if changed {
            st.filters_changed(cx.core);
        }
    }

    pub fn toggle_row(&mut self, cx: &mut UiCtx, id: &str) {
        let _ = cx;
        let st = &mut self.st;
        if !st.expanded.remove(id) && st.items.iter().any(|r| r.id == id) {
            st.expanded.insert(id.to_string());
        }
    }

    pub fn replay(&mut self, cx: &mut UiCtx, id: &str) {
        let st = &mut self.st;
        let Some(r) = st.items.iter().find(|r| r.id == id) else { return };
        if !can_replay(r) {
            // Focusable but inert: the tooltip gives the reason.
            return;
        }
        let req = cx.core.request_id();
        cx.core.send(Command::Replay { req: Some(req), read_id: id.to_string() });
        st.replays.insert(req, id.to_string());
    }

    pub fn copy_text(&mut self, cx: &mut UiCtx, id: &str) {
        let st = &mut self.st;
        let Some(r) = st.items.iter().find(|r| r.id == id) else { return };
        let (text, truncated) = (r.text.clone(), r.text_truncated);
        let req = cx.core.request_id();
        cx.core.send(Command::CopyText { req, text });
        st.copies.insert(req, truncated);
    }

    pub fn toggle_favorite(&mut self, cx: &mut UiCtx, id: &str) {
        self.st.toggle_favorite(id, cx.core);
    }

    pub fn delete(&mut self, cx: &mut UiCtx, id: &str) {
        self.st.delete(id, cx.toasts);
    }

    pub fn open_url(&mut self, cx: &mut UiCtx, id: &str) {
        let st = &mut self.st;
        let Some(url) = st.items.iter().find(|r| r.id == id).and_then(|r| r.source_url.clone()) else { return };
        if !is_web_url(&url) {
            return;
        }
        let req = cx.core.request_id();
        cx.core.send(Command::OpenExternal { url, req: Some(req) });
        st.links.insert(req);
    }

    pub fn load_more(&mut self, cx: &mut UiCtx) {
        self.st.load_more(cx.core);
    }

    pub fn export(&mut self, cx: &mut UiCtx, owner: Option<isize>) {
        let st = &mut self.st;
        if st.export_req.is_some() || st.grand_total == 0 {
            return;
        }
        // Flush pending deletes first so just-removed reads are not in the file.
        st.flush(cx.core, cx.toasts);
        let req = cx.core.request_id();
        cx.core.send(Command::ExportHistory { req, owner });
        st.export_req = Some(req);
    }

    pub fn clear_all(&mut self, cx: &mut UiCtx) {
        let st = &self.st;
        if st.grand_total == 0 || st.clear_req.is_some() {
            return;
        }
        let n = st.grand_total as u64;
        cx.dialogs.open(DialogSpec {
            id: CLEAR_DIALOG_ID.into(),
            owner: AppPage::History,
            title: "Clear all history?".into(),
            body: format!(
                "All {} will be removed from this computer. This can\u{2019}t be undone.",
                count_label(n, "read", "reads")
            ),
            confirm_label: "Clear history".into(),
            cancel_label: "Cancel".into(),
            danger: true,
            busy: false,
        });
    }

    pub fn retry(&mut self, cx: &mut UiCtx) {
        self.refresh_timer.stop();
        self.st.reload(cx.core);
    }

    pub fn paste_and_read(&mut self, cx: &mut UiCtx) {
        cx.nav.navigate(AppRoute::page(AppPage::Home));
    }

    /// Commits a pending undo batch now and dismisses its toast (before `Command::Quit`).
    pub fn flush(&mut self, cx: &mut UiCtx) {
        self.st.flush(cx.core, cx.toasts);
    }
}

impl PageController for HistoryController {
    type Data = HistoryData;

    fn on_enter(&mut self, cx: &mut UiCtx, route: &AppRoute, intent: &NavIntent) {
        let _ = route;
        self.search_timer.stop();
        self.refresh_timer.stop();
        let field = match intent {
            NavIntent::Search(text) => text.clone(),
            NavIntent::None => String::new(),
        };
        let search = js_trim(&field).to_string();
        cx.fields.history_search_text = Some(field);
        let st = &mut self.st;
        let was_active = st.active;
        st.active = true;
        // A re-created page: filters, expanded rows and the list start over (pending deletes stay excluded).
        st.search_raw = search.clone();
        st.search = search;
        st.favorites_only = false;
        st.source = None;
        st.expanded.clear();
        st.revision += 1;
        if !was_active {
            st.items.clear();
            st.total = 0;
            st.grand_total = 0;
            st.loaded_once = false;
            st.error = None;
            st.toasted_error = None;
        }
        st.loading_more = false;
        st.filters_changed(cx.core);
    }

    fn on_leave(&mut self, cx: &mut UiCtx) {
        self.search_timer.stop();
        self.refresh_timer.stop();
        cx.fields.history_search_text = Some(String::new());
        let st = &mut self.st;
        st.flush(cx.core, cx.toasts);
        st.active = false;
        // In-flight answers are ignored from now on.
        st.fetch = None;
        st.loading_more = false;
    }

    fn on_update(&mut self, cx: &mut UiCtx, update: &UiUpdate) -> bool {
        let st = &mut self.st;
        match update {
            UiUpdate::HistoryChanged => {
                if st.active {
                    if st.history_changed(cx.core) == Trigger::Debounce {
                        self.debounce_refresh();
                    } else {
                        self.refresh_timer.stop();
                    }
                }
                false
            }
            UiUpdate::HistoryPage { req, result } => {
                let dirty = st.on_page(*req, result, cx.core);
                st.check_error_toast(cx.toasts);
                dirty
            }
            UiUpdate::FavoriteToggled { req, result } => st.on_favorite(*req, result, cx.toasts),
            UiUpdate::Done { req, result } => st.on_done(*req, result, cx.core, cx.toasts, cx.dialogs),
            UiUpdate::Exported { req, result } => {
                if st.export_req != Some(*req) {
                    return false;
                }
                st.export_req = None;
                match result {
                    Ok(Some(path)) => cx.toasts.push(
                        Toast::success(path.display().to_string()).with_title("History exported").with_id("history-export"),
                        None,
                    ),
                    Ok(None) => {}
                    Err(e) => cx.toasts.push(Toast::error(format!("Export failed: {}", clean_error(e))).with_id("history-export"), None),
                }
                true
            }
            UiUpdate::ReadStarted { req, result } => {
                st.on_read_started(*req, result, cx.toasts);
                false
            }
            // Badges follow the player: rebuild when the (read, playing/paused) pair differs from the one the rows
            // were last built for.
            UiUpdate::Player(_) => player_key(cx.player) != *self.player_key.borrow(),
            UiUpdate::Settings(_) | UiUpdate::Models { .. } | UiUpdate::Refresh(_) => true,
            _ => false,
        }
    }

    fn on_tick(&mut self, cx: &mut UiCtx) {
        // Relative times and day labels are rebuilt from `cx.now_ms` by `build`.
        let _ = cx;
    }

    fn on_toast_removed(&mut self, cx: &mut UiCtx, id: &str, why: ToastRemoval) {
        let _ = why;
        self.st.toast_removed(id, cx.core);
    }

    fn on_toast_action(&mut self, cx: &mut UiCtx, id: &str) -> bool {
        match id {
            UNDO_TOAST_ID => {
                self.st.undo(cx.core);
                true
            }
            LOAD_TOAST_ID => {
                self.refresh_timer.stop();
                self.st.reload(cx.core);
                true
            }
            _ => false,
        }
    }

    fn on_dialog(&mut self, cx: &mut UiCtx, id: &str, confirmed: bool) -> bool {
        if id != CLEAR_DIALOG_ID {
            return false;
        }
        let st = &mut self.st;
        if st.clear_req.is_some() {
            // Closing is ignored while clearing.
            return true;
        }
        if !confirmed {
            cx.dialogs.close(CLEAR_DIALOG_ID);
            return true;
        }
        st.flush(cx.core, cx.toasts);
        let req = cx.core.request_id();
        cx.core.send(Command::ClearHistory { req });
        st.clear_req = Some(req);
        cx.dialogs.set_busy(CLEAR_DIALOG_ID, true);
        true
    }

    fn build(&self, cx: &mut UiCtx) -> HistoryData {
        let st = &self.st;
        *self.player_key.borrow_mut() = player_key(cx.player);
        let mut data = st.data(cx.settings);
        let views = if data.state == HistoryState::List {
            row_views(&chrono::Local, &st.items, cx.now_ms, cx.player, &st.expanded, cx.models)
        } else {
            Vec::new()
        };
        let next: Vec<(RowView, Image)> = views
            .into_iter()
            .map(|v| {
                let img = if v.header { Image::default() } else { cx.art.image(&v.art_seed, ArtSize::Large) };
                (v, img)
            })
            .collect();
        sync_model(&self.rows_model, &mut self.rows_prev.borrow_mut(), next, row_to_slint);
        data.rows = ModelRc::from(self.rows_model.clone());
        data
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;
    use crossbeam_channel::{unbounded, Receiver};

    const NOW: f64 = 1_792_195_200_000.0; // 2026-10-17T00:00:00Z

    fn tz() -> FixedOffset {
        FixedOffset::east_opt(0).unwrap()
    }

    fn core() -> (CoreHandle, Receiver<Command>) {
        let (tx, rx) = unbounded();
        (CoreHandle::new(tx), rx)
    }

    fn rec(id: &str, created_at: f64) -> ReadRecord {
        let mut r = ReadRecord::new(id.into(), created_at, ReadSource::ContextChip);
        r.text = format!("Text of {id}");
        r.char_count = 300;
        r.word_count = 50;
        r.voice_id = "v1".into();
        r.voice_name = "Brian".into();
        r.model_id = "eleven_flash_v2_5".into();
        r
    }

    fn page(items: Vec<ReadRecord>, total: usize) -> Result<(HistoryPage, usize), String> {
        Ok((HistoryPage { items, total }, total))
    }

    fn last_query(rx: &Receiver<Command>) -> (RequestId, HistoryQuery, bool) {
        let mut out = None;
        for c in rx.try_iter() {
            if let Command::QueryHistory { req, query, with_grand_total } = c {
                out = Some((req, query, with_grand_total));
            }
        }
        out.expect("a QueryHistory was sent")
    }

    fn loaded(n: usize, total: usize) -> (Inner, CoreHandle, Receiver<Command>) {
        let (c, rx) = core();
        let mut st = Inner { active: true, ..Inner::default() };
        st.filters_changed(&c);
        let (req, _, _) = last_query(&rx);
        let items: Vec<ReadRecord> = (0..n).map(|i| rec(&format!("r{i}"), NOW - 60_000.0 * (i as f64 + 1.0))).collect();
        assert!(st.on_page(req, &page(items, total), &c));
        (st, c, rx)
    }

    #[test]
    fn audio_strings_row_and_details() {
        let mut r = rec("a", NOW);
        r.audio_duration_sec = 0.0;
        assert_eq!(audio_strings(&r), ("~0:20".to_string(), "about 0:20".to_string()));
        r.audio_duration_sec = 263.0;
        assert_eq!(audio_strings(&r), ("4:23".to_string(), "4:23".to_string()));
    }

    #[test]
    fn loading_then_list_state_and_counts() {
        let (c, rx) = core();
        let mut st = Inner { active: true, ..Inner::default() };
        st.filters_changed(&c);
        let s = Settings::default();
        let d = st.data(&s);
        assert_eq!(d.state, HistoryState::Loading);
        assert_eq!(d.count_label, "Loading");
        let (req, q, grand) = last_query(&rx);
        assert_eq!(q.effective_limit(), 50);
        assert!(!grand);
        let items: Vec<ReadRecord> = (0..50).map(|i| rec(&format!("r{i}"), NOW - 1000.0 * i as f64)).collect();
        assert!(st.on_page(req, &page(items, 214), &c));
        let d = st.data(&s);
        assert_eq!(d.state, HistoryState::List);
        assert_eq!(d.count_label, "214 reads");
        assert_eq!(d.footer, "Showing 50 of 214");
        assert!(d.can_load_more);
        assert!(d.actions_visible && d.can_export);
    }

    #[test]
    fn stale_answers_are_dropped() {
        let (c, rx) = core();
        let mut st = Inner { active: true, ..Inner::default() };
        st.filters_changed(&c);
        let (old, _, _) = last_query(&rx);
        st.favorites_only = true;
        st.filters_changed(&c);
        let (new, _, grand) = last_query(&rx);
        assert!(grand, "filtered queries ask for the unfiltered total");
        assert!(!st.on_page(old, &page(vec![rec("x", NOW)], 1), &c));
        assert_eq!(st.state(), HistoryState::Loading);
        assert!(st.on_page(new, &Ok((HistoryPage { items: vec![], total: 0 }, 214)), &c));
        let d = st.data(&Settings::default());
        assert_eq!(d.state, HistoryState::NoMatches);
        assert_eq!(d.count_label, "0 of 214 reads");
        assert_eq!(d.state_title, "No favorites yet");
        assert_eq!(d.state_icon, "star");
    }

    #[test]
    fn error_state_and_nothing_yet_copy() {
        let (c, rx) = core();
        let mut st = Inner { active: true, ..Inner::default() };
        st.filters_changed(&c);
        let (req, _, _) = last_query(&rx);
        assert!(st.on_page(req, &Err("Error: disk on fire".into()), &c));
        let d = st.data(&Settings::default());
        assert_eq!(d.state, HistoryState::Error);
        assert_eq!(d.state_title, "Couldn\u{2019}t load your history");
        assert_eq!(d.state_body, "disk on fire");
        assert_eq!(d.state_button, "Try again");

        st.reload(&c);
        let (req, _, _) = last_query(&rx);
        assert!(st.on_page(req, &page(vec![], 0), &c));
        let d = st.data(&Settings::default());
        assert_eq!(d.state, HistoryState::NothingYet);
        assert!(!d.actions_visible);
        assert_eq!(d.count_label, "0 reads");
        assert_eq!(d.state_body, "Select text in any app, right-click and choose \u{201C}Read with Vox\u{201D} \u{2014} or press");
        assert_eq!(d.state_keys.row_count(), 3);
        assert_eq!(d.state_body_after, ". Every read lands here.");
        let mut s = Settings::default();
        s.triggers.hotkeys_enabled = false;
        let d = st.data(&s);
        assert_eq!(d.state_body, "Select text in any app, right-click and choose \u{201C}Read with Vox\u{201D}");
        assert_eq!(d.state_keys.row_count(), 0);
    }

    #[test]
    fn no_matches_copy_by_priority() {
        let (mut st, _c, _rx) = loaded(0, 0);
        st.search = "quarterly forecast".into();
        st.source = Some(ReadSource::Hotkey);
        st.grand_total = 214;
        let d = st.data(&Settings::default());
        assert_eq!(d.state, HistoryState::NoMatches);
        assert_eq!(d.state_body, "Nothing matches \u{201C}quarterly forecast\u{201D} with these filters.");
        st.search.clear();
        let d = st.data(&Settings::default());
        assert_eq!(d.state_title, "No reads here");
        assert_eq!(d.state_body, "No reads came from Hotkey yet.");
        assert_eq!(d.state_button, "Clear filters");
    }

    #[test]
    fn count_label_singular_when_one() {
        let (mut st, _c, _rx) = loaded(1, 1);
        assert_eq!(st.count_label(), "1 read");
        st.favorites_only = true;
        st.grand_total = 1;
        assert_eq!(st.count_label(), "1 of 1 read");
    }

    #[test]
    fn delete_undo_and_commit_rules() {
        let (mut st, c, rx) = loaded(3, 3);
        let mut toasts = ToastSink::default();
        st.delete("r0", &mut toasts);
        st.delete("r1", &mut toasts);
        assert_eq!(st.items.len(), 1);
        assert_eq!((st.total, st.grand_total), (1, 1));
        let (t, local) = toasts.pushes.last().unwrap();
        assert_eq!(t.id, UNDO_TOAST_ID);
        assert_eq!(t.message, "2 reads removed from history");
        assert_eq!(*local, Some(UiToastAction::Page(AppPage::History)));
        assert_eq!(toasts.pushes[0].0.message, "Removed from history");
        // Another toast leaving does not commit while the undo toast is present.
        st.toast_removed("history-copy", &c);
        assert_eq!(st.batch.len(), 2);
        assert!(rx.try_iter().all(|c| !matches!(c, Command::RemoveReads { .. })));
        // Undo restores both in date order and refetches.
        st.undo(&c);
        assert_eq!(st.items.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["r0", "r1", "r2"]);
        assert_eq!((st.total, st.grand_total), (3, 3));
        assert!(rx.try_iter().any(|c| matches!(c, Command::QueryHistory { .. })));
        // The undo toast leaving afterwards commits nothing.
        st.toast_removed(UNDO_TOAST_ID, &c);
        assert!(rx.try_iter().all(|c| !matches!(c, Command::RemoveReads { .. })));
    }

    #[test]
    fn undo_toast_gone_commits_one_remove_per_record() {
        let (mut st, c, rx) = loaded(3, 3);
        let mut toasts = ToastSink::default();
        st.delete("r0", &mut toasts);
        st.delete("r2", &mut toasts);
        st.toast_removed(UNDO_TOAST_ID, &c);
        let removes: Vec<(RequestId, Vec<String>)> = rx
            .try_iter()
            .filter_map(|c| match c {
                Command::RemoveReads { req, ids } => Some((req, ids)),
                _ => None,
            })
            .collect();
        assert_eq!(removes.len(), 2);
        assert!(st.batch.is_empty());
        // Success confirms; failure restores that record and toasts.
        let mut dialogs = super::super::DialogSink::default();
        let mut toasts = ToastSink::default();
        assert!(st.on_done(removes[0].0, &Ok(()), &c, &mut toasts, &mut dialogs));
        assert!(st.deleted.contains(&removes[0].1[0]));
        assert!(st.on_done(removes[1].0, &Err("busy".into()), &c, &mut toasts, &mut dialogs));
        assert_eq!(toasts.pushes[0].0.message, "Couldn\u{2019}t delete: busy");
        assert_eq!(toasts.pushes[0].0.id, "history-remove-error");
        assert!(st.items.iter().any(|r| r.id == removes[1].1[0]));
    }

    #[test]
    fn flush_commits_and_dismisses() {
        let (mut st, c, rx) = loaded(2, 2);
        let mut toasts = ToastSink::default();
        st.delete("r1", &mut toasts);
        let mut toasts = ToastSink::default();
        st.flush(&c, &mut toasts);
        assert_eq!(toasts.dismisses, vec![UNDO_TOAST_ID.to_string()]);
        assert_eq!(rx.try_iter().filter(|c| matches!(c, Command::RemoveReads { .. })).count(), 1);
        // A late refresh still holding the deleted row drops it.
        st.reload(&c);
        let (req, _, _) = last_query(&rx);
        assert!(st.on_page(req, &page(vec![rec("r0", NOW - 60_000.0), rec("r1", NOW - 120_000.0)], 2), &c));
        assert_eq!(st.items.len(), 1);
        assert_eq!(st.total, 1);
    }

    #[test]
    fn deleting_the_last_read_keeps_the_toolbar() {
        let (mut st, _c, _rx) = loaded(1, 1);
        let mut toasts = ToastSink::default();
        st.delete("r0", &mut toasts);
        let d = st.data(&Settings::default());
        assert_eq!(d.state, HistoryState::List);
        assert!(d.actions_visible);
    }

    #[test]
    fn load_more_appends_unique_and_refresh_supersedes() {
        let (mut st, c, rx) = loaded(50, 120);
        assert!(st.load_more(&c));
        let (lm, q, _) = last_query(&rx);
        assert_eq!(q.effective_offset(), 50);
        assert!(!st.load_more(&c), "refused while loading more");
        let more: Vec<ReadRecord> = (49..100).map(|i| rec(&format!("r{i}"), NOW - 60_000.0 * (i as f64 + 1.0))).collect();
        assert!(st.on_page(lm, &page(more, 120), &c));
        assert_eq!(st.items.len(), 100);
        assert_eq!(st.wanted, 100);
        // A refresh during a load-more supersedes it and fetches the enlarged window.
        assert!(st.load_more(&c));
        let (lm2, _, _) = last_query(&rx);
        st.start_refresh(&c);
        let (rf, q, _) = last_query(&rx);
        assert_eq!(q.effective_limit(), 150);
        assert!(!st.on_page(lm2, &page(vec![], 120), &c));
        assert!(st.loading_more);
        assert!(st.on_page(rf, &page((0..120).map(|i| rec(&format!("r{i}"), NOW - 1000.0 * i as f64)).collect(), 120), &c));
        assert!(!st.loading_more);
        assert_eq!(st.items.len(), 120);
    }

    #[test]
    fn big_windows_are_fetched_in_pages_of_500() {
        let (c, rx) = core();
        let mut st = Inner { active: true, wanted: 600, ..Inner::default() };
        st.start_refresh(&c);
        let (r1, q1, _) = last_query(&rx);
        assert_eq!(q1.effective_limit(), 500);
        let first: Vec<ReadRecord> = (0..500).map(|i| rec(&format!("r{i}"), NOW - i as f64)).collect();
        assert!(!st.on_page(r1, &page(first, 700), &c));
        let (r2, q2, _) = last_query(&rx);
        assert_eq!((q2.effective_offset(), q2.effective_limit()), (500, 100));
        let second: Vec<ReadRecord> = (500..600).map(|i| rec(&format!("r{i}"), NOW - i as f64)).collect();
        assert!(st.on_page(r2, &page(second, 700), &c));
        assert_eq!(st.items.len(), 600);
        assert_eq!(st.total, 700);
    }

    #[test]
    fn background_change_debounces_only_when_ready() {
        let (c, rx) = core();
        let mut st = Inner { active: true, ..Inner::default() };
        assert_eq!(st.history_changed(&c), Trigger::Now);
        let (req, _, _) = last_query(&rx);
        assert!(st.on_page(req, &page(vec![rec("a", NOW)], 1), &c));
        assert_eq!(st.history_changed(&c), Trigger::Debounce);
        assert_eq!(rx.try_iter().count(), 0);
    }

    #[test]
    fn favorite_results() {
        let (mut st, c, rx) = loaded(2, 2);
        st.toggle_favorite("r0", &c);
        assert!(st.items[0].favorite);
        let req = match rx.try_iter().last() {
            Some(Command::ToggleFavorite { req, .. }) => req,
            other => panic!("unexpected {other:?}"),
        };
        let mut toasts = ToastSink::default();
        assert!(st.on_favorite(req, &Ok(None), &mut toasts));
        assert!(st.items[0].favorite, "Ok(None) keeps the optimistic value");
        st.toggle_favorite("r0", &c);
        let req = match rx.try_iter().last() {
            Some(Command::ToggleFavorite { req, .. }) => req,
            other => panic!("unexpected {other:?}"),
        };
        assert!(!st.items[0].favorite, "the second click flips back at once");
        assert!(st.on_favorite(req, &Err("TypeError: nope".into()), &mut toasts));
        assert!(st.items[0].favorite, "a failure reverts to the value before the click");
        assert_eq!(toasts.pushes[0].0.message, "Couldn\u{2019}t update favorites: nope");
        assert_eq!(toasts.pushes[0].0.id, "history-favorite");
    }

    #[test]
    fn background_error_toasts_once_while_rows_exist() {
        let (mut st, c, rx) = loaded(2, 2);
        let mut toasts = ToastSink::default();
        st.reload(&c);
        let (req, _, _) = last_query(&rx);
        st.on_page(req, &Err("offline".into()), &c);
        st.check_error_toast(&mut toasts);
        st.check_error_toast(&mut toasts);
        assert_eq!(toasts.pushes.len(), 1);
        assert_eq!(toasts.pushes[0].0.message, "Couldn\u{2019}t load history: offline");
        assert_eq!(toasts.pushes[0].0.id, LOAD_TOAST_ID);
        assert_eq!(st.data(&Settings::default()).state, HistoryState::List, "old rows stay");
    }

    #[test]
    fn copy_and_replay_feedback() {
        let (mut st, c, _rx) = loaded(1, 1);
        let mut toasts = ToastSink::default();
        let mut dialogs = super::super::DialogSink::default();
        let req = c.request_id();
        st.copies.insert(req, true);
        assert!(!st.on_done(req, &Ok(()), &c, &mut toasts, &mut dialogs));
        assert_eq!(toasts.pushes[0].0.message, "Copied the saved preview");
        assert_eq!(toasts.pushes[0].0.id, "history-copy");
        let req = c.request_id();
        st.replays.insert(req, "r0".into());
        st.on_read_started(req, &ReadResult::not_cached("r0"), &mut toasts);
        assert_eq!(toasts.pushes[1].0.id, "replay-r0");
        assert_eq!(toasts.pushes[1].0.message, "The audio is no longer cached and the full text was not saved");
        let req = c.request_id();
        st.replays.insert(req, "r0".into());
        st.on_read_started(req, &ReadResult::failed_with_id("new", ReadErrorCode::Network, "x"), &mut toasts);
        assert_eq!(toasts.pushes.len(), 2, "a failed re-synthesis is toasted by the core");
    }

    #[test]
    fn rows_group_by_day_with_meta_badges_and_details() {
        let tz = tz();
        let mut a = rec("a", NOW + 3_600_000.0); // today 01:00
        a.listened_sec = 300.0;
        a.audio_duration_sec = 100.0;
        a.max_position_sec = Some(62.0);
        a.source_app = Some("Firefox".into());
        a.source_url = Some("https://www.example.com/a/b?q=1".into());
        a.source_title = Some("Window title".into());
        a.text_truncated = true;
        let mut b = rec("b", NOW + 1_800_000.0); // today 00:30
        b.cached = true;
        b.completed = true;
        b.error = Some("Only part of the text could be synthesized".into());
        let c = rec("c", NOW - 3_600_000.0); // yesterday 23:00
        let mut player = PlayerSnapshot::default();
        player.state.read_id = Some("b".into());
        player.state.status = PlayerStatus::Paused;
        let expanded: HashSet<String> = ["a".to_string()].into_iter().collect();
        let now = NOW + 4.0 * 3_600_000.0;
        let rows = row_views(&tz, &[a, b, c], now, &player, &expanded, &[]);
        assert_eq!(rows.len(), 5);
        assert!(rows[0].header);
        assert_eq!(rows[0].header_label, "Today");
        assert_eq!(rows[0].header_meta, "2 reads \u{00B7} 5m");
        assert_eq!(rows[0].id, "h-2026-10-17");
        let ra = &rows[1];
        assert_eq!(ra.meta, "Firefox \u{00B7} Brian \u{00B7} 1:40 \u{00B7} 3 hours ago");
        assert_eq!(ra.percent, "62%");
        assert!(!ra.complete);
        assert_eq!(ra.replay_label, "Replay");
        // A preview-only read without cached audio cannot be replayed (the tooltip says why).
        assert!(!ra.can_replay);
        assert_eq!(ra.replay_tooltip, "Audio no longer cached and full text wasn\u{2019}t saved");
        assert_eq!(rows[4].replay_tooltip, "Replay \u{00B7} uses credits");
        assert!(rows[4].can_replay);
        assert_eq!(ra.url_label, "example.com/a/b");
        assert_eq!(ra.notes, vec!["Only a short preview was saved for this read (full text storage is off).".to_string()]);
        assert_eq!(ra.facts[0], ("Chars".to_string(), "300".to_string(), "300 characters".to_string()));
        assert_eq!(ra.details[0].1, "Right-click chip \u{00B7} Firefox");
        assert_eq!(ra.details[1], ("Window".to_string(), "Window title".to_string(), "Window title".to_string()));
        assert_eq!(ra.details[2].1, "Brian \u{00B7} Flash v2.5");
        assert_eq!(ra.details[3].1, "1:40");
        let rb = &rows[2];
        assert_eq!(rb.badges, vec!["Paused".to_string(), "Failed".to_string()]);
        assert!(rb.complete);
        assert_eq!(rb.percent, "100%");
        assert_eq!(rb.replay_label, "Replay (free)");
        assert!(rb.details.is_empty(), "collapsed rows carry no details");
        assert_eq!(rows[3].header_label, "Yesterday");
        assert_eq!(rows[3].header_meta, "1 read");
        assert_eq!(rows[4].meta, "Right-click chip \u{00B7} Brian \u{00B7} ~0:20 \u{00B7} 5 hours ago");
    }

    #[test]
    fn preview_only_read_cannot_replay() {
        let mut r = rec("p", NOW);
        r.text_truncated = true;
        let v = read_row(&tz(), &r, NOW, &PlayerSnapshot::default(), false, &[]);
        assert!(!v.can_replay);
        assert_eq!(v.replay_tooltip, "Audio no longer cached and full text wasn\u{2019}t saved");
        let mut e = rec("e", NOW);
        e.text.clear();
        assert_eq!(read_row(&tz(), &e, NOW, &PlayerSnapshot::default(), false, &[]).title, "Empty read");
    }

    #[test]
    fn sync_model_updates_in_place() {
        let model: VecModel<String> = VecModel::default();
        let mut prev: Vec<(String, Image)> = Vec::new();
        let rows = |v: &[&str]| v.iter().map(|s| (s.to_string(), Image::default())).collect::<Vec<_>>();
        let make = |v: &String, _: &Image| v.clone();
        sync_model(&model, &mut prev, rows(&["h", "a", "b", "c"]), make);
        assert_eq!(model.iter().collect::<Vec<_>>(), ["h", "a", "b", "c"]);
        sync_model(&model, &mut prev, rows(&["h2", "a", "c"]), make);
        assert_eq!(model.iter().collect::<Vec<_>>(), ["h2", "a", "c"]);
        sync_model(&model, &mut prev, rows(&["h2", "a", "x", "c", "d"]), make);
        assert_eq!(model.iter().collect::<Vec<_>>(), ["h2", "a", "x", "c", "d"]);
        sync_model(&model, &mut prev, rows(&[]), make);
        assert_eq!(model.row_count(), 0);
    }

    #[test]
    fn web_url_rule() {
        assert!(is_web_url("HTTPS://x.com"));
        assert!(is_web_url("http://a"));
        assert!(!is_web_url("file:///c:/a.txt"));
        assert!(!is_web_url(""));
    }

    #[test]
    fn nothing_yet_wins_over_filters_on_an_empty_history() {
        // ui-main.md §4.5: #3 ("nothing yet") is decided before #4 ("no matches"), filters notwithstanding
        // (reachable from the sidebar search on an empty profile).
        let (c, rx) = core();
        let mut st = Inner { active: true, search: "foo".into(), search_raw: "foo".into(), ..Inner::default() };
        st.filters_changed(&c);
        let (req, q, grand) = last_query(&rx);
        assert_eq!(q.search.as_deref(), Some("foo"));
        assert!(grand);
        assert!(st.on_page(req, &Ok((HistoryPage { items: vec![], total: 0 }, 0)), &c));
        let d = st.data(&Settings::default());
        assert_eq!(d.state, HistoryState::NothingYet);
        assert_eq!(d.state_title, "Nothing read yet");
        assert!(!d.actions_visible);
        // With reads elsewhere, the filter is what excludes everything.
        st.reload(&c);
        let (req, _, _) = last_query(&rx);
        assert!(st.on_page(req, &Ok((HistoryPage { items: vec![], total: 0 }, 3)), &c));
        let d = st.data(&Settings::default());
        assert_eq!(d.state, HistoryState::NoMatches);
        assert_eq!(d.state_body, "Nothing matches \u{201C}foo\u{201D}.");
        assert!(d.actions_visible);
    }

    #[test]
    fn filtered_undo_of_the_last_read_keeps_the_toolbar() {
        // D-UM-12 still holds under a filter: while the batch is pending the page is not "nothing yet".
        let (c, rx) = core();
        let mut st = Inner { active: true, favorites_only: true, ..Inner::default() };
        st.filters_changed(&c);
        let (req, _, _) = last_query(&rx);
        assert!(st.on_page(req, &Ok((HistoryPage { items: vec![rec("r0", NOW)], total: 1 }, 1)), &c));
        let mut toasts = ToastSink::default();
        st.delete("r0", &mut toasts);
        assert_eq!(st.grand_total, 0);
        assert_eq!(st.state(), HistoryState::NoMatches);
        st.toast_removed(UNDO_TOAST_ID, &c);
        assert_eq!(st.state(), HistoryState::NothingYet, "committed: the history is empty");
    }

    // ── controller (through the `PageController` interface) ──

    use super::super::analytics::test_world::World;

    fn enter(c: &mut HistoryController, w: &mut World, intent: NavIntent) {
        c.on_enter(&mut w.cx(), &AppRoute::page(AppPage::History), &intent);
    }

    /// Answers the latest `QueryHistory` sent through `w` with `items` (and `total` as both totals).
    fn answer(c: &mut HistoryController, w: &mut World, items: Vec<ReadRecord>, total: usize) -> bool {
        let req = w
            .sent()
            .into_iter()
            .filter_map(|cmd| match cmd {
                Command::QueryHistory { req, .. } => Some(req),
                _ => None,
            })
            .last()
            .expect("a QueryHistory was sent");
        let update = UiUpdate::HistoryPage { req, result: Ok((HistoryPage { items, total }, total)) };
        c.on_update(&mut w.cx(), &update)
    }

    fn badges(d: &HistoryData, id: &str) -> Vec<String> {
        let row = d.rows.iter().find(|r| r.id == id).expect("row");
        row.badges.iter().map(|b| b.to_string()).collect()
    }

    fn playing(id: &str, status: PlayerStatus) -> PlayerSnapshot {
        let mut p = PlayerSnapshot::default();
        p.state.read_id = Some(id.into());
        p.state.status = status;
        p
    }

    #[test]
    fn player_badges_rebuild_after_reentry() {
        let mut w = World::new();
        let mut c = HistoryController::default();
        let now = 1_792_254_600_000.0; // World's `now_ms`
        let mut x = rec("x", now - 60_000.0);
        x.cached = true;
        enter(&mut c, &mut w, NavIntent::None);
        assert!(answer(&mut c, &mut w, vec![x.clone()], 1));
        let _ = c.build(&mut w.cx());
        // X plays while History is open.
        w.player = playing("x", PlayerStatus::Playing);
        let update = UiUpdate::Player(w.player.clone());
        assert!(c.on_update(&mut w.cx(), &update));
        assert_eq!(badges(&c.build(&mut w.cx()), "x"), ["Playing"]);
        assert!(!c.on_update(&mut w.cx(), &update), "same pair: no rebuild");
        // The user goes Home and X ends there (only the shown page receives `Player`).
        c.on_leave(&mut w.cx());
        w.player = PlayerSnapshot::default();
        enter(&mut c, &mut w, NavIntent::None);
        assert!(answer(&mut c, &mut w, vec![x], 1));
        assert_eq!(badges(&c.build(&mut w.cx()), "x"), ["Free replay"]);
        // Replay X: the same (X, Playing) pair as before the leave still counts as a change.
        w.player = playing("x", PlayerStatus::Loading);
        let update = UiUpdate::Player(w.player.clone());
        assert!(c.on_update(&mut w.cx(), &update));
        assert_eq!(badges(&c.build(&mut w.cx()), "x"), ["Playing"]);
        w.player = playing("x", PlayerStatus::Paused);
        let update = UiUpdate::Player(w.player.clone());
        assert!(c.on_update(&mut w.cx(), &update));
        assert_eq!(badges(&c.build(&mut w.cx()), "x"), ["Paused"]);
    }

    #[test]
    fn settled_search_dims_the_list_and_shows_the_applied_search() {
        let mut w = World::new();
        let mut c = HistoryController::default();
        let now = 1_792_254_600_000.0;
        enter(&mut c, &mut w, NavIntent::None);
        let items: Vec<ReadRecord> = (0..3).map(|i| rec(&format!("r{i}"), now - 60_000.0 * (i as f64 + 1.0))).collect();
        assert!(answer(&mut c, &mut w, items, 3));
        // Typing is debounced: nothing is sent yet and the applied search is unchanged.
        c.search_edited(&mut w.cx(), "  quarterly ");
        assert!(w.sent().is_empty());
        let d = c.build(&mut w.cx());
        assert_eq!((d.search.as_str(), d.dimmed), ("", false));
        // What the timer runs through `AppUi::history_cb` (which then rebuilds the page).
        c.search_settled(&mut w.cx());
        match w.sent().as_slice() {
            [Command::QueryHistory { query, with_grand_total: true, .. }] => {
                assert_eq!(query.search.as_deref(), Some("quarterly"))
            }
            other => panic!("unexpected {other:?}"),
        }
        let d = c.build(&mut w.cx());
        assert_eq!(d.state, HistoryState::List, "the old rows stay");
        assert!(d.dimmed, "dimmed and busy while loading (ui-main.md §4.5 #5)");
        assert_eq!(d.search, "quarterly");
        // Settling again with nothing new does nothing; a late settle after leaving is inert.
        c.search_settled(&mut w.cx());
        assert!(w.sent().is_empty());
        c.search_edited(&mut w.cx(), "other");
        c.on_leave(&mut w.cx());
        let _ = w.sent();
        c.search_settled(&mut w.cx());
        assert!(w.sent().is_empty());
    }

    #[test]
    fn background_refresh_fires_through_the_controller() {
        let mut w = World::new();
        let mut c = HistoryController::default();
        enter(&mut c, &mut w, NavIntent::None);
        assert!(answer(&mut c, &mut w, vec![rec("a", 1_792_254_000_000.0)], 1));
        // Ready → the change is debounced (nothing sent now).
        assert!(!c.on_update(&mut w.cx(), &UiUpdate::HistoryChanged));
        assert!(w.sent().is_empty());
        c.refresh_due(&mut w.cx());
        assert!(matches!(w.sent().as_slice(), [Command::QueryHistory { .. }]));
        c.on_leave(&mut w.cx());
        let _ = w.sent();
        c.refresh_due(&mut w.cx());
        assert!(w.sent().is_empty(), "inert once the page was left");
    }

    #[test]
    fn sidebar_search_on_an_empty_profile_shows_nothing_yet() {
        let mut w = World::new();
        let mut c = HistoryController::default();
        enter(&mut c, &mut w, NavIntent::Search(" foo ".into()));
        assert_eq!(w.fields.history_search_text.take(), Some(" foo ".to_string()));
        assert!(answer(&mut c, &mut w, vec![], 0));
        let d = c.build(&mut w.cx());
        assert_eq!(d.state, HistoryState::NothingYet);
        assert_eq!(d.state_button, "Paste & read");
    }
}
