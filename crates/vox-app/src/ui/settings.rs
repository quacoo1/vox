//! Settings controller (ui-settings-widget.md §2).
//!
//! - **Optimistic overlay** ([`Overlay`], pure): every write is `UpdateSettings{req: Some, patch}`; the pending patches
//!   are applied locally with `vox_core::settings::deep_merge` until **their own** `SettingsSaved{req}` arrives (an
//!   unrelated `UiUpdate::Settings` broadcast — tray checkbox, widget drag, CycleRate, key set — never drops the
//!   overlay early); the overlay ends when the pending set is empty; `Err` → revert + toast `Couldn’t save that
//!   setting` (id `settings-save-error`, `clean_error`). The status badge shows "Saving" while the set is non-empty,
//!   then "Saved automatically" (D-USW-10).
//! - Slider values arrive as Slint `float` (`f32`) and go through `vox_core::settings::quantize` before `patch_at`
//!   (`volume` and the 0..1 voice sliders step 0.01, `voice.speed` its step, `library.cacheLimitMB` 50 as an integer);
//!   `format_number(key, value)` (the `settings-format-number` pure callback) formats a slider's label live while it is
//!   dragged (`percent`, `{:.2}×` for `voice.speed`, `format_megabytes`).
//! - Every section's model (option builders in `vox_core::settings`; cache slider max = `max(5000, stored
//!   cacheLimitMB)` per D-C-2; separate busy flags per Library action per D-USW-16), generic `set-bool/number/choice` →
//!   JSON patches (`settings::patch_at`; model change → `model_change_patch` in the same write; rate / volume also
//!   `Command::Player(Rate|Volume)`), the API-key form, the remove-key dialog (`remove-key`), the Library clear-history
//!   dialog (`library-clear-history`: `Clear history?`, success toast id `library-history`, stays open on failure), the
//!   hotkey rows (recording state, 6 s rejection message, persistent warnings via `hotkey::row_warning`), excluded apps
//!   (`add_excluded_app`; the input's text is the main window's in-out `settings-exclude-draft`, set to `""` after a
//!   successful add), the retention guard ([`RetentionGuard`]), cache stats (re-measured 2.5 s after the last history
//!   change), the port field ([`PortField`]; text = in-out `settings-port-draft`), the bridge badge re-check window,
//!   copy chips (1.4 s "Copied"), About (version, runtime label, **build hash**; links via `OpenExternal` without a
//!   req — failures ignored), Appearance (`copy::APPEARANCE_INTRO`, `copy::THEME_NOTES`).
//! - **Re-check window** (integrations.md §1.9): entered when the bridge toggle changes, a different port is
//!   committed, or the Explorer toggle changes; left on the first `UiUpdate::AppInfo` received after entering (the core
//!   publishes one at the end of every bridge / Explorer / launch-at-login reaction) or after `BRIDGE_RECHECK_MAX_MS`.
//!   The spec's delayed 700/900/1200 ms re-queries are replaced by that push.
//! - **Stateful controls** (ARCHITECTURE.md §7.1): sections re-assert their toggles/selects/sliders from `data` in a
//!   `changed data` handler; the controller bumps `SettingsData.revision` on every rebuild that must override what the
//!   user did (revert, Reset, retention cancel, a change made elsewhere).
//! - Scroll spy: Slint-side (`SettingsPage.active-section`, ARCHITECTURE.md §7.4); Rust only sets `scroll-to`/nonce.
//! - Account and the onboarding key step show `AppInfo.api_key_notice` (failed key migration). Triggers shows the hook
//!   notice while `AppInfo.native_hook_available == false` (`copy::HOOK_UNAVAILABLE_*`) and the media-keys row without
//!   any restart copy (applies live). Deep links scroll to `settings/<section>` (nonce re-announces).
//!
//! Tests (UI-free structs with injected time): [`Overlay`] with a failed first update; [`RetentionGuard`] (7→30→7
//! within 450 ms gives one count; back to the stored value writes nothing; a stricter value with count 0 saves
//! silently; a count failure reverts and toasts `Couldn’t check your history`, id `library-retention`); [`PortField`]
//! (`00080` is an error; Escape restores; blur with the stored value writes nothing); [`HotkeyRows`] (a reserved
//! chord keeps the old shortcut with the 6 s message; a stored reserved chord shows the persistent message); a model
//! switch resets the language in the same write.
//!
//! ## Implementation notes (integration contract of this module)
//!
//! - **Callbacks → methods**: every `settings-*` callback of `MainWindow` maps to the method of the same name
//!   (`settings-save-key(k)` → [`SettingsController::save_key`], …). The recorder's three callbacks are
//!   `settings-hotkey-start(action)` → [`SettingsController::hotkey_start`], `settings-hotkey-stop()` →
//!   [`SettingsController::hotkey_stop`], `settings-hotkey-clear(action)` → [`SettingsController::hotkey_clear`];
//!   `settings-export-history()` → [`SettingsController::export_history`] with the main window's HWND as `owner`.
//! - **Answers** this controller handles (also when the page is not active: `AppUi` routes answers to every page):
//!   `SettingsSaved`, `ApiKey`, `Done` (remove key, Get a key, clear history, copy chips), `CacheStats`, `Exported`,
//!   `ExpiringCount`, `ReadStarted` (Test voice), `HotkeysResumed`. Entering the page resets the page-local UI state
//!   (form, errors, warnings, the retention guard, the re-check window — the old app re-mounted the page) but keeps
//!   the requests in flight and the overlay, whose answers still arrive.
//! - **Deadlines** (retention settle 450 ms, cache re-measure 2.5 s, re-check window 4 s, "Copied" 1.4 s, hotkey
//!   rejection 6 s) are processed with `cx.now_ms` at the start of every `on_update` ([`SettingsController`]'s poll),
//!   and expiries are evaluated again in `build`. One `DeadlineTimer` makes sure an update arrives at the next
//!   deadline: when it fires it sends `Command::QueryAppInfo` (a "rebuild tick", see `hotkey_recorder` module docs) —
//!   the controller interface has no rebuild hook for timers. The `AppInfo` that answers is marked dirty.
//! - The re-check window ends on the first `AppInfo` received after the change's own `SettingsSaved{req}`: the core
//!   publishes `Settings`, `SettingsSaved{req}`, then the reaction's `AppInfo` (core `update_settings`), so a rebuild
//!   tick sent before the change can never end it early. A failed save ends it at once.
//! - The voice list (current voice tags) and the models are fetched on entry and whenever `hasApiKey` flips, only with
//!   a key or in demo mode (spec §2.1); without either the model select offers only the current id.

use std::sync::Arc;

use serde_json::{json, Value};
use slint::{ModelRc, SharedString, VecModel};
use vox_core::app::{AppPage, AppRoute, Toast};
use vox_core::cache::CacheStats;
use vox_core::constants::{
    APP_NAME, BRIDGE_RECHECK_MAX_MS, CACHE_REMEASURE_MS, COPY_CONFIRM_MS, ELEVENLABS_API_KEYS_URL, ELEVENLABS_BILLING_URL,
    ELEVENLABS_DASHBOARD_URL, ELEVENLABS_DOCS_URL, HOTKEY_REJECT_MESSAGE_MS, RETENTION_SETTLE_MS,
};
use vox_core::format;
use vox_core::history::ReadSource;
use vox_core::hotkey::HotkeyAction;
use vox_core::js::{js_trim, number_to_string, to_fixed};
use vox_core::player::PlayerCommand;
use vox_core::read::{ReadResult, ReadTextOptions};
use vox_core::settings::{self as vs, OutputFormat, Settings, ThemeMode, WidgetPosition};
use vox_core::voices::{TtsModel, Voice};

use super::art::ArtSize;
use super::convert::{credits_data, keycaps, strings, CreditsSurface};
use super::generated::{
    AboutData, AccountData, AccountState, AppearanceData, BadgeData, BadgeTone, ChoiceItem, HotkeyRowData, IntegrationsData,
    LibraryData, PlaybackData, SettingsData, TriggersData, VoiceModelData, WidgetSettingsData,
};
use super::hotkey_recorder::{self, RecorderEvent};
use super::shell::DeadlineTimer;
use super::{DialogSpec, NavIntent, PageController, UiCtx};
use crate::command::{Command, RequestId, RevealTarget};
use crate::update::UiUpdate;

// ───────────────────────────── copy ─────────────────────────────

const KEY_LEAD: &str = "Vox speaks with your own ElevenLabs account. Paste an API key to start reading.";
const KEY_HINT: &str = "Needs Text to Speech access. Add \u{201C}User: read\u{201D} to see your credits here.";
const KEY_EMPTY: &str = "Paste your ElevenLabs API key first.";
const KEY_REJECTED: &str = "ElevenLabs rejected that key.";
const MODEL_FALLBACK_DESCRIPTION: &str = "Which ElevenLabs model synthesizes speech.";
const PORT_INVALID: &str = "Use a port between 1024 and 65535.";
const DEFAULT_CACHE_SLIDER_MAX: u32 = 5000;

/// Dotted keys the generic `set-bool` callback may write.
const BOOL_KEYS: [&str; 20] = [
    "voice.useSpeakerBoost",
    "text.skipUrls",
    "text.skipEmails",
    "triggers.rightClickChip",
    "triggers.selectionPopup",
    "triggers.hotkeysEnabled",
    "triggers.mediaKeys",
    "triggers.restoreClipboard",
    "triggers.browserExtension",
    "triggers.explorerMenu",
    "widget.enabled",
    "widget.compact",
    "widget.highlightWords",
    "widget.autoHide",
    "app.launchAtLogin",
    "app.startMinimized",
    "app.closeToTray",
    "library.saveHistory",
    "library.storeText",
    "library.cacheAudio",
];

// ───────────────────────────── optimistic overlay ─────────────────────────────

/// The optimistic settings overlay.
#[derive(Debug, Default)]
pub struct Overlay {
    pending: Vec<(RequestId, serde_json::Value)>,
}

impl Overlay {
    /// Records a patch sent with `req` (the view applies it at once).
    pub fn push(&mut self, req: RequestId, patch: serde_json::Value) {
        self.pending.push((req, patch));
    }
    /// The settings to show: `stored` with every pending patch merged in order.
    pub fn view(&self, stored: &vox_core::settings::Settings) -> vox_core::settings::Settings {
        let mut out = stored.clone();
        for (_, patch) in &self.pending {
            out = out.apply_patch(patch);
        }
        out
    }
    /// `SettingsSaved{req, result}`: drops that patch; `Some(message)` = the error toast to show.
    pub fn on_saved(&mut self, req: RequestId, result: &Result<(), String>) -> Option<String> {
        let before = self.pending.len();
        self.pending.retain(|(r, _)| *r != req);
        if self.pending.len() == before {
            return None;
        }
        result.as_ref().err().map(|e| format::clean_error(e))
    }
    /// "Saving" badge.
    pub fn saving(&self) -> bool {
        !self.pending.is_empty()
    }
    /// Whether `req` is one of the pending writes.
    pub fn contains(&self, req: RequestId) -> bool {
        self.pending.iter().any(|(r, _)| *r == req)
    }
    /// Forgets every pending write (tests: settle the overlay without answers). The page keeps its pending writes
    /// across a leave and re-entry, so a late failure still reverts the control and toasts.
    #[cfg(test)]
    pub fn clear(&mut self) {
        self.pending.clear();
    }
}

// ───────────────────────────── retention guard ─────────────────────────────

/// What the retention guard asks the controller to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetentionStep {
    Nothing,
    /// Send `CountExpiring{req, days}` (the 450 ms settle elapsed).
    Count { days: u32 },
    /// Save the value without asking.
    Save { days: u32 },
    /// Open the `retention` dialog (`Delete N reads older than …?`).
    Confirm { days: u32, count: usize },
    /// Revert the select to the stored value (+ toast when `error` is set).
    Revert { error: Option<String> },
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum RetentionPhase {
    /// Waiting for the choice to settle (last change at `at`).
    Settling { days: u32, at: f64 },
    /// `CountExpiring` is out.
    Counting { days: u32 },
    /// The `retention` dialog is open.
    Confirming { days: u32 },
}

/// The retention guard (ui-settings-widget.md §2.8), pure with injected milliseconds.
#[derive(Debug, Default)]
pub struct RetentionGuard {
    phase: Option<RetentionPhase>,
}

impl RetentionGuard {
    /// The user picked `days` at `now_ms` (stored value `stored`).
    pub fn on_choice(&mut self, days: u32, stored: u32, now_ms: f64) -> RetentionStep {
        // Every choice cancels a pending check / confirmation first; the stored value is always the baseline.
        self.phase = None;
        if days == stored {
            return RetentionStep::Nothing;
        }
        if !vs::is_stricter_retention(days, stored) {
            return RetentionStep::Save { days };
        }
        self.phase = Some(RetentionPhase::Settling { days, at: now_ms });
        RetentionStep::Nothing
    }
    /// The settle timer fired.
    pub fn on_settle(&mut self, now_ms: f64) -> RetentionStep {
        match self.phase {
            Some(RetentionPhase::Settling { days, at }) if now_ms - at >= RETENTION_SETTLE_MS as f64 - 1.0 => {
                self.phase = Some(RetentionPhase::Counting { days });
                RetentionStep::Count { days }
            }
            _ => RetentionStep::Nothing,
        }
    }
    /// `ExpiringCount` arrived.
    pub fn on_count(&mut self, result: &Result<usize, String>) -> RetentionStep {
        let Some(RetentionPhase::Counting { days }) = self.phase else { return RetentionStep::Nothing };
        match result {
            Ok(0) => {
                self.phase = None;
                RetentionStep::Save { days }
            }
            Ok(count) => {
                self.phase = Some(RetentionPhase::Confirming { days });
                RetentionStep::Confirm { days, count: *count }
            }
            Err(e) => {
                self.phase = None;
                RetentionStep::Revert { error: Some(format::clean_error(e)) }
            }
        }
    }
    /// The dialog was confirmed / cancelled.
    pub fn on_dialog(&mut self, confirmed: bool) -> RetentionStep {
        let Some(RetentionPhase::Confirming { days }) = self.phase else { return RetentionStep::Nothing };
        self.phase = None;
        if confirmed {
            RetentionStep::Save { days }
        } else {
            RetentionStep::Revert { error: None }
        }
    }
    /// The value the select shows while a stricter choice is being checked.
    pub fn pending_days(&self) -> Option<u32> {
        match self.phase? {
            RetentionPhase::Settling { days, .. } | RetentionPhase::Counting { days } | RetentionPhase::Confirming { days } => {
                Some(days)
            }
        }
    }
    /// When the settle timer is due.
    pub fn settle_deadline(&self) -> Option<f64> {
        match self.phase? {
            RetentionPhase::Settling { at, .. } => Some(at + RETENTION_SETTLE_MS as f64),
            _ => None,
        }
    }
    /// Drops whatever is pending (page leave); true when something was.
    pub fn cancel(&mut self) -> bool {
        self.phase.take().is_some()
    }
}

// ───────────────────────────── port field ─────────────────────────────

/// What a port-field event produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortCommit {
    Nothing,
    /// Write `server.port`.
    Save(u16),
    /// Show `Use a port between 1024 and 65535.`
    Invalid,
}

/// The port field (ui-settings-widget.md §2.9), pure.
#[derive(Debug, Default)]
pub struct PortField {
    pub draft: String,
    pub error: bool,
}

impl PortField {
    /// Every edit: strip non-digits, cut to 5; returns the draft to write back to `settings-port-draft`.
    pub fn edited(&mut self, text: &str) -> String {
        self.draft = text.chars().filter(|c| c.is_ascii_digit()).take(5).collect();
        self.error = false;
        self.draft.clone()
    }
    /// Blur / Enter.
    pub fn commit(&mut self, text: &str, stored: u16) -> PortCommit {
        let t = text.trim();
        if t == stored.to_string() {
            self.error = false;
            return PortCommit::Nothing;
        }
        match vs::parse_port(t) {
            None => {
                // An invalid draft is not reverted on blur: it stays with the error until edited or Escape.
                self.draft = t.to_string();
                self.error = true;
                PortCommit::Invalid
            }
            Some(p) if p == stored => {
                // Leading zeros of the stored port: nothing to save, show the stored spelling.
                self.draft = stored.to_string();
                self.error = false;
                PortCommit::Nothing
            }
            Some(p) => {
                self.draft = p.to_string();
                self.error = false;
                PortCommit::Save(p)
            }
        }
    }
    /// Escape: back to the stored port (returns the draft).
    pub fn revert(&mut self, stored: u16) -> String {
        self.draft = stored.to_string();
        self.error = false;
        self.draft.clone()
    }
    /// The stored port changed (even mid-edit): reset the draft (returns it).
    pub fn on_stored_change(&mut self, stored: u16) -> String {
        self.revert(stored)
    }
}

// ───────────────────────────── hotkey rows ─────────────────────────────

/// The hotkey rows' messages (6 s rejection, 1.8 s hint, persistent warnings), pure with injected time.
#[derive(Debug, Default)]
pub struct HotkeyRows {
    rejection: Option<(vox_core::hotkey::HotkeyAction, String, f64)>,
}

impl HotkeyRows {
    /// The warning to show under `action`'s row at `now_ms` (rejection first, else `hotkey::row_warning`).
    pub fn warning(&self, settings: &vox_core::settings::Settings, action: vox_core::hotkey::HotkeyAction, now_ms: f64) -> Option<String> {
        if let Some((a, message, until)) = &self.rejection {
            if *a == action && now_ms < *until {
                return Some(message.clone());
            }
        }
        vox_core::hotkey::row_warning(&settings.triggers.hotkeys, action)
    }
    /// A reserved chord was recorded: keep the old shortcut, show `reserved_rejection` for 6 s.
    pub fn reject(&mut self, action: vox_core::hotkey::HotkeyAction, accel: &str, now_ms: f64) {
        // Only one rejection at a time: a rejection on another row replaces it.
        self.rejection = Some((action, vox_core::hotkey::reserved_rejection(accel), now_ms + HOTKEY_REJECT_MESSAGE_MS as f64));
    }
    /// A shortcut was saved (any row): the rejection message goes at once.
    pub fn clear(&mut self) {
        self.rejection = None;
    }
    /// When the rejection message ends (if one shows at `now_ms`).
    pub fn deadline(&self, now_ms: f64) -> Option<f64> {
        self.rejection.as_ref().map(|(_, _, until)| *until).filter(|u| *u > now_ms)
    }
    /// Drops a rejection message whose time is up; true when one was dropped (the page must be rebuilt).
    pub fn expire(&mut self, now_ms: f64) -> bool {
        if self.rejection.as_ref().is_some_and(|(_, _, until)| *until <= now_ms) {
            self.rejection = None;
            return true;
        }
        false
    }
}

// ───────────────────────────── small helpers ─────────────────────────────

/// A fetched list that keeps its previous data through loading and errors (spec §1.2).
#[derive(Debug, Clone, Default)]
struct Loadable<T> {
    data: Option<T>,
    loading: bool,
    error: Option<String>,
}

impl<T> Loadable<T> {
    fn start(&mut self) {
        self.loading = true;
        self.error = None;
    }
    fn settle(&mut self, result: Result<T, String>) {
        self.loading = false;
        match result {
            Ok(v) => {
                self.data = Some(v);
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
    }
    /// The key went away: drop the data (the only transition that does).
    fn reset(&mut self) {
        self.data = None;
        self.loading = false;
        self.error = None;
    }
}

/// The bridge badge's re-check window (module docs).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Recheck {
    req: RequestId,
    /// The change's own `SettingsSaved{req}` arrived: the next `AppInfo` ends the window.
    saved: bool,
    until: f64,
}

fn choice(value: impl Into<SharedString>, label: impl Into<SharedString>, accessible: impl Into<SharedString>) -> ChoiceItem {
    ChoiceItem { value: value.into(), label: label.into(), accessible_label: accessible.into() }
}

fn choice_model(items: Vec<ChoiceItem>) -> ModelRc<ChoiceItem> {
    ModelRc::new(VecModel::from(items))
}

fn badge(text: impl Into<SharedString>, tone: BadgeTone) -> BadgeData {
    BadgeData { text: text.into(), tone }
}

fn index_of<T>(items: &[T], pred: impl Fn(&T) -> bool) -> i32 {
    items.iter().position(pred).map(|i| i as i32).unwrap_or(0)
}

/// `{n} read` / `{n} reads` with the count locale-grouped.
fn reads_label(n: usize) -> (String, &'static str) {
    (format::format_number(n as f64), if n == 1 { "" } else { "s" })
}

/// `80%` for a 0..1 slider.
fn percent_label(v: f64) -> String {
    format::percent(v)
}

/// `1.08×` (`v.toFixed(2)×`).
fn speed_label(v: f64) -> String {
    format!("{}\u{00D7}", to_fixed(v, 2))
}

/// A key's slider step (0 = not a slider).
fn slider_step(key: &str) -> f64 {
    match key {
        "volume" | "voice.stability" | "voice.similarityBoost" | "voice.style" | "voice.speed" => 0.01,
        "library.cacheLimitMB" => 50.0,
        _ => 0.0,
    }
}

/// The current voice's tags (spec §2.3): `[voiceKind, ...voiceTags(v, 4)]`, a placeholder while the list loads, else
/// `ElevenLabs voice`. Returns (tags, loading).
fn current_voice_tags(voice_id: &str, voices: Option<&[Voice]>, loading: bool) -> (Vec<String>, bool) {
    if let Some(v) = voices.and_then(|list| list.iter().find(|v| v.voice_id == voice_id)) {
        let mut tags = vec![vox_core::voices::voice_kind(v)];
        tags.extend(vox_core::voices::voice_tags(v, 4, &[]));
        tags.retain(|t| !t.trim().is_empty());
        return (tags, false);
    }
    if loading {
        return (Vec::new(), true);
    }
    (vec!["ElevenLabs voice".to_string()], false)
}

// ───────────────────────────── controller ─────────────────────────────

#[derive(Default)]
pub struct SettingsController {
    pub overlay: Overlay,
    pub retention: RetentionGuard,
    pub port: PortField,
    pub hotkeys: HotkeyRows,
    revision: i32,
    /// The page is on screen.
    active: bool,
    // ── Account ──
    key_form_open: bool,
    key_req: Option<RequestId>,
    key_error: Option<String>,
    /// The warning returned by the last successful save (restricted key).
    key_warning: Option<String>,
    remove_req: Option<RequestId>,
    get_key_req: Option<RequestId>,
    // ── Voice & model ──
    voices: Loadable<Arc<Vec<Voice>>>,
    models: Loadable<()>,
    test_req: Option<RequestId>,
    // ── Triggers ──
    exclude_error: Option<String>,
    // ── Library ──
    stats: Option<CacheStats>,
    stats_error: Option<String>,
    stats_req: Option<RequestId>,
    remeasure_at: Option<f64>,
    clear_cache_req: Option<RequestId>,
    export_req: Option<RequestId>,
    clear_history_req: Option<RequestId>,
    count_req: Option<RequestId>,
    // ── Integrations ──
    recheck: Option<Recheck>,
    copied: Option<(String, f64)>,
    copy_reqs: Vec<(RequestId, String)>,
    // ── change tracking ──
    last_port: Option<u16>,
    last_has_key: Option<bool>,
    timer: DeadlineTimer,
}

impl SettingsController {
    /// The settings the page shows (stored + pending writes).
    fn view(&self, cx: &UiCtx) -> Settings {
        self.overlay.view(cx.settings)
    }

    fn demo(cx: &UiCtx) -> bool {
        cx.demo || cx.app_info.demo_mode
    }

    /// Sends one optimistic write.
    fn update(&mut self, cx: &mut UiCtx, patch: Value) -> RequestId {
        let req = cx.core.request_id();
        self.overlay.push(req, patch.clone());
        cx.core.send(Command::UpdateSettings { req: Some(req), patch });
        req
    }

    fn toast(cx: &mut UiCtx, toast: Toast) {
        cx.toasts.push(toast, None);
    }

    /// Voices and models: fetched with a key or in demo mode; dropped without (spec §2.1).
    fn fetch_catalog(&mut self, cx: &mut UiCtx) {
        if cx.settings.has_api_key || Self::demo(cx) {
            self.voices.start();
            self.models.start();
            cx.core.send(Command::FetchVoices { refresh: false });
            cx.core.send(Command::FetchModels { refresh: false });
        } else {
            self.voices.reset();
            self.models.reset();
        }
    }

    fn query_stats(&mut self, cx: &mut UiCtx) {
        let req = cx.core.request_id();
        self.stats_req = Some(req);
        cx.core.send(Command::QueryCacheStats { req });
    }

    /// The stored port changed (any source): the field follows, even mid-edit.
    fn sync_port(&mut self, cx: &mut UiCtx) {
        let port = self.view(cx).server.port;
        if self.last_port != Some(port) {
            self.last_port = Some(port);
            cx.fields.settings_port_draft = Some(self.port.on_stored_change(port));
        }
    }

    fn enter_recheck(&mut self, req: RequestId, now_ms: f64) {
        self.recheck = Some(Recheck { req, saved: false, until: now_ms + BRIDGE_RECHECK_MAX_MS as f64 });
    }

    fn rechecking(&self, now_ms: f64) -> bool {
        self.recheck.is_some_and(|r| now_ms < r.until)
    }

    /// The next time something is due (module docs "Deadlines").
    fn next_deadline(&self, now_ms: f64) -> Option<f64> {
        [
            self.retention.settle_deadline(),
            self.remeasure_at,
            self.recheck.map(|r| r.until),
            self.copied.as_ref().map(|(_, until)| *until),
            self.hotkeys.deadline(now_ms),
        ]
        .into_iter()
        .flatten()
        .reduce(f64::min)
    }

    /// Arms the deadline timer: when it fires it asks the core for an `AppInfo` (a rebuild tick), whose `on_update`
    /// processes the due work.
    fn arm(&mut self, cx: &UiCtx) {
        if !self.active {
            self.timer.cancel();
            return;
        }
        let core = cx.core.clone();
        // A few ms of slack so the tick lands after the deadline.
        let at = self.next_deadline(cx.now_ms).map(|d| d + 5.0);
        self.timer.arm(at, cx.now_ms, move || hotkey_recorder::request_rebuild(&core));
    }

    /// Recorder events and due deadlines (start of every update). True when the page must be rebuilt (a recorder
    /// event, an ended "Copied" chip, rejection message or bridge recheck, a retention step), whatever update
    /// carried the poll: the deadline timer is re-armed for later work only, so the rebuild must not wait for it.
    fn poll(&mut self, cx: &mut UiCtx) -> bool {
        let now = cx.now_ms;
        let mut dirty = false;
        for ev in hotkey_recorder::take_events() {
            dirty = true;
            match ev {
                RecorderEvent::Saved { action, req, patch } => {
                    tracing::debug!(target: "ui", "hotkey recorder saved {}", action.id());
                    self.overlay.push(req, patch);
                    self.hotkeys.clear();
                }
                RecorderEvent::Rejected { action, accel } => self.hotkeys.reject(action, &accel, now),
            }
        }
        dirty |= self.hotkeys.expire(now);
        if self.retention.settle_deadline().is_some_and(|d| d <= now + 1.0) {
            let step = self.retention.on_settle(now);
            self.apply_retention(cx, step);
            dirty = true;
        }
        if self.remeasure_at.is_some_and(|d| d <= now) {
            self.remeasure_at = None;
            if self.active {
                self.query_stats(cx);
            }
        }
        if self.recheck.is_some_and(|r| r.until <= now) {
            self.recheck = None;
            cx.core.send(Command::QueryAppInfo);
            dirty = true;
        }
        if self.copied.as_ref().is_some_and(|(_, until)| *until <= now) {
            self.copied = None;
            dirty = true;
        }
        dirty
    }

    fn apply_retention(&mut self, cx: &mut UiCtx, step: RetentionStep) {
        match step {
            RetentionStep::Nothing => {}
            RetentionStep::Count { days } => {
                let req = cx.core.request_id();
                self.count_req = Some(req);
                cx.core.send(Command::CountExpiring { req, days });
            }
            RetentionStep::Save { days } => {
                self.update(cx, json!({ "library": { "retentionDays": days } }));
            }
            RetentionStep::Confirm { days, count } => {
                let (n, s) = reads_label(count);
                cx.dialogs.open(DialogSpec {
                    id: "retention".into(),
                    owner: AppPage::Settings,
                    title: format!("Delete {n} read{s} older than {}?", vs::retention_label(days)),
                    body: "Their saved audio goes too. Favorites are always kept. This can\u{2019}t be undone.".into(),
                    confirm_label: format!("Delete {n} read{s}"),
                    cancel_label: "Cancel".into(),
                    danger: true,
                    busy: false,
                });
            }
            RetentionStep::Revert { error } => {
                self.revision = self.revision.wrapping_add(1);
                if let Some(e) = error {
                    Self::toast(cx, Toast::error(e).with_title("Couldn\u{2019}t check your history").with_id("library-retention"));
                }
            }
        }
    }

    /// A fresh page (the old app re-mounted Settings on every visit): page-local UI state starts over. Requests still
    /// in flight are kept: their answers reach this controller on any page (`AppUi` routes answers to every page), so
    /// their toasts and busy flags stay right; so does the overlay (every pending write gets its `SettingsSaved`).
    fn reset_page_state(&mut self) {
        self.retention.cancel();
        self.count_req = None;
        self.port.error = false;
        self.hotkeys.clear();
        if self.key_req.is_none() {
            self.key_form_open = false;
            self.key_error = None;
        }
        self.key_warning = None;
        self.exclude_error = None;
        self.remeasure_at = None;
        self.recheck = None;
        self.copied = None;
    }

    // ───────────── callbacks ─────────────

    /// A section index click: the page scrolls itself (Slint scroll spy, ARCHITECTURE.md §7.4); nothing to do here.
    pub fn section_selected(&mut self, cx: &mut UiCtx, section: &str) {
        let _ = (cx, section);
    }
    pub fn set_bool(&mut self, cx: &mut UiCtx, key: &str, value: bool) {
        if !BOOL_KEYS.contains(&key) {
            tracing::debug!(target: "ui", "ignored an unknown settings toggle: {key}");
            return;
        }
        let req = self.update(cx, vs::patch_at(key, Value::Bool(value)));
        if key == "triggers.browserExtension" || key == "triggers.explorerMenu" {
            self.enter_recheck(req, cx.now_ms);
        }
        self.arm(cx);
    }
    /// A slider commit (`f32` → `quantize`).
    pub fn set_number(&mut self, cx: &mut UiCtx, key: &str, value: f32) {
        let step = slider_step(key);
        if step <= 0.0 || !value.is_finite() {
            tracing::debug!(target: "ui", "ignored an unknown settings slider: {key}");
            return;
        }
        let v = vs::quantize(value, step);
        let view = self.view(cx);
        match key {
            "volume" => {
                let v = v.clamp(0.0, 1.0);
                if v == view.volume {
                    return;
                }
                self.update(cx, json!({ "volume": v }));
                cx.core.send(Command::Player(PlayerCommand::Volume { volume: v }));
            }
            "library.cacheLimitMB" => {
                let mb = v.clamp(50.0, 10_000.0).round() as u32;
                if mb == view.library.cache_limit_mb {
                    return;
                }
                self.update(cx, json!({ "library": { "cacheLimitMB": mb } }));
            }
            _ => {
                let current = match key {
                    "voice.stability" => view.voice.stability,
                    "voice.similarityBoost" => view.voice.similarity_boost,
                    "voice.style" => view.voice.style,
                    _ => view.voice.speed,
                };
                if v == current {
                    return;
                }
                self.update(cx, vs::patch_at(key, json!(v)));
            }
        }
    }
    pub fn set_choice(&mut self, cx: &mut UiCtx, key: &str, value: &str) {
        let view = self.view(cx);
        match key {
            "modelId" => {
                if value.is_empty() {
                    return;
                }
                let patch = vs::model_change_patch(cx.models, value, &view.language_code);
                self.update(cx, patch);
            }
            "languageCode" => {
                self.update(cx, json!({ "languageCode": value }));
            }
            "outputFormat" => {
                if OutputFormat::parse(value).is_some() {
                    self.update(cx, json!({ "outputFormat": value }));
                }
            }
            "playbackRate" => {
                // Re-selecting the current rate re-sends the live command too (spec §1.8 Segmented).
                let Some(rate) = value.parse::<f64>().ok().filter(|r| r.is_finite() && *r > 0.0) else { return };
                self.update(cx, json!({ "playbackRate": rate }));
                cx.core.send(Command::Player(PlayerCommand::Rate { rate }));
            }
            "text.maxChars" => {
                if let Ok(n) = value.parse::<u32>() {
                    self.update(cx, json!({ "text": { "maxChars": n } }));
                }
            }
            "widget.position" => {
                if WidgetPosition::parse(value).is_some() {
                    self.update(cx, json!({ "widget": { "position": value } }));
                }
            }
            "app.theme" => {
                if ThemeMode::parse(value).is_some() {
                    self.update(cx, json!({ "app": { "theme": value } }));
                }
            }
            "library.retentionDays" => {
                let Ok(days) = value.parse::<u32>() else { return };
                // A new choice cancels a pending check (its answer is dropped).
                self.count_req = None;
                let step = self.retention.on_choice(days, view.library.retention_days, cx.now_ms);
                self.apply_retention(cx, step);
                self.arm(cx);
            }
            _ => tracing::debug!(target: "ui", "ignored an unknown settings choice: {key}"),
        }
    }
    /// The `settings-format-number` pure callback: a slider's live label.
    pub fn format_number(&self, key: &str, value: f32) -> String {
        let step = slider_step(key);
        if step <= 0.0 || !value.is_finite() {
            return String::new();
        }
        let v = vs::quantize(value, step);
        match key {
            "voice.speed" => speed_label(v),
            "library.cacheLimitMB" => format::format_megabytes(v.clamp(0.0, u32::MAX as f64).round() as u32),
            _ => percent_label(v),
        }
    }
    pub fn save_key(&mut self, cx: &mut UiCtx, key: &str) {
        if self.key_req.is_some() {
            return;
        }
        let key = js_trim(key);
        if key.is_empty() {
            self.key_error = Some(KEY_EMPTY.into());
            return;
        }
        self.key_error = None;
        let req = cx.core.request_id();
        self.key_req = Some(req);
        cx.core.send(Command::SetApiKey { req, key: key.to_string() });
    }
    pub fn key_edited(&mut self, cx: &mut UiCtx) {
        let _ = cx;
        self.key_error = None;
    }
    pub fn open_key_form(&mut self, cx: &mut UiCtx) {
        self.key_form_open = true;
        self.key_error = None;
        cx.fields.settings_key_draft = Some(String::new());
    }
    pub fn cancel_key_form(&mut self, cx: &mut UiCtx) {
        // Cancel is disabled while saving.
        if self.key_req.is_some() {
            return;
        }
        self.key_form_open = false;
        self.key_error = None;
        cx.fields.settings_key_draft = Some(String::new());
    }
    pub fn remove_key(&mut self, cx: &mut UiCtx) {
        if !cx.settings.has_api_key {
            return;
        }
        cx.dialogs.open(DialogSpec {
            id: "remove-key".into(),
            owner: AppPage::Settings,
            title: "Remove API key?".into(),
            body: format!("{APP_NAME} won\u{2019}t be able to read anything until you add a key again. History and cached audio stay."),
            confirm_label: "Remove key".into(),
            cancel_label: "Cancel".into(),
            danger: true,
            busy: self.remove_req.is_some(),
        });
    }
    pub fn get_key(&mut self, cx: &mut UiCtx) {
        let req = cx.core.request_id();
        self.get_key_req = Some(req);
        cx.core.send(Command::OpenExternal { url: ELEVENLABS_API_KEYS_URL.into(), req: Some(req) });
    }
    pub fn credits_retry(&mut self, cx: &mut UiCtx) {
        cx.core.send(Command::RefreshCredits { force: true });
    }
    pub fn test_voice(&mut self, cx: &mut UiCtx) {
        if self.test_req.is_some() {
            return;
        }
        let req = cx.core.request_id();
        self.test_req = Some(req);
        let text = vox_core::copy::test_voice_sample(&self.view(cx).voice_name);
        cx.core.send(Command::ReadText {
            req: Some(req),
            text,
            options: ReadTextOptions { source: Some(ReadSource::Manual), source_app: Some(APP_NAME.into()), ..ReadTextOptions::default() },
        });
    }
    pub fn browse_voices(&mut self, cx: &mut UiCtx) {
        cx.nav.navigate(AppRoute::page(AppPage::Voices));
    }
    pub fn reset_voice_settings(&mut self, cx: &mut UiCtx) {
        if self.view(cx).voice.is_default() {
            return;
        }
        let defaults = serde_json::to_value(vs::VoiceSettings::default()).unwrap_or_else(|_| {
            json!({ "stability": 0.5, "similarityBoost": 0.75, "style": 0, "useSpeakerBoost": true, "speed": 1 })
        });
        self.update(cx, json!({ "voice": defaults }));
        self.revision = self.revision.wrapping_add(1);
    }
    /// Recorder start request for `action` (`settings-hotkey-start`): Rust may decline (window not focused, shortcuts
    /// off); the row then stays idle.
    pub fn hotkey_start(&mut self, cx: &mut UiCtx, action: &str) {
        let Some(action) = HotkeyAction::parse(action) else { return };
        if !self.view(cx).triggers.hotkeys_enabled {
            return;
        }
        hotkey_recorder::start(cx.core, action);
    }
    /// The recorder lost focus (`settings-hotkey-stop`): stop recording, value unchanged.
    pub fn hotkey_stop(&mut self, cx: &mut UiCtx) {
        hotkey_recorder::stop(cx.core);
    }
    /// Idle Backspace/Delete on a row (`settings-hotkey-clear`): the shortcut becomes `""` (disabled).
    pub fn hotkey_clear(&mut self, cx: &mut UiCtx, action: &str) {
        let Some(action) = HotkeyAction::parse(action) else { return };
        self.hotkeys.clear();
        self.update(cx, vs::patch_at(&action.settings_path(), Value::String(String::new())));
    }
    pub fn add_excluded(&mut self, cx: &mut UiCtx, name: &str) {
        if js_trim(name).is_empty() {
            return;
        }
        let view = self.view(cx);
        match vs::add_excluded_app(&view.triggers.excluded_apps, name) {
            Ok(list) => {
                self.exclude_error = None;
                self.update(cx, json!({ "triggers": { "excludedApps": list } }));
                cx.fields.settings_exclude_draft = Some(String::new());
            }
            Err(message) => self.exclude_error = Some(message),
        }
    }
    pub fn remove_excluded(&mut self, cx: &mut UiCtx, name: &str) {
        let view = self.view(cx);
        let list: Vec<String> = view.triggers.excluded_apps.iter().filter(|a| a.as_str() != name).cloned().collect();
        if list.len() != view.triggers.excluded_apps.len() {
            self.update(cx, json!({ "triggers": { "excludedApps": list } }));
        }
    }
    pub fn excluded_edited(&mut self, cx: &mut UiCtx) {
        let _ = cx;
        self.exclude_error = None;
    }
    pub fn clear_cache(&mut self, cx: &mut UiCtx) {
        if self.clear_cache_req.is_some() {
            return;
        }
        let req = cx.core.request_id();
        self.clear_cache_req = Some(req);
        cx.core.send(Command::ClearCache { req });
    }
    pub fn export_history(&mut self, cx: &mut UiCtx, owner: Option<isize>) {
        if self.export_req.is_some() {
            return;
        }
        let req = cx.core.request_id();
        self.export_req = Some(req);
        cx.core.send(Command::ExportHistory { req, owner });
    }
    pub fn clear_history(&mut self, cx: &mut UiCtx) {
        cx.dialogs.open(DialogSpec {
            id: "library-clear-history".into(),
            owner: AppPage::Settings,
            title: "Clear history?".into(),
            body: "Every read, favorite and listening stat will be deleted. This can\u{2019}t be undone.".into(),
            confirm_label: "Clear history".into(),
            cancel_label: "Cancel".into(),
            danger: true,
            busy: self.clear_history_req.is_some(),
        });
    }
    pub fn port_edited(&mut self, cx: &mut UiCtx, text: &str) {
        let draft = self.port.edited(text);
        if draft != text {
            cx.fields.settings_port_draft = Some(draft);
        }
    }
    pub fn commit_port(&mut self, cx: &mut UiCtx, text: &str) {
        let stored = self.view(cx).server.port;
        match self.port.commit(text, stored) {
            PortCommit::Save(port) => {
                let req = self.update(cx, json!({ "server": { "port": port } }));
                self.enter_recheck(req, cx.now_ms);
                self.sync_port(cx);
                self.arm(cx);
            }
            PortCommit::Nothing => {
                if self.port.draft != text && !self.port.draft.is_empty() {
                    cx.fields.settings_port_draft = Some(self.port.draft.clone());
                }
            }
            PortCommit::Invalid => {}
        }
    }
    pub fn revert_port(&mut self, cx: &mut UiCtx) {
        let stored = self.view(cx).server.port;
        cx.fields.settings_port_draft = Some(self.port.revert(stored));
    }
    pub fn copy_text(&mut self, cx: &mut UiCtx, text: &str) {
        if text.is_empty() {
            return;
        }
        let req = cx.core.request_id();
        self.copy_reqs.push((req, text.to_string()));
        cx.core.send(Command::CopyText { req, text: text.to_string() });
    }
    pub fn show_extension_folder(&mut self, cx: &mut UiCtx) {
        cx.core.send(Command::RevealPath(RevealTarget::Extension));
    }
    pub fn open_data_folder(&mut self, cx: &mut UiCtx) {
        cx.core.send(Command::RevealPath(RevealTarget::UserData));
    }
    pub fn open_link(&mut self, cx: &mut UiCtx, which: &str) {
        let url = match which {
            "dashboard" => ELEVENLABS_DASHBOARD_URL,
            "billing" => ELEVENLABS_BILLING_URL,
            "docs" => ELEVENLABS_DOCS_URL,
            _ => return,
        };
        // About links: failures are only logged by the core (no request id, spec §8 "no toast").
        cx.core.send(Command::OpenExternal { url: url.into(), req: None });
    }

    // ───────────── answers ─────────────

    fn on_saved(&mut self, cx: &mut UiCtx, req: RequestId, result: &Result<(), String>) -> bool {
        let mut dirty = false;
        if self.overlay.contains(req) {
            if let Some(message) = self.overlay.on_saved(req, result) {
                Self::toast(cx, Toast::error(message).with_title("Couldn\u{2019}t save that setting").with_id("settings-save-error"));
                // Controls the user flipped must show the reverted value; the port field (a text draft, not bound
                // to the data) follows the stored port again.
                self.revision = self.revision.wrapping_add(1);
                self.port.error = false;
                self.sync_port(cx);
            }
            dirty = true;
        }
        if self.recheck.is_some_and(|r| r.req == req) {
            if result.is_ok() {
                if let Some(r) = &mut self.recheck {
                    r.saved = true;
                }
            } else {
                self.recheck = None;
            }
            dirty = true;
        }
        dirty
    }

    fn on_api_key(&mut self, cx: &mut UiCtx, req: RequestId, result: &vox_core::voices::ApiKeyResult) -> bool {
        if self.key_req != Some(req) {
            return false;
        }
        self.key_req = None;
        if result.ok {
            self.key_form_open = false;
            self.key_error = None;
            self.key_warning = result.warning.clone().filter(|w| !w.trim().is_empty());
            cx.fields.settings_key_draft = Some(String::new());
            Self::toast(cx, Toast::success("API key saved").with_id("api-key"));
        } else {
            let error = result.error.as_deref().map(str::trim).filter(|e| !e.is_empty()).map(format::clean_error);
            self.key_error = Some(error.unwrap_or_else(|| KEY_REJECTED.into()));
        }
        true
    }

    fn on_done(&mut self, cx: &mut UiCtx, req: RequestId, result: &Result<(), String>) -> bool {
        if self.remove_req == Some(req) {
            self.remove_req = None;
            match result {
                Ok(()) => {
                    self.key_warning = None;
                    self.key_form_open = false;
                    self.key_error = None;
                    cx.dialogs.close("remove-key");
                    Self::toast(cx, Toast::info("API key removed").with_id("api-key"));
                }
                Err(e) => {
                    cx.dialogs.set_busy("remove-key", false);
                    Self::toast(cx, Toast::error(format::clean_error(e)).with_title("Couldn\u{2019}t remove the key").with_id("api-key"));
                }
            }
            return true;
        }
        if self.get_key_req == Some(req) {
            self.get_key_req = None;
            if let Err(e) = result {
                Self::toast(cx, Toast::error(vox_core::copy::open_link_failed(e)).with_id("open-external"));
            }
            return false;
        }
        if self.clear_history_req == Some(req) {
            self.clear_history_req = None;
            match result {
                Ok(()) => {
                    cx.dialogs.close("library-clear-history");
                    Self::toast(cx, Toast::success("History cleared").with_id("library-history"));
                }
                Err(e) => {
                    // The dialog stays open (buttons re-enabled) behind the error toast.
                    cx.dialogs.set_busy("library-clear-history", false);
                    Self::toast(cx, Toast::error(format::clean_error(e)).with_id("library-history"));
                }
            }
            return true;
        }
        if let Some(i) = self.copy_reqs.iter().position(|(r, _)| *r == req) {
            let (_, text) = self.copy_reqs.remove(i);
            match result {
                Ok(()) => self.copied = Some((text, cx.now_ms + COPY_CONFIRM_MS as f64)),
                Err(_) => Self::toast(cx, Toast::error("Couldn\u{2019}t copy to the clipboard").with_id("copy")),
            }
            return true;
        }
        false
    }

    fn on_cache_stats(&mut self, cx: &mut UiCtx, req: RequestId, result: &Result<CacheStats, String>) -> bool {
        if self.clear_cache_req == Some(req) {
            self.clear_cache_req = None;
            match result {
                Ok(stats) => {
                    self.stats = Some(*stats);
                    self.stats_error = None;
                    Self::toast(cx, Toast::success("Audio cache cleared").with_id("library-cache"));
                }
                Err(e) => Self::toast(cx, Toast::error(format::clean_error(e)).with_id("library-cache")),
            }
            return true;
        }
        if self.stats_req == Some(req) {
            self.stats_req = None;
            match result {
                Ok(stats) => {
                    self.stats = Some(*stats);
                    self.stats_error = None;
                }
                // A failed re-measure keeps the previous stats; the error shows only if nothing was ever measured.
                Err(e) => self.stats_error = Some(format::clean_error(e)),
            }
            return true;
        }
        false
    }

    fn on_exported(&mut self, cx: &mut UiCtx, req: RequestId, result: &Result<Option<std::path::PathBuf>, String>) -> bool {
        if self.export_req != Some(req) {
            return false;
        }
        self.export_req = None;
        match result {
            Ok(Some(path)) => {
                Self::toast(cx, Toast::success(path.display().to_string()).with_title("History exported").with_id("library-export"))
            }
            Ok(None) => {}
            Err(e) => Self::toast(cx, Toast::error(format::clean_error(e)).with_id("library-export")),
        }
        true
    }

    fn on_read_started(&mut self, cx: &mut UiCtx, req: RequestId, result: &ReadResult) -> bool {
        if self.test_req != Some(req) {
            return false;
        }
        self.test_req = None;
        // The core toasts every ordinary read failure itself; only the "transport" failure (the core's panic
        // answer) is toasted here.
        if *result == ReadResult::internal_error() {
            let message = format::clean_error(result.error.as_deref().unwrap_or_default());
            Self::toast(cx, Toast::error(message).with_id("voice-test"));
        }
        true
    }

    // ───────────── sections ─────────────

    fn account_data(&self, cx: &UiCtx) -> AccountData {
        let has_key = cx.settings.has_api_key;
        let demo = Self::demo(cx);
        let state = if has_key {
            AccountState::Stored
        } else if demo {
            AccountState::Demo
        } else {
            AccountState::NoKey
        };
        let badge = match state {
            AccountState::Stored => badge("Connected", BadgeTone::Success),
            AccountState::Demo => badge("Demo account", BadgeTone::Neutral),
            AccountState::NoKey => badge("No key", BadgeTone::Attention),
        };
        // "Replace" while a key is stored; otherwise the first-time copy (also the demo's "Add key").
        let (field_label, submit_label, lead) =
            if has_key { ("New API key", "Save new key", "") } else { ("API key", "Save key", KEY_LEAD) };
        AccountData {
            state,
            badge,
            form_open: self.key_form_open || state == AccountState::NoKey,
            replacing: self.key_form_open && has_key,
            busy: self.key_req.is_some(),
            field_label: field_label.into(),
            hint: KEY_HINT.into(),
            error: self.key_error.clone().unwrap_or_default().into(),
            submit_label: submit_label.into(),
            lead: lead.into(),
            limited_warning: if has_key { self.key_warning.clone().unwrap_or_default().into() } else { SharedString::new() },
            remove_busy: self.remove_req.is_some(),
            credits: credits_data(cx.credits, CreditsSurface::SettingsAccount, demo, cx.now_ms),
            notice: cx.app_info.api_key_notice.clone().unwrap_or_default().into(),
        }
    }

    fn voice_data(&self, cx: &mut UiCtx, view: &Settings) -> VoiceModelData {
        let catalog = cx.settings.has_api_key || Self::demo(cx);
        let empty: Vec<TtsModel> = Vec::new();
        let models: &[TtsModel] = if catalog { cx.models.as_slice() } else { &empty };
        let (tags, tags_loading) =
            current_voice_tags(&view.voice_id, self.voices.data.as_deref().map(|v| v.as_slice()), self.voices.loading);
        let model_opts = vs::model_options(models, &view.model_id);
        let model = models.iter().find(|m| m.model_id == view.model_id);
        let model_badges: Vec<BadgeData> = model
            .map(vs::model_badges)
            .unwrap_or_default()
            .into_iter()
            .map(|(text, flagged)| badge(text, if flagged { BadgeTone::Attention } else { BadgeTone::Neutral }))
            .collect();
        let lang_opts = vs::language_options(models, &view.model_id, &view.language_code);
        let v = &view.voice;
        VoiceModelData {
            voice_name: view.voice_name.as_str().into(),
            voice_art: cx.art.image(&view.voice_id, ArtSize::Large),
            voice_tags: strings(tags),
            tags_loading,
            model_index: index_of(&model_opts, |(id, _)| *id == view.model_id),
            model_options: choice_model(model_opts.iter().map(|(id, label)| choice(id.as_str(), label.as_str(), label.as_str())).collect()),
            model_description: model
                .and_then(|m| m.description.as_deref())
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .unwrap_or(MODEL_FALLBACK_DESCRIPTION)
                .into(),
            model_badges: ModelRc::new(VecModel::from(model_badges)),
            model_error: self
                .models
                .error
                .as_deref()
                .map(|e| format!("Couldn\u{2019}t load models: {}", format::clean_error(e)))
                .unwrap_or_default()
                .into(),
            models_loading: self.models.loading && models.is_empty(),
            language_index: index_of(&lang_opts, |(id, _)| *id == view.language_code),
            language_options: choice_model(lang_opts.iter().map(|(id, label)| choice(id.as_str(), label.as_str(), label.as_str())).collect()),
            quality_index: index_of(&OutputFormat::ALL, |f| *f == view.output_format),
            quality_options: choice_model(OutputFormat::ALL.iter().map(|f| choice(f.as_str(), f.label(), f.label())).collect()),
            stability: v.stability as f32,
            stability_text: percent_label(v.stability).into(),
            similarity: v.similarity_boost as f32,
            similarity_text: percent_label(v.similarity_boost).into(),
            style: v.style as f32,
            style_text: percent_label(v.style).into(),
            speed: v.speed as f32,
            speed_text: speed_label(v.speed).into(),
            speaker_boost: v.use_speaker_boost,
            reset_enabled: !v.is_default(),
            test_busy: self.test_req.is_some(),
        }
    }

    fn playback_data(view: &Settings) -> PlaybackData {
        let rates = vs::playback_rate_options(view.playback_rate);
        let max_chars = vs::max_chars_options(view.text.max_chars);
        PlaybackData {
            rate_index: vs::rate_option_index(&rates, view.playback_rate).map(|i| i as i32).unwrap_or(0),
            rate_options: choice_model(
                rates.iter().map(|r| choice(number_to_string(*r), format::rate_label(*r), vs::rate_accessible_label(*r))).collect(),
            ),
            volume: view.volume as f32,
            volume_text: percent_label(view.volume).into(),
            skip_urls: view.text.skip_urls,
            skip_emails: view.text.skip_emails,
            max_chars_index: index_of(&max_chars, |n| *n == view.text.max_chars),
            max_chars_options: choice_model(
                max_chars
                    .iter()
                    .map(|n| {
                        let label = vs::max_chars_label(*n);
                        choice(n.to_string(), label.as_str(), label.as_str())
                    })
                    .collect(),
            ),
        }
    }

    fn triggers_data(&self, cx: &UiCtx, view: &Settings) -> TriggersData {
        let hook = cx.app_info.native_hook_available;
        let rows: Vec<HotkeyRowData> = HotkeyAction::ALL
            .iter()
            .map(|&action| {
                let accel = view.triggers.hotkeys.get(action);
                let (recording, preview, hint) = hotkey_recorder::row_view(action, cx.now_ms);
                HotkeyRowData {
                    action: action.id().into(),
                    label: action.label().into(),
                    description: action.description().into(),
                    keys: keycaps(accel),
                    spoken: if accel.trim().is_empty() { SharedString::new() } else { vox_core::hotkey::spoken(accel).into() },
                    recording,
                    preview: strings(preview),
                    hint: hint.into(),
                    warning: self.hotkeys.warning(view, action, cx.now_ms).unwrap_or_default().into(),
                }
            })
            .collect();
        TriggersData {
            hook_available: hook,
            hook_notice_title: if hook { "" } else { vox_core::copy::HOOK_UNAVAILABLE_TITLE }.into(),
            hook_notice_body: if hook { "" } else { vox_core::copy::HOOK_UNAVAILABLE_BODY }.into(),
            right_click_chip: view.triggers.right_click_chip,
            selection_popup: view.triggers.selection_popup,
            hotkeys_enabled: view.triggers.hotkeys_enabled,
            hotkeys: ModelRc::new(VecModel::from(rows)),
            media_keys: view.triggers.media_keys,
            restore_clipboard: view.triggers.restore_clipboard,
            excluded_apps: strings(view.triggers.excluded_apps.iter().cloned()),
            exclude_error: self.exclude_error.clone().unwrap_or_default().into(),
        }
    }

    fn widget_data(view: &Settings) -> WidgetSettingsData {
        let w = &view.widget;
        WidgetSettingsData {
            enabled: w.enabled,
            position_options: choice_model(WidgetPosition::ALL.iter().map(|p| choice(p.as_str(), p.label(), p.accessible_label())).collect()),
            position_index: index_of(&WidgetPosition::ALL, |p| *p == w.position),
            position_hint: w.position.hint().into(),
            compact: w.compact,
            highlight_words: w.highlight_words,
            auto_hide: w.auto_hide,
        }
    }

    fn appearance_data(view: &Settings) -> AppearanceData {
        AppearanceData {
            theme_options: choice_model(ThemeMode::ALL.iter().map(|t| choice(t.as_str(), t.label(), t.label())).collect()),
            theme_index: index_of(&ThemeMode::ALL, |t| *t == view.app.theme),
            intro: vox_core::copy::APPEARANCE_INTRO.into(),
            theme_notes: strings(vox_core::copy::THEME_NOTES.iter().map(|s| s.to_string())),
        }
    }

    fn library_data(&self, view: &Settings) -> LibraryData {
        let lib = &view.library;
        let shown_days = self.retention.pending_days().unwrap_or(lib.retention_days);
        let options = vs::retention_options(lib.retention_days);
        let limit_bytes = lib.cache_limit_bytes() as f64;
        let (usage_text, usage_fraction) = match (&self.stats, &self.stats_error) {
            (Some(s), _) => (
                format!(
                    "{} of {} \u{00B7} {} cached",
                    format::format_bytes(s.bytes),
                    format::format_megabytes(lib.cache_limit_mb),
                    format::count_label(s.reads, "read", "reads")
                ),
                (s.bytes as f64 / limit_bytes.max(1.0)).min(1.0),
            ),
            (None, Some(_)) => ("Couldn\u{2019}t read the cache size.".to_string(), 0.0),
            (None, None) => ("Measuring\u{2026}".to_string(), 0.0),
        };
        LibraryData {
            cache_badge: self.stats.map(|s| format::cache_badge(s.bytes)).unwrap_or_default().into(),
            save_history: lib.save_history,
            store_text: lib.store_text,
            retention_index: index_of(&options, |(d, _)| *d == shown_days),
            retention_options: choice_model(options.iter().map(|(d, label)| choice(d.to_string(), label.as_str(), label.as_str())).collect()),
            cache_audio: lib.cache_audio,
            cache_limit: lib.cache_limit_mb as f32,
            cache_limit_max: lib.cache_limit_mb.max(DEFAULT_CACHE_SLIDER_MAX) as f32,
            cache_limit_text: format::format_megabytes(lib.cache_limit_mb).into(),
            usage_text: usage_text.into(),
            usage_fraction: usage_fraction as f32,
            usage_high: usage_fraction > 0.9,
            clear_cache_enabled: self.stats.is_some_and(|s| s.bytes > 0),
            clear_cache_busy: self.clear_cache_req.is_some(),
            export_busy: self.export_req.is_some(),
            clear_history_busy: self.clear_history_req.is_some(),
        }
    }

    fn integrations_data(&self, cx: &UiCtx, view: &Settings) -> IntegrationsData {
        let info = cx.app_info;
        let bridge_on = view.triggers.browser_extension;
        let rechecking = self.rechecking(cx.now_ms);
        let bridge_badge = if !bridge_on {
            badge("Off", BadgeTone::Neutral)
        } else if rechecking {
            badge("Checking\u{2026}", BadgeTone::Neutral)
        } else if info.server_running {
            badge(format!("Listening on {}", info.server_port), BadgeTone::Success)
        } else {
            badge("Not running", BadgeTone::Attention)
        };
        let port_error = if self.port.error { PORT_INVALID } else { "" };
        let port_note = if bridge_on && !rechecking && !info.server_running {
            vox_core::copy::port_may_be_in_use(view.server.port)
        } else {
            String::new()
        };
        IntegrationsData {
            bridge_enabled: bridge_on,
            bridge_badge,
            port_error: port_error.into(),
            port_note: port_note.into(),
            explorer_enabled: view.triggers.explorer_menu,
            explorer_installed: info.explorer_menu_installed,
            launch_at_login: view.app.launch_at_login,
            start_minimized: view.app.start_minimized,
            close_to_tray: view.app.close_to_tray,
            copied: self
                .copied
                .as_ref()
                .filter(|(_, until)| cx.now_ms < *until)
                .map(|(t, _)| t.as_str())
                .unwrap_or_default()
                .into(),
        }
    }

    fn about_data(cx: &UiCtx) -> AboutData {
        let info = cx.app_info;
        let loaded = !info.version.is_empty();
        let folder = info.user_data_path.display().to_string();
        AboutData {
            version: if !loaded {
                "\u{2026}".into()
            } else if info.is_packaged {
                info.version.as_str().into()
            } else {
                format!("{} \u{00B7} dev", info.version).into()
            },
            runtime: if info.runtime.is_empty() { "\u{2026}".into() } else { info.runtime.as_str().into() },
            build: if info.build.is_empty() { "dev".into() } else { info.build.as_str().into() },
            data_folder_ready: !folder.is_empty(),
            data_folder: if folder.is_empty() { "Settings, history and cached audio".into() } else { folder.into() },
            demo: Self::demo(cx),
        }
    }
}

impl PageController for SettingsController {
    type Data = SettingsData;

    fn on_enter(&mut self, cx: &mut UiCtx, route: &AppRoute, intent: &NavIntent) {
        let _ = (route, intent);
        if self.active {
            // A deep link to another section while the page is up: the shell scrolls; nothing to reset.
            return;
        }
        self.reset_page_state();
        self.active = true;
        self.poll(cx);
        let port = self.view(cx).server.port;
        self.last_port = Some(port);
        self.last_has_key = Some(cx.settings.has_api_key);
        cx.fields.settings_port_draft = Some(self.port.revert(port));
        cx.fields.settings_key_draft = Some(String::new());
        cx.fields.settings_exclude_draft = Some(String::new());
        self.fetch_catalog(cx);
        self.query_stats(cx);
        cx.core.send(Command::QueryAppInfo);
        if cx.settings.has_api_key || Self::demo(cx) {
            cx.core.send(Command::RefreshCredits { force: false });
        }
        self.revision = self.revision.wrapping_add(1);
    }

    fn on_leave(&mut self, cx: &mut UiCtx) {
        self.active = false;
        hotkey_recorder::stop(cx.core);
        self.retention.cancel();
        self.count_req = None;
        self.recheck = None;
        self.remeasure_at = None;
        // The key field never keeps a secret around while the page is not shown.
        cx.fields.settings_key_draft = Some(String::new());
        self.timer.cancel();
    }

    fn on_update(&mut self, cx: &mut UiCtx, update: &UiUpdate) -> bool {
        let polled = self.poll(cx);
        let dirty = match update {
            UiUpdate::Settings(s) => {
                if self.last_has_key != Some(s.has_api_key) {
                    let first = self.last_has_key.is_none();
                    self.last_has_key = Some(s.has_api_key);
                    if !s.has_api_key {
                        self.key_warning = None;
                    }
                    if !first && self.active {
                        self.fetch_catalog(cx);
                    }
                }
                self.sync_port(cx);
                if !self.overlay.saving() {
                    // A change made elsewhere (tray, widget drag, CycleRate…): controls follow the data.
                    self.revision = self.revision.wrapping_add(1);
                }
                true
            }
            UiUpdate::SettingsSaved { req, result } => self.on_saved(cx, *req, result),
            UiUpdate::AppInfo(_) => {
                if self.recheck.is_some_and(|r| r.saved) {
                    self.recheck = None;
                }
                true
            }
            UiUpdate::Credits(_) | UiUpdate::Refresh(_) => true,
            UiUpdate::Voices { result, .. } => {
                if self.active {
                    self.voices.settle(result.clone());
                }
                self.active
            }
            UiUpdate::Models { result, .. } => {
                if self.active {
                    self.models.settle(result.as_ref().map(|_| ()).map_err(Clone::clone));
                }
                self.active
            }
            UiUpdate::ApiKey { req, result } => self.on_api_key(cx, *req, result),
            UiUpdate::Done { req, result } => self.on_done(cx, *req, result),
            UiUpdate::CacheStats { req, result } => self.on_cache_stats(cx, *req, result),
            UiUpdate::Exported { req, result } => self.on_exported(cx, *req, result),
            UiUpdate::ExpiringCount { req, result } if self.count_req == Some(*req) => {
                self.count_req = None;
                let step = self.retention.on_count(result);
                self.apply_retention(cx, step);
                true
            }
            UiUpdate::ReadStarted { req, result } => self.on_read_started(cx, *req, result),
            UiUpdate::HistoryChanged => {
                // Re-measure 2.5 s after the last history change (trailing).
                if self.active {
                    self.remeasure_at = Some(cx.now_ms + CACHE_REMEASURE_MS as f64);
                }
                false
            }
            UiUpdate::HotkeysResumed => {
                hotkey_recorder::on_resumed();
                true
            }
            _ => false,
        };
        self.arm(cx);
        dirty || polled
    }

    fn on_dialog(&mut self, cx: &mut UiCtx, id: &str, confirmed: bool) -> bool {
        match id {
            "remove-key" => {
                if confirmed {
                    if self.remove_req.is_none() {
                        let req = cx.core.request_id();
                        self.remove_req = Some(req);
                        cx.dialogs.set_busy(id, true);
                        cx.core.send(Command::ClearApiKey { req });
                    }
                } else if self.remove_req.is_none() {
                    // Close requests are ignored while removing.
                    cx.dialogs.close(id);
                }
                true
            }
            "library-clear-history" => {
                if confirmed {
                    if self.clear_history_req.is_none() {
                        let req = cx.core.request_id();
                        self.clear_history_req = Some(req);
                        cx.dialogs.set_busy(id, true);
                        cx.core.send(Command::ClearHistory { req });
                    }
                } else if self.clear_history_req.is_none() {
                    cx.dialogs.close(id);
                }
                true
            }
            "retention" => {
                let step = self.retention.on_dialog(confirmed);
                cx.dialogs.close(id);
                self.apply_retention(cx, step);
                true
            }
            _ => false,
        }
    }

    fn build(&self, cx: &mut UiCtx) -> SettingsData {
        let view = self.view(cx);
        let saving = self.overlay.saving();
        SettingsData {
            loading: false,
            saving,
            status: if saving { "Saving" } else { "Saved automatically" }.into(),
            sections: choice_model(vs::SETTINGS_SECTIONS.iter().map(|(id, label)| choice(*id, *label, *label)).collect()),
            revision: self.revision,
            account: self.account_data(cx),
            voice: self.voice_data(cx, &view),
            playback: Self::playback_data(&view),
            triggers: self.triggers_data(cx, &view),
            widget: Self::widget_data(&view),
            appearance: Self::appearance_data(&view),
            library: self.library_data(&view),
            integrations: self.integrations_data(cx, &view),
            about: Self::about_data(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::CoreHandle;
    use crate::ui::art::ArtCache;
    use crate::ui::{DialogSink, FieldWrites, NavSink, ToastSink};
    use crate::update::{CreditsUpdate, CreditsView};
    use crossbeam_channel::{unbounded, Receiver};
    use slint::Model;
    use vox_core::app::AppInfo;
    use vox_core::locale::{with_locale, Locale};
    use vox_core::player::PlayerSnapshot;
    use vox_core::voices::{ApiKeyResult, ModelLanguage};

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
        now: f64,
        demo: bool,
        art: ArtCache,
        toasts: ToastSink,
        nav: NavSink,
        dialogs: DialogSink,
        fields: FieldWrites,
    }

    impl World {
        fn new() -> Self {
            let (tx, rx) = unbounded();
            let settings = Settings { has_api_key: true, onboarded: true, ..Settings::default() };
            let info = AppInfo {
                name: "Vox".into(),
                version: "1.0.0".into(),
                runtime: "Rust 1.98 \u{00B7} Slint 1.18.1".into(),
                build: "a41c9e2".into(),
                is_packaged: true,
                user_data_path: "C:\\Users\\Kwaku\\AppData\\Roaming\\Vox".into(),
                server_port: 47823,
                server_running: true,
                native_hook_available: true,
                explorer_menu_installed: false,
                ..AppInfo::default()
            };
            World {
                core: CoreHandle::new(tx),
                rx,
                settings: Arc::new(settings),
                player: PlayerSnapshot::default(),
                credits: CreditsUpdate { view: CreditsView::Loading, refreshing: false },
                app_info: Arc::new(info),
                models: Arc::new(Vec::new()),
                now: NOW,
                demo: false,
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
                now_ms: self.now,
                demo: self.demo,
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
        /// The core: applies the update's patch, publishes `Settings` then `SettingsSaved{req}`.
        fn apply(&mut self, c: &mut SettingsController, req: RequestId, patch: &Value, ok: bool) {
            if ok {
                self.settings = Arc::new(self.settings.apply_patch(patch));
            }
            let s = self.settings.clone();
            c.on_update(&mut self.cx(), &UiUpdate::Settings(s));
            let result = if ok { Ok(()) } else { Err("Error: disk full".to_string()) };
            c.on_update(&mut self.cx(), &UiUpdate::SettingsSaved { req, result });
        }
    }

    /// The `UpdateSettings` commands sent.
    fn writes(cmds: &[Command]) -> Vec<(RequestId, Value)> {
        cmds.iter()
            .filter_map(|c| match c {
                Command::UpdateSettings { req: Some(r), patch } => Some((*r, patch.clone())),
                _ => None,
            })
            .collect()
    }

    fn entered() -> (World, SettingsController) {
        let mut w = World::new();
        let mut c = SettingsController::default();
        c.on_enter(&mut w.cx(), &AppRoute::settings("account"), &NavIntent::None);
        w.sent();
        (w, c)
    }

    // ── pure pieces ──

    #[test]
    fn overlay_failed_first_update_reverts() {
        let stored = Settings::default();
        let mut o = Overlay::default();
        o.push(RequestId(1), json!({ "volume": 0.5 }));
        o.push(RequestId(2), json!({ "text": { "skipEmails": true } }));
        let v = o.view(&stored);
        assert_eq!((v.volume, v.text.skip_emails), (0.5, true));
        assert!(o.saving());
        // The second write landed (the broadcast has it); the first failed.
        let mut stored2 = stored.clone();
        stored2.text.skip_emails = true;
        assert_eq!(o.on_saved(RequestId(1), &Err("TypeError: disk full".into())).as_deref(), Some("disk full"));
        let v = o.view(&stored2);
        assert_eq!((v.volume, v.text.skip_emails), (1.0, true), "the failed write reverts, the other stays");
        assert!(o.saving(), "still waiting for the second answer");
        // An unrelated broadcast does not settle anything; the second answer does.
        assert_eq!(o.on_saved(RequestId(9), &Ok(())), None);
        assert!(o.saving());
        assert_eq!(o.on_saved(RequestId(2), &Ok(())), None);
        assert!(!o.saving());
        assert_eq!(o.view(&stored2), stored2);
    }

    #[test]
    fn retention_7_30_7_within_450ms_counts_once() {
        let mut g = RetentionGuard::default();
        assert_eq!(g.on_choice(7, 0, 0.0), RetentionStep::Nothing);
        assert_eq!(g.on_choice(30, 0, 100.0), RetentionStep::Nothing);
        assert_eq!(g.on_choice(7, 0, 200.0), RetentionStep::Nothing);
        assert_eq!(g.pending_days(), Some(7));
        assert_eq!(g.settle_deadline(), Some(650.0));
        assert_eq!(g.on_settle(450.0), RetentionStep::Nothing, "a stale timer does nothing");
        assert_eq!(g.on_settle(650.0), RetentionStep::Count { days: 7 });
        assert_eq!(g.on_settle(700.0), RetentionStep::Nothing, "one count only");
        assert_eq!(g.on_count(&Ok(3)), RetentionStep::Confirm { days: 7, count: 3 });
        assert_eq!(g.on_dialog(true), RetentionStep::Save { days: 7 });
        assert_eq!(g.pending_days(), None);
    }

    #[test]
    fn retention_back_to_stored_writes_nothing_and_looser_saves_at_once() {
        let mut g = RetentionGuard::default();
        assert_eq!(g.on_choice(7, 30, 0.0), RetentionStep::Nothing);
        assert_eq!(g.on_choice(30, 30, 100.0), RetentionStep::Nothing);
        assert_eq!(g.pending_days(), None);
        assert_eq!(g.on_settle(1000.0), RetentionStep::Nothing);
        // Looser (or keep forever) is never guarded.
        assert_eq!(g.on_choice(90, 30, 0.0), RetentionStep::Save { days: 90 });
        assert_eq!(g.on_choice(0, 30, 0.0), RetentionStep::Save { days: 0 });
    }

    #[test]
    fn retention_count_zero_saves_silently_and_failure_reverts() {
        let mut g = RetentionGuard::default();
        g.on_choice(30, 0, 0.0);
        assert_eq!(g.on_settle(450.0), RetentionStep::Count { days: 30 });
        assert_eq!(g.on_count(&Ok(0)), RetentionStep::Save { days: 30 });
        g.on_choice(7, 0, 0.0);
        g.on_settle(500.0);
        assert_eq!(g.on_count(&Err("Error: locked".into())), RetentionStep::Revert { error: Some("locked".into()) });
        g.on_choice(7, 0, 0.0);
        g.on_settle(500.0);
        g.on_count(&Ok(2));
        assert_eq!(g.on_dialog(false), RetentionStep::Revert { error: None });
        assert_eq!(g.on_count(&Ok(5)), RetentionStep::Nothing, "a stale answer does nothing");
    }

    #[test]
    fn port_field_rules() {
        let mut p = PortField::default();
        assert_eq!(p.edited("8a0-80123"), "80801");
        assert!(!p.error);
        assert_eq!(p.commit("00080", 47823), PortCommit::Invalid);
        assert!(p.error);
        assert_eq!(p.edited("0008"), "0008");
        assert!(!p.error, "typing clears the error");
        assert_eq!(p.revert(47823), "47823");
        assert!(!p.error);
        assert_eq!(p.commit("47823", 47823), PortCommit::Nothing, "blur with the stored value writes nothing");
        assert_eq!(p.commit(" 51200 ", 47823), PortCommit::Save(51200));
        assert_eq!(p.commit("1023", 47823), PortCommit::Invalid);
        assert_eq!(p.commit("", 47823), PortCommit::Invalid);
        assert_eq!(p.commit("01024", 1024), PortCommit::Nothing);
        assert_eq!(p.draft, "1024");
        assert_eq!(p.on_stored_change(50000), "50000");
    }

    #[test]
    fn hotkey_rows_rejection_and_persistent_warnings() {
        let mut s = Settings::default();
        let mut rows = HotkeyRows::default();
        assert_eq!(rows.warning(&s, HotkeyAction::Stop, 0.0), None);
        rows.reject(HotkeyAction::Stop, "Ctrl+C", 1000.0);
        let msg = "Ctrl+C is used by Windows and other apps, so it can\u{2019}t be a global shortcut. Pick another.";
        assert_eq!(rows.warning(&s, HotkeyAction::Stop, 1000.0).as_deref(), Some(msg));
        assert_eq!(rows.warning(&s, HotkeyAction::ReadSelection, 1000.0), None, "only that row");
        assert_eq!(s.triggers.hotkeys.stop, "Ctrl+Alt+S", "the old shortcut stays");
        assert_eq!(rows.deadline(1000.0), Some(7000.0));
        assert_eq!(rows.warning(&s, HotkeyAction::Stop, 7000.0), None, "gone after 6 s");
        // A stored reserved chord (hand-edited file) and a shared shortcut show persistent messages.
        s.triggers.hotkeys.stop = "Ctrl+Insert".into();
        assert_eq!(
            rows.warning(&s, HotkeyAction::Stop, 8000.0).as_deref(),
            Some("Ctrl+Ins is used by Windows and other apps (copying, pasting\u{2026}). Pick another shortcut.")
        );
        s.triggers.hotkeys.play_pause = "Ctrl+Alt+R".into();
        let shared = "Same shortcut as another action \u{2014} only one of them will fire.";
        assert_eq!(rows.warning(&s, HotkeyAction::PlayPause, 0.0).as_deref(), Some(shared));
        assert_eq!(rows.warning(&s, HotkeyAction::ReadSelection, 0.0).as_deref(), Some(shared));
    }

    // ── controller ──

    #[test]
    fn entering_fetches_and_resets_the_fields() {
        let mut w = World::new();
        let mut c = SettingsController::default();
        c.on_enter(&mut w.cx(), &AppRoute::settings("library"), &NavIntent::None);
        let cmds = w.sent();
        assert!(cmds.iter().any(|c| matches!(c, Command::FetchVoices { refresh: false })));
        assert!(cmds.iter().any(|c| matches!(c, Command::FetchModels { refresh: false })));
        assert!(cmds.iter().any(|c| matches!(c, Command::QueryCacheStats { .. })));
        assert!(cmds.iter().any(|c| matches!(c, Command::QueryAppInfo)));
        assert_eq!(w.fields.settings_port_draft.as_deref(), Some("47823"));
        assert_eq!(w.fields.settings_key_draft.as_deref(), Some(""));
        // No key and no demo: nothing is fetched.
        let mut w = World::new();
        w.settings = Arc::new(Settings::default());
        let mut c = SettingsController::default();
        c.on_enter(&mut w.cx(), &AppRoute::page(AppPage::Settings), &NavIntent::None);
        assert!(!w.sent().iter().any(|c| matches!(c, Command::FetchVoices { .. })));
    }

    #[test]
    fn populated_build_matches_the_fixture() {
        with_locale(Locale::EN_US, || {
            let (mut w, mut c) = entered();
            w.credits = CreditsUpdate {
                view: CreditsView::Ready(vox_core::voices::AccountInfo {
                    tier: "creator".into(),
                    character_count: 61_234.0,
                    character_limit: 100_000.0,
                    next_reset_unix: Some((NOW + 12.0 * 86_400_000.0 - 3_600_000.0) / 1000.0),
                    status: None,
                }),
                refreshing: false,
            };
            c.on_update(&mut w.cx(), &UiUpdate::CacheStats { req: c.stats_req.unwrap(), result: Ok(CacheStats { reads: 214, files: 400, bytes: 187_000_000 }) });
            let d = c.build(&mut w.cx());
            assert!(!d.loading && !d.saving);
            assert_eq!(d.status, "Saved automatically");
            assert_eq!(d.sections.row_count(), 9);
            assert_eq!(d.sections.row_data(6).unwrap().label, "Library & privacy");
            // Account
            assert_eq!(d.account.state, AccountState::Stored);
            assert_eq!((d.account.badge.text.as_str(), d.account.badge.tone), ("Connected", BadgeTone::Success));
            assert_eq!((d.account.field_label.as_str(), d.account.submit_label.as_str(), d.account.lead.as_str()), ("New API key", "Save new key", ""));
            assert_eq!(d.account.credits.percent, "39%");
            assert_eq!(d.account.credits.tier, "Creator");
            // Voice & model
            assert_eq!(d.voice.voice_name, "George");
            assert_eq!((d.voice.stability_text.as_str(), d.voice.similarity_text.as_str(), d.voice.style_text.as_str()), ("50%", "75%", "0%"));
            assert_eq!(d.voice.speed_text, "1.00\u{00D7}");
            assert!(!d.voice.reset_enabled);
            assert_eq!(d.voice.quality_options.row_data(0).unwrap().label, "High \u{00B7} 128 kbps");
            assert!(d.voice.tags_loading, "the voice list is loading");
            // Playback
            assert_eq!(d.playback.rate_options.row_count(), 6);
            assert_eq!(d.playback.rate_index, 1);
            let r = d.playback.rate_options.row_data(2).unwrap();
            assert_eq!((r.value.as_str(), r.label.as_str(), r.accessible_label.as_str()), ("1.25", "1.25\u{00D7}", "1.25 times speed"));
            assert_eq!(d.playback.volume_text, "100%");
            assert_eq!(d.playback.max_chars_index, 3);
            assert_eq!(d.playback.max_chars_options.row_data(3).unwrap().label, "20,000 characters");
            // Triggers
            let t = &d.triggers;
            assert!(t.hook_available && t.hook_notice_title.is_empty());
            let h0 = t.hotkeys.row_data(0).unwrap();
            assert_eq!((h0.action.as_str(), h0.label.as_str(), h0.spoken.as_str()), ("readSelection", "Read selection", "Ctrl plus Alt plus R"));
            assert_eq!(h0.keys.iter().collect::<Vec<_>>(), vec!["Ctrl", "Alt", "R"]);
            assert!(!h0.recording && h0.hint.is_empty() && h0.warning.is_empty());
            // Widget / Appearance
            assert_eq!(d.widget.position_options.row_data(3).unwrap().accessible_label, "Near the cursor");
            assert_eq!(d.widget.position_hint, "Above the clock, out of the way of most documents.");
            assert_eq!(d.appearance.theme_index, 0);
            assert_eq!(d.appearance.theme_notes.row_count(), 3);
            assert_eq!(d.appearance.intro, vox_core::copy::APPEARANCE_INTRO);
            // Library
            let l = &d.library;
            assert_eq!(l.cache_badge, "187MB");
            assert_eq!(l.usage_text, "187 MB of 500 MB \u{00B7} 214 reads cached");
            assert!((l.usage_fraction - 0.374).abs() < 1e-6);
            assert!(!l.usage_high && l.clear_cache_enabled);
            assert_eq!((l.cache_limit, l.cache_limit_max, l.cache_limit_text.as_str()), (500.0, 5000.0, "500 MB"));
            assert_eq!(l.retention_options.row_data(4).unwrap().label, "1 year");
            // Integrations / About
            assert_eq!(d.integrations.bridge_badge.text, "Listening on 47823");
            assert_eq!(d.integrations.port_note, "");
            assert_eq!(d.about.version, "1.0.0");
            assert_eq!(d.about.build, "a41c9e2");
            assert_eq!(d.about.data_folder, "C:\\Users\\Kwaku\\AppData\\Roaming\\Vox");
            assert!(d.about.data_folder_ready && !d.about.demo);
        });
    }

    #[test]
    fn a_failed_toggle_reverts_with_a_toast_and_a_revision_bump() {
        let (mut w, mut c) = entered();
        c.set_bool(&mut w.cx(), "triggers.mediaKeys", false);
        let ws = writes(&w.sent());
        assert_eq!(ws.len(), 1);
        assert_eq!(ws[0].1, json!({ "triggers": { "mediaKeys": false } }));
        let d = c.build(&mut w.cx());
        assert!(!d.triggers.media_keys && d.saving);
        assert_eq!(d.status, "Saving");
        let rev = d.revision;
        w.apply(&mut c, ws[0].0, &ws[0].1, false);
        let d = c.build(&mut w.cx());
        assert!(d.triggers.media_keys, "reverted");
        assert!(!d.saving);
        assert_ne!(d.revision, rev);
        let (t, _) = &w.toasts.pushes[0];
        assert_eq!((t.id.as_str(), t.title.as_deref(), t.message.as_str()), ("settings-save-error", Some("Couldn\u{2019}t save that setting"), "disk full"));
    }

    #[test]
    fn an_unrelated_broadcast_keeps_the_overlay() {
        let (mut w, mut c) = entered();
        c.set_number(&mut w.cx(), "volume", 0.8f32);
        let cmds = w.sent();
        assert!(cmds.iter().any(|c| matches!(c, Command::Player(PlayerCommand::Volume { volume }) if *volume == 0.8)));
        let ws = writes(&cmds);
        assert_eq!(ws[0].1, json!({ "volume": 0.8 }));
        // A tray checkbox changes something else meanwhile.
        w.settings = Arc::new(w.settings.apply_patch(&json!({ "widget": { "enabled": false } })));
        { let u = UiUpdate::Settings(w.settings.clone()); c.on_update(&mut w.cx(), &u) };
        let d = c.build(&mut w.cx());
        assert_eq!((d.playback.volume, d.widget.enabled, d.saving), (0.8, false, true));
        // The same value again writes nothing.
        c.set_number(&mut w.cx(), "volume", 0.8f32 + f32::EPSILON);
        assert!(writes(&w.sent()).is_empty());
    }

    #[test]
    fn a_model_switch_resets_the_language_in_the_same_write() {
        let (mut w, mut c) = entered();
        let lang = |id: &str| ModelLanguage { id: id.into(), name: id.to_uppercase() };
        w.models = Arc::new(vec![
            TtsModel { model_id: "eleven_flash_v2_5".into(), name: "Eleven Flash v2.5".into(), description: Some("Fast.".into()), languages: vec![lang("en"), lang("de")], max_chars: Some(40_000.0), cost_factor: Some(0.5) },
            TtsModel { model_id: "eleven_english".into(), name: "Eleven English".into(), description: None, languages: vec![lang("en")], max_chars: None, cost_factor: Some(2.0) },
        ]);
        w.settings = Arc::new(w.settings.apply_patch(&json!({ "languageCode": "de" })));
        let d = c.build(&mut w.cx());
        assert_eq!(d.voice.language_options.row_count(), 3);
        assert_eq!(d.voice.language_index, 1, "Auto-detect, DE, EN sorted by name");
        assert_eq!(d.voice.model_description, "Fast.");
        assert_eq!(d.voice.model_badges.row_data(0).unwrap().text, "0.5\u{00D7} credits");
        c.set_choice(&mut w.cx(), "modelId", "eleven_english");
        assert_eq!(writes(&w.sent())[0].1, json!({ "modelId": "eleven_english", "languageCode": "" }));
        let d = c.build(&mut w.cx());
        assert_eq!(d.voice.model_description, MODEL_FALLBACK_DESCRIPTION);
        assert_eq!(d.voice.model_badges.row_data(0).unwrap().tone, BadgeTone::Attention, "2× credits is flagged");
        // A model that has the pinned language keeps it.
        c.set_choice(&mut w.cx(), "modelId", "eleven_flash_v2_5");
        assert_eq!(writes(&w.sent())[0].1, json!({ "modelId": "eleven_flash_v2_5" }));
    }

    #[test]
    fn slider_labels_and_quantized_writes() {
        let (mut w, mut c) = entered();
        assert_eq!(c.format_number("volume", 0.8f32), "80%");
        assert_eq!(c.format_number("voice.stability", 0.354f32), "35%");
        assert_eq!(c.format_number("voice.speed", 1.0799f32), "1.08\u{00D7}");
        assert_eq!(c.format_number("library.cacheLimitMB", 1490.0), "1.5 GB");
        assert_eq!(c.format_number("library.cacheLimitMB", 520.0), "500 MB");
        assert_eq!(c.format_number("nope", 1.0), "");
        c.set_number(&mut w.cx(), "voice.speed", 1.0799f32);
        c.set_number(&mut w.cx(), "library.cacheLimitMB", 1490.0);
        let ws = writes(&w.sent());
        assert_eq!(ws[0].1, json!({ "voice": { "speed": 1.08 } }));
        assert_eq!(ws[1].1, json!({ "library": { "cacheLimitMB": 1500 } }));
        // Reset writes the whole default voice object and overrides the sliders.
        let rev = c.revision;
        c.reset_voice_settings(&mut w.cx());
        let ws = writes(&w.sent());
        assert_eq!(ws[0].1["voice"]["similarityBoost"], json!(0.75));
        assert_ne!(c.revision, rev);
    }

    #[test]
    fn playback_rate_writes_and_applies_live() {
        let (mut w, mut c) = entered();
        c.set_choice(&mut w.cx(), "playbackRate", "1.25");
        let cmds = w.sent();
        assert_eq!(writes(&cmds)[0].1, json!({ "playbackRate": 1.25 }));
        assert!(cmds.iter().any(|c| matches!(c, Command::Player(PlayerCommand::Rate { rate }) if *rate == 1.25)));
        // A rate set elsewhere is offered too.
        w.settings = Arc::new(w.settings.apply_patch(&json!({ "playbackRate": 1.4 })));
        c.overlay.clear();
        let d = c.build(&mut w.cx());
        assert_eq!(d.playback.rate_options.row_count(), 7);
        assert_eq!(d.playback.rate_index, 3);
        assert_eq!(d.playback.rate_options.row_data(3).unwrap().label, "1.4\u{00D7}");
    }

    #[test]
    fn retention_guard_through_the_controller() {
        with_locale(Locale::EN_US, || {
            let (mut w, mut c) = entered();
            c.set_choice(&mut w.cx(), "library.retentionDays", "30");
            assert!(w.sent().is_empty(), "waits for the choice to settle");
            assert_eq!(c.build(&mut w.cx()).library.retention_index, 2, "the select shows the pending choice");
            w.now += 450.0;
            { let u = UiUpdate::AppInfo(w.app_info.clone()); c.on_update(&mut w.cx(), &u) };
            let req = w
                .sent()
                .into_iter()
                .find_map(|c| match c {
                    Command::CountExpiring { req, days: 30 } => Some(req),
                    _ => None,
                })
                .expect("count sent");
            c.on_update(&mut w.cx(), &UiUpdate::ExpiringCount { req, result: Ok(1204) });
            let spec = w.dialogs.open.clone().expect("dialog");
            assert_eq!(spec.id, "retention");
            assert_eq!(spec.title, "Delete 1,204 reads older than 30\u{00A0}days?");
            assert_eq!(spec.confirm_label, "Delete 1,204 reads");
            assert!(spec.danger);
            // Cancel reverts the select.
            let rev = c.revision;
            assert!(c.on_dialog(&mut w.cx(), "retention", false));
            assert_eq!(w.dialogs.close.as_deref(), Some("retention"));
            assert!(writes(&w.sent()).is_empty());
            assert_ne!(c.revision, rev);
            assert_eq!(c.build(&mut w.cx()).library.retention_index, 0);
            // A count failure toasts.
            c.set_choice(&mut w.cx(), "library.retentionDays", "7");
            w.now += 500.0;
            { let u = UiUpdate::AppInfo(w.app_info.clone()); c.on_update(&mut w.cx(), &u) };
            let req = w.sent().into_iter().find_map(|c| match c { Command::CountExpiring { req, .. } => Some(req), _ => None }).unwrap();
            c.on_update(&mut w.cx(), &UiUpdate::ExpiringCount { req, result: Err("boom".into()) });
            let (t, _) = w.toasts.pushes.last().unwrap();
            assert_eq!((t.id.as_str(), t.title.as_deref(), t.message.as_str()), ("library-retention", Some("Couldn\u{2019}t check your history"), "boom"));
        });
    }

    #[test]
    fn api_key_form_flow() {
        let (mut w, mut c) = entered();
        w.settings = Arc::new(Settings { has_api_key: false, ..(*w.settings).clone() });
        { let u = UiUpdate::Settings(w.settings.clone()); c.on_update(&mut w.cx(), &u) };
        w.sent();
        let d = c.build(&mut w.cx()).account;
        assert_eq!(d.state, AccountState::NoKey);
        assert!(d.form_open && !d.replacing);
        assert_eq!((d.badge.text.as_str(), d.badge.tone), ("No key", BadgeTone::Attention));
        assert_eq!((d.field_label.as_str(), d.submit_label.as_str(), d.lead.as_str()), ("API key", "Save key", KEY_LEAD));
        // Blank → message, nothing sent.
        c.save_key(&mut w.cx(), " \u{00A0} ");
        assert_eq!(c.build(&mut w.cx()).account.error, KEY_EMPTY);
        assert!(w.sent().is_empty());
        c.key_edited(&mut w.cx());
        assert_eq!(c.build(&mut w.cx()).account.error, "");
        // A key is trimmed and sent; the form shows busy.
        c.save_key(&mut w.cx(), "  sk_abc  ");
        let req = w.sent().into_iter().find_map(|c| match c { Command::SetApiKey { req, key } if key == "sk_abc" => Some(req), _ => None }).unwrap();
        assert!(c.build(&mut w.cx()).account.busy);
        // Rejected.
        let bad = ApiKeyResult { ok: false, error: None, warning: None, account: None };
        c.on_update(&mut w.cx(), &UiUpdate::ApiKey { req, result: bad });
        assert_eq!(c.build(&mut w.cx()).account.error, KEY_REJECTED);
        // Accepted with a warning: the core already published Settings with the key.
        c.save_key(&mut w.cx(), "sk_good");
        let req = w.sent().into_iter().find_map(|c| match c { Command::SetApiKey { req, .. } => Some(req), _ => None }).unwrap();
        w.settings = Arc::new(Settings { has_api_key: true, ..(*w.settings).clone() });
        { let u = UiUpdate::Settings(w.settings.clone()); c.on_update(&mut w.cx(), &u) };
        let ok = ApiKeyResult { ok: true, error: None, warning: Some("Limited.".into()), account: None };
        c.on_update(&mut w.cx(), &UiUpdate::ApiKey { req, result: ok });
        let d = c.build(&mut w.cx()).account;
        assert_eq!(d.state, AccountState::Stored);
        assert!(!d.form_open && !d.busy);
        assert_eq!(d.limited_warning, "Limited.");
        assert_eq!(w.fields.settings_key_draft.as_deref(), Some(""));
        let (t, _) = w.toasts.pushes.last().unwrap();
        assert_eq!((t.id.as_str(), t.message.as_str()), ("api-key", "API key saved"));
        // The key change refetched the catalog.
        assert!(w.sent().iter().any(|c| matches!(c, Command::FetchVoices { .. })));
        // Replace opens the form with the replace copy.
        c.open_key_form(&mut w.cx());
        let d = c.build(&mut w.cx()).account;
        assert!(d.form_open && d.replacing);
        c.cancel_key_form(&mut w.cx());
        assert!(!c.build(&mut w.cx()).account.form_open);
    }

    #[test]
    fn remove_key_dialog_flow() {
        let (mut w, mut c) = entered();
        c.remove_key(&mut w.cx());
        let spec = w.dialogs.open.clone().unwrap();
        assert_eq!((spec.id.as_str(), spec.title.as_str(), spec.confirm_label.as_str()), ("remove-key", "Remove API key?", "Remove key"));
        assert_eq!(spec.body, "Vox won\u{2019}t be able to read anything until you add a key again. History and cached audio stay.");
        assert!(c.on_dialog(&mut w.cx(), "remove-key", true));
        assert_eq!(w.dialogs.busy, Some(("remove-key".to_string(), true)));
        let req = w.sent().into_iter().find_map(|c| match c { Command::ClearApiKey { req } => Some(req), _ => None }).unwrap();
        // Cancel is ignored while removing.
        w.dialogs = DialogSink::default();
        assert!(c.on_dialog(&mut w.cx(), "remove-key", false));
        assert!(w.dialogs.close.is_none());
        c.on_update(&mut w.cx(), &UiUpdate::Done { req, result: Err("Error: access denied".into()) });
        assert_eq!(w.dialogs.busy, Some(("remove-key".to_string(), false)));
        let (t, _) = w.toasts.pushes.last().unwrap();
        assert_eq!((t.title.as_deref(), t.message.as_str()), (Some("Couldn\u{2019}t remove the key"), "access denied"));
        c.on_dialog(&mut w.cx(), "remove-key", true);
        let req = w.sent().into_iter().find_map(|c| match c { Command::ClearApiKey { req } => Some(req), _ => None }).unwrap();
        c.on_update(&mut w.cx(), &UiUpdate::Done { req, result: Ok(()) });
        assert_eq!(w.dialogs.close.as_deref(), Some("remove-key"));
        assert_eq!(w.toasts.pushes.last().unwrap().0.message, "API key removed");
    }

    #[test]
    fn excluded_apps_add_remove_and_errors() {
        let (mut w, mut c) = entered();
        c.add_excluded(&mut w.cx(), "   ");
        assert!(w.sent().is_empty());
        c.add_excluded(&mut w.cx(), "\"C:\\Program Files\\Foo\\Bar.EXE\"");
        assert_eq!(writes(&w.sent())[0].1, json!({ "triggers": { "excludedApps": ["bar.exe"] } }));
        assert_eq!(w.fields.settings_exclude_draft.as_deref(), Some(""));
        c.add_excluded(&mut w.cx(), "bar");
        assert_eq!(c.build(&mut w.cx()).triggers.exclude_error, "bar.exe is already excluded");
        c.excluded_edited(&mut w.cx());
        c.add_excluded(&mut w.cx(), "con .exe");
        assert_eq!(c.build(&mut w.cx()).triggers.exclude_error, "Enter a program name like notepad.exe");
        c.remove_excluded(&mut w.cx(), "bar.exe");
        assert_eq!(writes(&w.sent())[0].1, json!({ "triggers": { "excludedApps": [] } }));
    }

    #[test]
    fn port_field_and_recheck_window() {
        let (mut w, mut c) = entered();
        c.port_edited(&mut w.cx(), "5x1200");
        assert_eq!(w.fields.settings_port_draft.as_deref(), Some("51200"));
        c.commit_port(&mut w.cx(), "00080");
        assert_eq!(c.build(&mut w.cx()).integrations.port_error, PORT_INVALID);
        assert!(writes(&w.sent()).is_empty());
        c.revert_port(&mut w.cx());
        assert_eq!(w.fields.settings_port_draft.as_deref(), Some("47823"));
        c.commit_port(&mut w.cx(), "47823");
        assert!(writes(&w.sent()).is_empty(), "blur with the stored value writes nothing");
        c.commit_port(&mut w.cx(), "51200");
        let ws = writes(&w.sent());
        assert_eq!(ws[0].1, json!({ "server": { "port": 51200 } }));
        assert_eq!(c.build(&mut w.cx()).integrations.bridge_badge.text, "Checking\u{2026}");
        // A tick AppInfo before the change was saved does not end the window.
        { let u = UiUpdate::AppInfo(w.app_info.clone()); c.on_update(&mut w.cx(), &u) };
        assert_eq!(c.build(&mut w.cx()).integrations.bridge_badge.text, "Checking\u{2026}");
        w.apply(&mut c, ws[0].0, &ws[0].1, true);
        assert_eq!(c.build(&mut w.cx()).integrations.bridge_badge.text, "Checking\u{2026}");
        // The reaction's AppInfo: the bridge could not bind.
        w.app_info = Arc::new(AppInfo { server_running: false, server_port: 51200, ..(*w.app_info).clone() });
        { let u = UiUpdate::AppInfo(w.app_info.clone()); c.on_update(&mut w.cx(), &u) };
        let d = c.build(&mut w.cx()).integrations;
        assert_eq!((d.bridge_badge.text.as_str(), d.bridge_badge.tone), ("Not running", BadgeTone::Attention));
        assert_eq!(d.port_note, "Port 51200 may be in use by another app. Try another one.");
        // The window never sticks: 4 s at most.
        c.set_bool(&mut w.cx(), "triggers.explorerMenu", true);
        assert_eq!(c.build(&mut w.cx()).integrations.bridge_badge.text, "Checking\u{2026}");
        w.now += 4000.0;
        assert_eq!(c.build(&mut w.cx()).integrations.bridge_badge.text, "Not running");
        { let u = UiUpdate::Credits(w.credits.clone()); c.on_update(&mut w.cx(), &u) };
        assert!(w.sent().iter().any(|c| matches!(c, Command::QueryAppInfo)), "re-queried at the end of the window");
        // Bridge off.
        w.settings = Arc::new(w.settings.apply_patch(&json!({ "triggers": { "browserExtension": false } })));
        c.overlay.clear();
        assert_eq!(c.build(&mut w.cx()).integrations.bridge_badge.text, "Off");
    }

    #[test]
    fn a_failed_port_write_puts_the_stored_port_back_in_the_field() {
        let (mut w, mut c) = entered();
        c.commit_port(&mut w.cx(), "51200");
        assert_eq!(w.fields.settings_port_draft.take().as_deref(), Some("51200"));
        let ws = writes(&w.sent());
        // The core refuses the write (`Settings` unchanged, then `SettingsSaved{Err}`).
        w.apply(&mut c, ws[0].0, &ws[0].1, false);
        assert_eq!(w.fields.settings_port_draft.take().as_deref(), Some("47823"), "the field shows the stored port");
        assert_eq!(c.last_port, Some(47823));
        assert!(!c.port.error);
        // A later stored change still reaches the field (last_port was not left at the failed value).
        w.settings = Arc::new(w.settings.apply_patch(&json!({ "server": { "port": 51200 } })));
        { let u = UiUpdate::Settings(w.settings.clone()); c.on_update(&mut w.cx(), &u) };
        assert_eq!(w.fields.settings_port_draft.take().as_deref(), Some("51200"));
    }

    #[test]
    fn deadline_work_done_by_any_update_rebuilds_the_page() {
        let (mut w, mut c) = entered();
        c.copy_text(&mut w.cx(), "chrome://extensions");
        let req = w.sent().into_iter().find_map(|c| match c { Command::CopyText { req, .. } => Some(req), _ => None }).unwrap();
        assert!(c.on_update(&mut w.cx(), &UiUpdate::Done { req, result: Ok(()) }));
        // A player update lands after the "Copied" deadline but before the timer tick: the poll ends the chip and the
        // update reports the page dirty, so it is rebuilt without the tick.
        w.now += 1500.0;
        assert!(c.on_update(&mut w.cx(), &UiUpdate::Player(PlayerSnapshot::default())));
        assert_eq!(c.build(&mut w.cx()).integrations.copied, "");
        assert!(!c.on_update(&mut w.cx(), &UiUpdate::Player(PlayerSnapshot::default())), "nothing due: not dirty");
        // A reserved-chord rejection message ends the same way (6 s).
        c.hotkeys.reject(HotkeyAction::Stop, "Ctrl+C", w.now);
        w.now += HOTKEY_REJECT_MESSAGE_MS as f64;
        assert!(c.on_update(&mut w.cx(), &UiUpdate::HistoryChanged));
        assert_eq!(c.hotkeys.deadline(w.now), None);
        // Voices / Models answers while the page is away: not dirty, but the timer is still re-armed (no early return).
        c.on_leave(&mut w.cx());
        assert!(!c.on_update(&mut w.cx(), &UiUpdate::Voices { refresh: false, result: Ok(Arc::new(Vec::new())) }));
    }

    #[test]
    fn library_actions_have_their_own_busy_flags() {
        let (mut w, mut c) = entered();
        let stats_req = c.stats_req.unwrap();
        let d = c.build(&mut w.cx()).library;
        assert_eq!((d.usage_text.as_str(), d.cache_badge.as_str(), d.clear_cache_enabled), ("Measuring\u{2026}", "", false));
        c.on_update(&mut w.cx(), &UiUpdate::CacheStats { req: stats_req, result: Err("Error: nope".into()) });
        assert_eq!(c.build(&mut w.cx()).library.usage_text, "Couldn\u{2019}t read the cache size.");
        c.clear_cache(&mut w.cx());
        c.export_history(&mut w.cx(), Some(42));
        let cmds = w.sent();
        let cache_req = cmds.iter().find_map(|c| match c { Command::ClearCache { req } => Some(*req), _ => None }).unwrap();
        let export_req = cmds.iter().find_map(|c| match c { Command::ExportHistory { req, owner: Some(42) } => Some(*req), _ => None }).unwrap();
        let d = c.build(&mut w.cx()).library;
        assert!(d.clear_cache_busy && d.export_busy && !d.clear_history_busy);
        c.on_update(&mut w.cx(), &UiUpdate::CacheStats { req: cache_req, result: Ok(CacheStats::default()) });
        assert_eq!(w.toasts.pushes.last().unwrap().0.message, "Audio cache cleared");
        let d = c.build(&mut w.cx()).library;
        assert!(!d.clear_cache_busy && d.export_busy);
        assert_eq!(d.usage_text, "0 MB of 500 MB \u{00B7} 0 reads cached");
        assert_eq!(d.cache_badge, "0MB");
        c.on_update(&mut w.cx(), &UiUpdate::Exported { req: export_req, result: Ok(Some("C:\\x\\vox-history.json".into())) });
        let (t, _) = w.toasts.pushes.last().unwrap();
        assert_eq!((t.id.as_str(), t.title.as_deref(), t.message.as_str()), ("library-export", Some("History exported"), "C:\\x\\vox-history.json"));
        // Clear history: dialog, busy, stays open on failure.
        c.clear_history(&mut w.cx());
        assert_eq!(w.dialogs.open.as_ref().unwrap().title, "Clear history?");
        c.on_dialog(&mut w.cx(), "library-clear-history", true);
        let req = w.sent().into_iter().find_map(|c| match c { Command::ClearHistory { req } => Some(req), _ => None }).unwrap();
        assert!(c.build(&mut w.cx()).library.clear_history_busy);
        w.dialogs = DialogSink::default();
        c.on_update(&mut w.cx(), &UiUpdate::Done { req, result: Err("busy".into()) });
        assert!(w.dialogs.close.is_none());
        assert_eq!(w.dialogs.busy, Some(("library-clear-history".to_string(), false)));
        // The history changed: re-measure 2.5 s later.
        c.on_update(&mut w.cx(), &UiUpdate::HistoryChanged);
        w.sent();
        w.now += 2500.0;
        c.on_update(&mut w.cx(), &UiUpdate::Refresh(crate::update::RefreshReason::Day));
        assert!(w.sent().iter().any(|c| matches!(c, Command::QueryCacheStats { .. })));
    }

    #[test]
    fn cache_slider_above_5000_shows_the_real_value() {
        let (mut w, c) = entered();
        w.settings = Arc::new(w.settings.apply_patch(&json!({ "library": { "cacheLimitMB": 6000 } })));
        let d = c.build(&mut w.cx()).library;
        assert_eq!((d.cache_limit, d.cache_limit_max, d.cache_limit_text.as_str()), (6000.0, 6000.0, "6 GB"));
    }

    #[test]
    fn copy_chips_show_copied_for_1_4_s() {
        let (mut w, mut c) = entered();
        c.copy_text(&mut w.cx(), "chrome://extensions");
        let req = w.sent().into_iter().find_map(|c| match c { Command::CopyText { req, .. } => Some(req), _ => None }).unwrap();
        c.on_update(&mut w.cx(), &UiUpdate::Done { req, result: Ok(()) });
        assert_eq!(c.build(&mut w.cx()).integrations.copied, "chrome://extensions");
        w.now += 1400.0;
        assert_eq!(c.build(&mut w.cx()).integrations.copied, "");
        c.copy_text(&mut w.cx(), "edge://extensions");
        let req = w.sent().into_iter().find_map(|c| match c { Command::CopyText { req, .. } => Some(req), _ => None }).unwrap();
        c.on_update(&mut w.cx(), &UiUpdate::Done { req, result: Err("x".into()) });
        let (t, _) = w.toasts.pushes.last().unwrap();
        assert_eq!((t.id.as_str(), t.message.as_str()), ("copy", "Couldn\u{2019}t copy to the clipboard"));
    }

    #[test]
    fn hotkey_rows_follow_the_recorder() {
        let (mut w, mut c) = entered();
        c.hotkey_start(&mut w.cx(), "playPause");
        assert!(w.sent().iter().any(|c| matches!(c, Command::SuspendHotkeys(true))));
        let rows = c.build(&mut w.cx()).triggers.hotkeys;
        assert!(rows.row_data(2).unwrap().recording);
        assert!(!rows.row_data(0).unwrap().recording);
        // A rejection from the recorder shows under its row for 6 s.
        hotkey_recorder::push_event_for_tests(RecorderEvent::Rejected { action: HotkeyAction::PlayPause, accel: "Ctrl+V".into() });
        { let u = UiUpdate::AppInfo(w.app_info.clone()); c.on_update(&mut w.cx(), &u) };
        let row = c.build(&mut w.cx()).triggers.hotkeys.row_data(2).unwrap();
        assert!(row.warning.starts_with("Ctrl+V is used by Windows"));
        // Clearing writes "" and drops the rejection.
        c.hotkey_clear(&mut w.cx(), "playPause");
        assert_eq!(writes(&w.sent())[0].1, json!({ "triggers": { "hotkeys": { "playPause": "" } } }));
        assert_eq!(c.build(&mut w.cx()).triggers.hotkeys.row_data(2).unwrap().keys.row_count(), 0);
        // Leaving the page stops recording.
        c.on_leave(&mut w.cx());
        assert!(w.sent().iter().any(|c| matches!(c, Command::SuspendHotkeys(false))));
        // Shortcuts off: a start is declined.
        let (mut w, mut c) = entered();
        w.settings = Arc::new(w.settings.apply_patch(&json!({ "triggers": { "hotkeysEnabled": false } })));
        c.hotkey_start(&mut w.cx(), "stop");
        assert!(w.sent().is_empty());
    }

    #[test]
    fn hook_notice_demo_account_and_about_variants() {
        let (mut w, c) = entered();
        w.app_info = Arc::new(AppInfo {
            native_hook_available: false,
            is_packaged: false,
            build: "dev".into(),
            demo_mode: true,
            api_key_notice: Some("Vox couldn\u{2019}t bring over the API key.".into()),
            ..(*w.app_info).clone()
        });
        w.settings = Arc::new(Settings { has_api_key: false, ..(*w.settings).clone() });
        let d = c.build(&mut w.cx());
        assert!(!d.triggers.hook_available);
        assert_eq!(d.triggers.hook_notice_title, vox_core::copy::HOOK_UNAVAILABLE_TITLE);
        assert_eq!(d.account.state, AccountState::Demo);
        assert_eq!((d.account.badge.text.as_str(), d.account.badge.tone), ("Demo account", BadgeTone::Neutral));
        assert!(!d.account.form_open);
        assert_eq!(d.account.notice, "Vox couldn\u{2019}t bring over the API key.");
        assert_eq!(d.about.version, "1.0.0 \u{00B7} dev");
        assert!(d.about.demo);
        // Nothing loaded yet.
        w.app_info = Arc::new(AppInfo::default());
        let a = c.build(&mut w.cx()).about;
        assert_eq!((a.version.as_str(), a.runtime.as_str(), a.data_folder.as_str(), a.data_folder_ready), ("\u{2026}", "\u{2026}", "Settings, history and cached audio", false));
    }

    #[test]
    fn current_voice_tags_rules() {
        let v = Voice {
            voice_id: "g".into(),
            name: "George".into(),
            labels: vec![("accent".into(), "british".into()), ("gender".into(), "male".into()), ("use_case".into(), "narration".into())],
            category: Some("cloned".into()),
            description: None,
            preview_url: None,
        };
        let (tags, loading) = current_voice_tags("g", Some(std::slice::from_ref(&v)), false);
        assert!(!loading);
        assert_eq!(tags[0], "Cloned");
        assert!(tags.len() >= 2 && tags.len() <= 5);
        assert_eq!(current_voice_tags("x", None, true), (vec![], true));
        assert_eq!(current_voice_tags("x", Some(std::slice::from_ref(&v)), false), (vec!["ElevenLabs voice".to_string()], false));
    }

    #[test]
    fn test_voice_reads_the_sample_and_toasts_only_transport_failures() {
        let (mut w, mut c) = entered();
        c.test_voice(&mut w.cx());
        let (req, text, options) = w
            .sent()
            .into_iter()
            .find_map(|c| match c { Command::ReadText { req: Some(r), text, options } => Some((r, text, options)), _ => None })
            .unwrap();
        assert!(text.starts_with("Hi, I\u{2019}m George."));
        assert_eq!((options.source, options.source_app.as_deref()), (Some(ReadSource::Manual), Some("Vox")));
        assert!(c.build(&mut w.cx()).voice.test_busy);
        c.on_update(&mut w.cx(), &UiUpdate::ReadStarted { req, result: ReadResult::no_api_key() });
        assert!(!c.build(&mut w.cx()).voice.test_busy);
        assert!(w.toasts.pushes.is_empty(), "the core toasts ordinary failures");
        c.test_voice(&mut w.cx());
        let req = w.sent().into_iter().find_map(|c| match c { Command::ReadText { req: Some(r), .. } => Some(r), _ => None }).unwrap();
        c.on_update(&mut w.cx(), &UiUpdate::ReadStarted { req, result: ReadResult::internal_error() });
        assert_eq!(w.toasts.pushes.last().unwrap().0.id, "voice-test");
    }
}
