//! Onboarding controller (ui-settings-widget.md §5, ui-main.md §10): steps welcome → key → voice → try → done,
//! furthest step, next label (`Get started` / `Skip for now` on the key step without a key / `Continue` / `Open Vox`),
//! the key step (connect → `SetApiKey` with the raw draft trimmed in Rust: blank → `Paste your API key first.`;
//! connected caption `{Tier} plan · {compact} characters left ({pct}%)` or `Ready to read.`; warning; the draft is the
//! main window's in-out `onboarding-key-draft`, cleared through `FieldWrites` after a successful connect), the voice
//! step (API order, `FetchVoices`, choose → `UpdateSettings`, previews; `PreviewUpdate::Failed` → toast
//! `voice-preview`), the try step (`copy::onboarding_sample`; `ReadText` source manual; caption while that read plays),
//! done tips, finish (`UpdateSettings{req, {onboarded: true}}`, then Home on `SettingsSaved{req, Ok}`; `Err` → toast
//! `Couldn't save your setup: …`). "Get a key" → `OpenExternal{url, req}` (failure → toast `open-external`).
//!
//! ## Implementation notes
//!
//! - **Callbacks → methods**: `onboarding-go-to(i)` → [`OnboardingController::go_to`], `-next` → `next`, `-back` →
//!   `back`, `-skip` → `skip`, `-connect-key(draft)` → `connect_key`, `-key-edited` → `key_edited`, `-get-key` →
//!   `get_key`, `-replace-key` → `replace_key`, `-cancel-replace` → `cancel_replace`, `-choose-voice(id)` →
//!   `choose_voice`, `-preview-voice(id)` → `preview_voice`, `-retry-voices` → `retry_voices`, `-read-sample` →
//!   `read_sample`. The try step's "Add a key" is Slint-local (`go-to(1)`).
//! - **Entering**: a fresh entry starts on the welcome step (or the step named by an `onboarding/<step>` route); a
//!   navigation to `settings/account` while onboarding (e.g. a core toast's "Add key") re-enters the page and jumps to
//!   the key step (the shell keeps showing onboarding whatever the route says).
//! - Decisions applied: D-USW-2 (API order, compact rows, the shared preview engine via `PreviewVoice`, which pauses a
//!   reading), D-USW-3 (demo counts as "can read" on the Voice and Try steps: the core serves demo voices without a
//!   key), D-USW-4 (`’` everywhere, also in `Couldn’t save your setup` / `Couldn’t select …`), D-USW-14 (errors through
//!   `clean_error`), D-USW-19 ("Get a key" failures toast `open-external`; Cancel is refused while connecting),
//!   D-USW-24 (`PreviewUpdate::Failed` carries the toast text).
//! - The connect result (`{warning, account}`) lives only while the key step is on screen; leaving the step forgets it
//!   (and the error and replace mode), so coming back shows the shell's credits and no warning (spec §5.3).
//! - Art: steps 2–3 show the current voice's art (seed = voice id); welcome, key and done have fixed seeds.
//! - Settings writes go straight to the core (no optimistic overlay, no Saving badge, spec §5.2). Finish waits for its
//!   own `SettingsSaved{req}` before navigating Home (the `Settings` broadcast that precedes it flips the shell's
//!   onboarding gate).

use std::sync::Arc;

use serde_json::json;
use slint::{ModelRc, SharedString, VecModel};
use vox_core::app::{AppPage, AppRoute, Toast};
use vox_core::constants::{APP_NAME, ELEVENLABS_API_KEYS_URL};
use vox_core::format;
use vox_core::history::ReadSource;
use vox_core::js::{capitalize_first, js_trim};
use vox_core::player::PlayerStatus;
use vox_core::read::{ReadResult, ReadTextOptions};
use vox_core::voices::{AccountInfo, ApiKeyResult, Voice};

use super::art::ArtSize;
use super::convert::{keycaps, strings};
use super::generated::{KeyGroup, KeyStepData, LoadState, OnboardingData, PreviewState, TryStepData, VoiceRow, VoiceStepData};
use super::shell::read_selection_hotkey;
use super::{NavIntent, PageController, UiCtx};
use crate::command::{Command, RequestId};
use crate::update::{CreditsView, PreviewUpdate, UiUpdate};

/// Step labels (dots, `Step {n}: {label}`), in order.
pub const STEP_LABELS: [&str; 5] = ["Welcome", "API key", "Voice", "Try it", "Done"];
/// Step ids (an `onboarding/<id>` route opens on that step).
pub const STEP_IDS: [&str; 5] = ["welcome", "key", "voice", "try", "done"];
const STEP_KEY: usize = 1;
const STEP_VOICE: usize = 2;
const STEP_TRY: usize = 3;
const STEP_DONE: usize = 4;

const KEY_HINT: &str = "Find it under Settings \u{2192} API keys on elevenlabs.io.";
const KEY_EMPTY: &str = "Paste your API key first.";
const KEY_REJECTED: &str = "ElevenLabs rejected that key.";
const READY_TO_READ: &str = "Ready to read.";
const CAPTION_PLAYING: &str = "Playing in the floating player\u{2026}";
const CAPTION_IDLE: &str = "Or hear it right here:";

/// The connect result, remembered while the key step is on screen.
#[derive(Debug, Clone, PartialEq)]
struct Connected {
    warning: Option<String>,
    account: Option<AccountInfo>,
}

/// The voice list (previous data kept through loading and errors).
#[derive(Debug, Clone, Default)]
struct VoiceList {
    data: Option<Arc<Vec<Voice>>>,
    loading: bool,
    error: Option<String>,
}

/// The connected caption (ui-main.md §10.2): `{Tier} plan · {compact(remaining)} characters left ({pct}%)`, the tier
/// with only its first letter upper-cased and omitted (with ` plan · `) when empty.
pub fn connected_caption(account: Option<&AccountInfo>) -> String {
    let Some(a) = account else { return READY_TO_READ.into() };
    let count = if a.character_count.is_finite() { a.character_count } else { 0.0 };
    let limit = if a.character_limit.is_finite() { a.character_limit } else { 0.0 };
    let remaining = (limit - count).max(0.0);
    let pct = format::percent(format::credits_left_fraction(count, limit));
    let tail = format!("{} characters left ({pct})", format::format_compact(remaining));
    let tier = capitalize_first(js_trim(&a.tier));
    if tier.is_empty() {
        tail
    } else {
        format!("{tier} plan \u{00B7} {tail}")
    }
}

/// The onboarding voice row's tag line (spec §5.3): `[accent, gender, use_case ?? "use case", age]`, non-empty ones,
/// first 3, raw values joined ` · `; else the category, else `Voice`.
pub fn voice_line(v: &Voice) -> String {
    let use_case = v.label("use_case").or_else(|| v.label("use case"));
    let parts: Vec<&str> = [v.label("accent"), v.label("gender"), use_case, v.label("age")]
        .into_iter()
        .flatten()
        .map(js_trim)
        .filter(|s| !s.is_empty())
        .take(3)
        .collect();
    if !parts.is_empty() {
        return parts.join(" \u{00B7} ");
    }
    match v.category.as_deref().map(js_trim).filter(|c| !c.is_empty()) {
        Some(c) => c.to_string(),
        None => "Voice".into(),
    }
}

/// The art seed of a step: the current voice on the voice and try steps, a fixed seed elsewhere.
fn step_art_seed(step: usize, voice_id: &str) -> &str {
    match step {
        0 => "vox-onboarding-welcome",
        STEP_KEY => "vox-onboarding-key",
        STEP_VOICE | STEP_TRY => voice_id,
        _ => "vox-onboarding-done",
    }
}

#[derive(Default)]
pub struct OnboardingController {
    step: usize,
    furthest: usize,
    /// The page is on screen.
    active: bool,
    // ── key step ──
    key_req: Option<RequestId>,
    key_error: Option<String>,
    replacing: bool,
    connected: Option<Connected>,
    get_key_req: Option<RequestId>,
    // ── voice step ──
    voices: VoiceList,
    /// The sample being loaded / played: (voice id, state).
    preview: Option<(String, PreviewState)>,
    /// A voice choice in flight: (request, voice name).
    choose_req: Option<(RequestId, String)>,
    // ── try step ──
    read_req: Option<RequestId>,
    /// The read id of the sample started here (the caption follows only that read).
    sample_read_id: Option<String>,
    sample_playing: bool,
    // ── frame ──
    finish_req: Option<RequestId>,
    last_has_key: Option<bool>,
}

impl OnboardingController {
    fn can_read(cx: &UiCtx) -> bool {
        cx.settings.has_api_key || cx.demo || cx.app_info.demo_mode
    }

    /// Voices are fetched with a key (or in demo mode, D-USW-3); without, the previous data goes.
    fn fetch_voices(&mut self, cx: &mut UiCtx, refresh: bool) {
        if Self::can_read(cx) {
            self.voices.loading = true;
            self.voices.error = None;
            cx.core.send(Command::FetchVoices { refresh });
        } else {
            self.voices = VoiceList::default();
        }
    }

    fn stop_preview(&mut self, cx: &mut UiCtx) {
        if self.preview.take().is_some() {
            cx.core.send(Command::StopPreview);
        }
    }

    /// Moves to `target` (clamped), with the per-step enter/leave work.
    fn set_step(&mut self, cx: &mut UiCtx, target: usize) {
        let target = target.min(STEP_DONE);
        let leaving = self.step;
        if leaving == STEP_KEY && target != STEP_KEY {
            // The connect result lives only while the step is on screen.
            self.connected = None;
            self.key_error = None;
            self.replacing = false;
        }
        if leaving == STEP_VOICE && target != STEP_VOICE {
            self.stop_preview(cx);
        }
        self.step = target;
        self.furthest = self.furthest.max(target);
        if target == STEP_VOICE && leaving != STEP_VOICE {
            self.fetch_voices(cx, false);
        }
    }

    fn finish(&mut self, cx: &mut UiCtx) {
        if self.finish_req.is_some() {
            return;
        }
        let req = cx.core.request_id();
        self.finish_req = Some(req);
        cx.core.send(Command::UpdateSettings { req: Some(req), patch: json!({ "onboarded": true }) });
    }

    /// The sample read started here is playing or loading.
    fn sample_is_playing(&self, cx: &UiCtx) -> bool {
        let state = &cx.player.state;
        self.sample_read_id.is_some()
            && state.read_id == self.sample_read_id
            && matches!(state.status, PlayerStatus::Playing | PlayerStatus::Loading)
    }

    pub fn go_to(&mut self, cx: &mut UiCtx, step: i32) {
        // A dot is clickable only when it is not the current step and at most the furthest visited one.
        let Ok(target) = usize::try_from(step) else { return };
        if target == self.step || target > self.furthest || target > STEP_DONE {
            return;
        }
        self.set_step(cx, target);
    }
    pub fn next(&mut self, cx: &mut UiCtx) {
        if self.step >= STEP_DONE {
            self.finish(cx);
        } else {
            self.set_step(cx, self.step + 1);
        }
    }
    pub fn back(&mut self, cx: &mut UiCtx) {
        if self.step > 0 && self.step < STEP_DONE {
            self.set_step(cx, self.step - 1);
        }
    }
    pub fn skip(&mut self, cx: &mut UiCtx) {
        self.finish(cx);
    }
    pub fn connect_key(&mut self, cx: &mut UiCtx, draft: &str) {
        if self.key_req.is_some() {
            return;
        }
        let key = js_trim(draft);
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
    pub fn get_key(&mut self, cx: &mut UiCtx) {
        let req = cx.core.request_id();
        self.get_key_req = Some(req);
        cx.core.send(Command::OpenExternal { url: ELEVENLABS_API_KEYS_URL.into(), req: Some(req) });
    }
    pub fn replace_key(&mut self, cx: &mut UiCtx) {
        let _ = cx;
        self.replacing = true;
        self.key_error = None;
    }
    pub fn cancel_replace(&mut self, cx: &mut UiCtx) {
        let _ = cx;
        // Refused while connecting (D-USW-19); the draft is kept.
        if self.key_req.is_some() {
            return;
        }
        self.replacing = false;
        self.key_error = None;
    }
    pub fn choose_voice(&mut self, cx: &mut UiCtx, voice_id: &str) {
        if voice_id.is_empty() || voice_id == cx.settings.voice_id {
            return;
        }
        let Some(name) = self.voices.data.as_ref().and_then(|l| l.iter().find(|v| v.voice_id == voice_id)).map(|v| v.name.clone())
        else {
            return;
        };
        let req = cx.core.request_id();
        self.choose_req = Some((req, name.clone()));
        cx.core.send(Command::UpdateSettings { req: Some(req), patch: json!({ "voiceId": voice_id, "voiceName": name }) });
    }
    pub fn preview_voice(&mut self, cx: &mut UiCtx, voice_id: &str) {
        if voice_id.is_empty() {
            return;
        }
        // The core toggles: the same voice stops, another one replaces it (and a reading is paused, D-USW-2).
        cx.core.send(Command::PreviewVoice { voice_id: voice_id.to_string() });
    }
    pub fn retry_voices(&mut self, cx: &mut UiCtx) {
        self.fetch_voices(cx, true);
    }
    pub fn read_sample(&mut self, cx: &mut UiCtx) {
        if self.read_req.is_some() || !Self::can_read(cx) {
            return;
        }
        let req = cx.core.request_id();
        self.read_req = Some(req);
        cx.core.send(Command::ReadText {
            req: Some(req),
            text: vox_core::copy::onboarding_sample(&cx.settings.voice_name),
            options: ReadTextOptions { source: Some(ReadSource::Manual), ..ReadTextOptions::default() },
        });
    }

    // ───────────── answers ─────────────

    fn on_api_key(&mut self, cx: &mut UiCtx, req: RequestId, result: &ApiKeyResult) -> bool {
        if self.key_req != Some(req) {
            return false;
        }
        self.key_req = None;
        if result.ok {
            self.key_error = None;
            self.replacing = false;
            self.connected = Some(Connected {
                warning: result.warning.clone().filter(|w| !w.trim().is_empty()),
                account: result.account.clone(),
            });
            cx.fields.onboarding_key_draft = Some(String::new());
        } else {
            let error = result.error.as_deref().map(str::trim).filter(|e| !e.is_empty()).map(format::clean_error);
            self.key_error = Some(error.unwrap_or_else(|| KEY_REJECTED.into()));
        }
        true
    }

    fn on_saved(&mut self, cx: &mut UiCtx, req: RequestId, result: &Result<(), String>) -> bool {
        if self.finish_req == Some(req) {
            self.finish_req = None;
            match result {
                Ok(()) => cx.nav.navigate(AppRoute::page(AppPage::Home)),
                Err(e) => cx.toasts.push(Toast::error(format!("Couldn\u{2019}t save your setup: {}", format::clean_error(e))), None),
            }
            return true;
        }
        if self.choose_req.as_ref().is_some_and(|(r, _)| *r == req) {
            if let (Some((_, name)), Err(e)) = (self.choose_req.take(), result) {
                cx.toasts.push(Toast::error(format!("Couldn\u{2019}t select {name}: {}", format::clean_error(e))), None);
            }
            return true;
        }
        false
    }

    fn on_preview(&mut self, cx: &mut UiCtx, update: &PreviewUpdate) -> bool {
        match update {
            PreviewUpdate::Loading(id) => self.preview = Some((id.clone(), PreviewState::Loading)),
            PreviewUpdate::Playing(id) => self.preview = Some((id.clone(), PreviewState::Playing)),
            PreviewUpdate::Stopped => self.preview = None,
            PreviewUpdate::Failed { message, .. } => {
                self.preview = None;
                cx.toasts.push(Toast::error(message.clone()).with_id("voice-preview"), None);
            }
        }
        true
    }

    fn on_read_started(&mut self, cx: &mut UiCtx, req: RequestId, result: &ReadResult) -> bool {
        if self.read_req != Some(req) {
            return false;
        }
        self.read_req = None;
        if result.ok {
            if let Some(id) = result.read_id.clone() {
                self.sample_read_id = Some(id);
            }
        } else if *result == ReadResult::internal_error() {
            // Read failures are toasted by the core; only the transport failure is toasted here.
            let message = format::clean_error(result.error.as_deref().unwrap_or_default());
            cx.toasts.push(Toast::error(message).with_id("read-failed"), None);
        }
        self.sample_playing = self.sample_is_playing(cx);
        true
    }

    // ───────────── data ─────────────

    fn key_data(&self, cx: &UiCtx) -> KeyStepData {
        let has_key = cx.settings.has_api_key;
        let (caption, warning) = if has_key {
            let account = match &self.connected {
                Some(Connected { account: Some(a), .. }) => Some(a.clone()),
                _ => match &cx.credits.view {
                    CreditsView::Ready(a) => Some(a.clone()),
                    _ => None,
                },
            };
            let warning = self.connected.as_ref().and_then(|c| c.warning.clone()).unwrap_or_default();
            (connected_caption(account.as_ref()), warning)
        } else {
            (String::new(), String::new())
        };
        KeyStepData {
            has_key,
            form_visible: !has_key || self.replacing,
            replacing: self.replacing && has_key,
            busy: self.key_req.is_some(),
            error: self.key_error.clone().unwrap_or_default().into(),
            hint: KEY_HINT.into(),
            connected_caption: caption.into(),
            warning: warning.into(),
            notice: cx.app_info.api_key_notice.clone().unwrap_or_default().into(),
        }
    }

    fn voice_rows(&self, cx: &mut UiCtx, list: &[Voice]) -> ModelRc<VoiceRow> {
        let current = cx.settings.voice_id.clone();
        let rows: Vec<VoiceRow> = list
            .iter()
            .map(|v| {
                let preview = if v.preview_url.as_deref().is_none_or(|u| u.trim().is_empty()) {
                    PreviewState::None
                } else {
                    match &self.preview {
                        Some((id, state)) if *id == v.voice_id => *state,
                        _ => PreviewState::Ready,
                    }
                };
                VoiceRow {
                    id: v.voice_id.as_str().into(),
                    name: v.name.as_str().into(),
                    art: cx.art.image(&v.voice_id, ArtSize::Compact),
                    subtitle: voice_line(v).into(),
                    description: SharedString::new(),
                    tags: ModelRc::default(),
                    caption: SharedString::new(),
                    in_use: v.voice_id == current,
                    preview,
                }
            })
            .collect();
        ModelRc::new(VecModel::from(rows))
    }

    fn voices_data(&self, cx: &mut UiCtx) -> VoiceStepData {
        let state = |state: LoadState, title: &str, body: String| VoiceStepData {
            state,
            title: title.into(),
            body: body.into(),
            items: ModelRc::default(),
        };
        if !Self::can_read(cx) {
            let name = js_trim(&cx.settings.voice_name);
            let name = if name.is_empty() { "The default voice" } else { name };
            return state(LoadState::Empty, "Connect a key to browse voices", format!("{name} will read for you until then."));
        }
        match (&self.voices.data, &self.voices.error) {
            (Some(list), _) if list.is_empty() => {
                state(LoadState::Empty, "No voices on this account", "Add voices on elevenlabs.io, then come back.".into())
            }
            (Some(list), _) => {
                let list = list.clone();
                VoiceStepData { state: LoadState::Ready, title: SharedString::new(), body: SharedString::new(), items: self.voice_rows(cx, &list) }
            }
            (None, Some(e)) if !self.voices.loading => state(LoadState::Error, "Couldn\u{2019}t load voices", format::clean_error(e)),
            (None, _) => state(LoadState::Loading, "", String::new()),
        }
    }

    fn try_data(&self, cx: &UiCtx) -> TryStepData {
        let can_read = Self::can_read(cx);
        TryStepData {
            can_read,
            caption: if self.sample_is_playing(cx) { CAPTION_PLAYING } else { CAPTION_IDLE }.into(),
            busy: self.read_req.is_some(),
            sample: vox_core::copy::onboarding_sample(&cx.settings.voice_name).into(),
            keys: keycaps(read_selection_hotkey(cx.settings)),
        }
    }

    fn tips(cx: &UiCtx) -> ModelRc<KeyGroup> {
        let mut tips =
            vec![KeyGroup { label: format!("Click the tray icon to open {APP_NAME}").into(), keys: ModelRc::default() }];
        let t = &cx.settings.triggers;
        if t.hotkeys_enabled {
            for (label, accel) in [("Read selection", t.hotkeys.read_selection.trim()), ("Play / pause", t.hotkeys.play_pause.trim())] {
                if !accel.is_empty() {
                    tips.push(KeyGroup { label: label.into(), keys: keycaps(accel) });
                }
            }
        }
        ModelRc::new(VecModel::from(tips))
    }
}

impl PageController for OnboardingController {
    type Data = OnboardingData;

    fn on_enter(&mut self, cx: &mut UiCtx, route: &AppRoute, intent: &NavIntent) {
        let _ = intent;
        if !self.active {
            // A fresh flow (the old app mounted the page anew).
            let voices = std::mem::take(&mut self.voices.data);
            *self = OnboardingController::default();
            self.voices.data = voices;
            self.active = true;
            self.last_has_key = Some(cx.settings.has_api_key);
            if route.page == AppPage::Onboarding {
                if let Some(i) = route.section.as_deref().and_then(|s| STEP_IDS.iter().position(|id| *id == s)) {
                    self.set_step(cx, i);
                }
            }
        }
        // While onboarding, `settings/account` (a toast's "Add key") jumps to the key step.
        if route.page == AppPage::Settings && route.section.as_deref() == Some("account") && self.step != STEP_KEY {
            self.set_step(cx, STEP_KEY);
        }
    }

    fn on_leave(&mut self, cx: &mut UiCtx) {
        self.stop_preview(cx);
        self.active = false;
        self.connected = None;
        self.replacing = false;
        self.key_error = None;
    }

    fn on_update(&mut self, cx: &mut UiCtx, update: &UiUpdate) -> bool {
        match update {
            UiUpdate::Settings(s) => {
                if self.last_has_key != Some(s.has_api_key) {
                    self.last_has_key = Some(s.has_api_key);
                    if !s.has_api_key {
                        self.connected = None;
                        self.replacing = false;
                    }
                    if self.active && self.step == STEP_VOICE {
                        // Re-fetched when the key appears; dropped when it goes (demo keeps its list).
                        self.fetch_voices(cx, false);
                    }
                }
                true
            }
            UiUpdate::SettingsSaved { req, result } => self.on_saved(cx, *req, result),
            UiUpdate::ApiKey { req, result } => self.on_api_key(cx, *req, result),
            UiUpdate::Done { req, result } => {
                if self.get_key_req != Some(*req) {
                    return false;
                }
                self.get_key_req = None;
                if let Err(e) = result {
                    cx.toasts.push(Toast::error(vox_core::copy::open_link_failed(e)).with_id("open-external"), None);
                }
                false
            }
            UiUpdate::Voices { result, .. } => {
                if !self.active {
                    return false;
                }
                self.voices.loading = false;
                match result {
                    Ok(list) => {
                        self.voices.data = Some(list.clone());
                        self.voices.error = None;
                    }
                    Err(e) => self.voices.error = Some(e.clone()),
                }
                true
            }
            // Previews reach every page: only the page on screen shows them (the Voices page toasts its own failures).
            UiUpdate::Preview(p) => self.active && self.on_preview(cx, p),
            UiUpdate::ReadStarted { req, result } => self.on_read_started(cx, *req, result),
            UiUpdate::Player(_) => {
                let playing = self.sample_is_playing(cx);
                let changed = playing != self.sample_playing;
                self.sample_playing = playing;
                changed
            }
            UiUpdate::Credits(_) | UiUpdate::AppInfo(_) | UiUpdate::Refresh(_) => true,
            _ => false,
        }
    }

    fn build(&self, cx: &mut UiCtx) -> OnboardingData {
        let step = self.step.min(STEP_DONE);
        let has_key = cx.settings.has_api_key;
        let (next_label, next_secondary) = match step {
            0 => ("Get started".to_string(), false),
            STEP_KEY if !has_key => ("Skip for now".to_string(), true),
            STEP_DONE => (format!("Open {APP_NAME}"), false),
            _ => ("Continue".to_string(), false),
        };
        let voice_id = cx.settings.voice_id.clone();
        OnboardingData {
            step: step as i32,
            furthest: self.furthest.max(step) as i32,
            step_labels: strings(STEP_LABELS.iter().map(|s| s.to_string())),
            crumb: if step == 0 { SharedString::new() } else { format!("Setup {}/{}", step + 1, STEP_LABELS.len()).into() },
            next_label: next_label.into(),
            next_secondary,
            show_back: step != 0 && step != STEP_DONE,
            show_skip: step != STEP_DONE,
            finishing: self.finish_req.is_some(),
            art: cx.art.image(step_art_seed(step, &voice_id), ArtSize::Large),
            key: self.key_data(cx),
            voices: self.voices_data(cx),
            try_it: self.try_data(cx),
            tips: Self::tips(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::CoreHandle;
    use crate::ui::art::ArtCache;
    use crate::ui::{DialogSink, FieldWrites, NavSink, ToastSink};
    use crate::update::CreditsUpdate;
    use crossbeam_channel::{unbounded, Receiver};
    use slint::Model;
    use vox_core::app::AppInfo;
    use vox_core::locale::{with_locale, Locale};
    use vox_core::player::PlayerSnapshot;
    use vox_core::settings::Settings;

    struct World {
        core: CoreHandle,
        rx: Receiver<Command>,
        settings: Arc<Settings>,
        player: PlayerSnapshot,
        credits: CreditsUpdate,
        app_info: Arc<AppInfo>,
        models: Arc<Vec<vox_core::voices::TtsModel>>,
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
            World {
                core: CoreHandle::new(tx),
                rx,
                settings: Arc::new(Settings::default()),
                player: PlayerSnapshot::default(),
                credits: CreditsUpdate { view: CreditsView::NoKey, refreshing: false },
                app_info: Arc::new(AppInfo::default()),
                models: Arc::new(Vec::new()),
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
                now_ms: 1_792_243_200_000.0,
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
        fn set_key(&mut self, c: &mut OnboardingController, has: bool) {
            self.settings = Arc::new(Settings { has_api_key: has, ..(*self.settings).clone() });
            let s = self.settings.clone();
            c.on_update(&mut self.cx(), &UiUpdate::Settings(s));
        }
    }

    fn voice(id: &str, name: &str, labels: &[(&str, &str)], preview: bool) -> Voice {
        Voice {
            voice_id: id.into(),
            name: name.into(),
            labels: labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            category: Some("premade".into()),
            description: None,
            preview_url: preview.then(|| format!("https://example.com/{id}.mp3")),
        }
    }

    fn entered() -> (World, OnboardingController) {
        let mut w = World::new();
        let mut c = OnboardingController::default();
        c.on_enter(&mut w.cx(), &AppRoute::page(AppPage::Onboarding), &NavIntent::None);
        (w, c)
    }

    #[test]
    fn frame_labels_per_step() {
        let (mut w, mut c) = entered();
        let d = c.build(&mut w.cx());
        assert_eq!((d.step, d.furthest, d.crumb.as_str()), (0, 0, ""));
        assert_eq!((d.next_label.as_str(), d.next_secondary, d.show_back, d.show_skip), ("Get started", false, false, true));
        assert_eq!(d.step_labels.row_count(), 5);
        c.next(&mut w.cx());
        let d = c.build(&mut w.cx());
        assert_eq!((d.step, d.crumb.as_str()), (1, "Setup 2/5"));
        assert_eq!((d.next_label.as_str(), d.next_secondary, d.show_back), ("Skip for now", true, true));
        w.set_key(&mut c, true);
        assert_eq!(c.build(&mut w.cx()).next_label, "Continue");
        c.next(&mut w.cx());
        c.next(&mut w.cx());
        c.next(&mut w.cx());
        let d = c.build(&mut w.cx());
        assert_eq!((d.step, d.furthest, d.crumb.as_str()), (4, 4, "Setup 5/5"));
        assert_eq!((d.next_label.as_str(), d.show_back, d.show_skip), ("Open Vox", false, false));
        // Dots: only visited steps, never the current one.
        c.go_to(&mut w.cx(), 1);
        assert_eq!(c.build(&mut w.cx()).step, 1);
        c.back(&mut w.cx());
        assert_eq!(c.build(&mut w.cx()).step, 0);
        c.back(&mut w.cx());
        assert_eq!(c.build(&mut w.cx()).step, 0, "clamped");
        let (mut w, mut c) = entered();
        c.go_to(&mut w.cx(), 3);
        assert_eq!(c.build(&mut w.cx()).step, 0, "not visited yet");
        c.go_to(&mut w.cx(), -1);
        assert_eq!(c.build(&mut w.cx()).step, 0);
    }

    #[test]
    fn key_step_connect_flow() {
        with_locale(Locale::EN_US, || {
            let (mut w, mut c) = entered();
            c.next(&mut w.cx());
            let k = c.build(&mut w.cx()).key;
            assert!(!k.has_key && k.form_visible && !k.busy);
            assert_eq!(k.hint, KEY_HINT);
            c.connect_key(&mut w.cx(), "   ");
            assert_eq!(c.build(&mut w.cx()).key.error, KEY_EMPTY);
            assert!(w.sent().is_empty());
            c.key_edited(&mut w.cx());
            c.connect_key(&mut w.cx(), "\u{FEFF} sk_live \n");
            let req = w.sent().into_iter().find_map(|c| match c { Command::SetApiKey { req, key } if key == "sk_live" => Some(req), _ => None }).unwrap();
            assert!(c.build(&mut w.cx()).key.busy);
            c.cancel_replace(&mut w.cx());
            // Rejected: the core's message.
            let bad = ApiKeyResult { ok: false, error: Some("ElevenLabs didn\u{2019}t accept this key".into()), warning: None, account: None };
            c.on_update(&mut w.cx(), &UiUpdate::ApiKey { req, result: bad });
            assert_eq!(c.build(&mut w.cx()).key.error, "ElevenLabs didn\u{2019}t accept this key");
            // Accepted (the core publishes Settings before ApiKey).
            c.connect_key(&mut w.cx(), "sk_live");
            let req = w.sent().into_iter().find_map(|c| match c { Command::SetApiKey { req, .. } => Some(req), _ => None }).unwrap();
            w.set_key(&mut c, true);
            let account = AccountInfo { tier: "creator".into(), character_count: 61_234.0, character_limit: 100_000.0, next_reset_unix: None, status: None };
            let ok = ApiKeyResult { ok: true, error: None, warning: None, account: Some(account) };
            c.on_update(&mut w.cx(), &UiUpdate::ApiKey { req, result: ok });
            let k = c.build(&mut w.cx()).key;
            assert!(k.has_key && !k.form_visible && !k.busy);
            assert_eq!(k.connected_caption, "Creator plan \u{00B7} 38.8K characters left (39%)");
            assert_eq!(w.fields.onboarding_key_draft.as_deref(), Some(""));
            assert!(w.toasts.pushes.is_empty(), "no success toast");
            // Replace → form; cancel → back.
            c.replace_key(&mut w.cx());
            let k = c.build(&mut w.cx()).key;
            assert!(k.form_visible && k.replacing);
            c.cancel_replace(&mut w.cx());
            assert!(!c.build(&mut w.cx()).key.form_visible);
        });
    }

    #[test]
    fn restricted_key_warning_is_forgotten_after_leaving_the_step() {
        let (mut w, mut c) = entered();
        c.next(&mut w.cx());
        c.connect_key(&mut w.cx(), "sk_x");
        let req = w.sent().into_iter().find_map(|c| match c { Command::SetApiKey { req, .. } => Some(req), _ => None }).unwrap();
        w.set_key(&mut c, true);
        let warning = "This key can\u{2019}t read your account usage.";
        let ok = ApiKeyResult { ok: true, error: None, warning: Some(warning.into()), account: None };
        c.on_update(&mut w.cx(), &UiUpdate::ApiKey { req, result: ok });
        let k = c.build(&mut w.cx()).key;
        assert_eq!((k.connected_caption.as_str(), k.warning.as_str()), (READY_TO_READ, warning));
        c.next(&mut w.cx());
        c.back(&mut w.cx());
        let k = c.build(&mut w.cx()).key;
        assert_eq!((k.connected_caption.as_str(), k.warning.as_str()), (READY_TO_READ, ""));
    }

    #[test]
    fn connected_caption_rules() {
        with_locale(Locale::EN_US, || {
            let a = |tier: &str, count: f64, limit: f64| AccountInfo { tier: tier.into(), character_count: count, character_limit: limit, next_reset_unix: None, status: None };
            assert_eq!(connected_caption(Some(&a("starter_plus", 0.0, 30_000.0))), "Starter_plus plan \u{00B7} 30K characters left (100%)");
            assert_eq!(connected_caption(Some(&a("", 10.0, 0.0))), "0 characters left (0%)");
            assert_eq!(connected_caption(None), READY_TO_READ);
        });
    }

    #[test]
    fn voice_step_states_and_rows() {
        let (mut w, mut c) = entered();
        c.next(&mut w.cx());
        c.next(&mut w.cx());
        // No key, no demo: nothing fetched, the default voice reads.
        assert!(!w.sent().iter().any(|c| matches!(c, Command::FetchVoices { .. })));
        let v = c.build(&mut w.cx()).voices;
        assert_eq!((v.state, v.title.as_str(), v.body.as_str()), (LoadState::Empty, "Connect a key to browse voices", "George will read for you until then."));
        // The key appears while on the step: fetched.
        w.set_key(&mut c, true);
        assert!(w.sent().iter().any(|c| matches!(c, Command::FetchVoices { refresh: false })));
        assert_eq!(c.build(&mut w.cx()).voices.state, LoadState::Loading);
        c.on_update(&mut w.cx(), &UiUpdate::Voices { refresh: false, result: Err("TypeError: Can\u{2019}t reach ElevenLabs.".into()) });
        let v = c.build(&mut w.cx()).voices;
        assert_eq!((v.state, v.title.as_str(), v.body.as_str()), (LoadState::Error, "Couldn\u{2019}t load voices", "Can\u{2019}t reach ElevenLabs."));
        c.retry_voices(&mut w.cx());
        assert!(w.sent().iter().any(|c| matches!(c, Command::FetchVoices { refresh: true })));
        c.on_update(&mut w.cx(), &UiUpdate::Voices { refresh: true, result: Ok(Arc::new(vec![])) });
        assert_eq!(c.build(&mut w.cx()).voices.title, "No voices on this account");
        let list = vec![
            voice("zz", "Zed", &[("accent", "british"), ("age", "middle aged"), ("gender", "male"), ("use case", "narration")], true),
            voice("JBFqnCBsd6RMkjVDRZzb", "George", &[("accent", "british"), ("use_case", "")], false),
            voice("aa", "Aria", &[], true),
        ];
        c.on_update(&mut w.cx(), &UiUpdate::Voices { refresh: false, result: Ok(Arc::new(list)) });
        c.on_update(&mut w.cx(), &UiUpdate::Preview(PreviewUpdate::Loading("aa".into())));
        let v = c.build(&mut w.cx()).voices;
        assert_eq!(v.state, LoadState::Ready);
        let rows: Vec<VoiceRow> = v.items.iter().collect();
        assert_eq!(rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), vec!["Zed", "George", "Aria"], "API order");
        assert_eq!(rows[0].subtitle, "british \u{00B7} male \u{00B7} narration");
        assert_eq!(rows[1].subtitle, "british", "an empty use_case does not fall back to `use case`");
        assert_eq!(rows[2].subtitle, "premade");
        assert_eq!((rows[0].preview, rows[1].preview, rows[2].preview), (PreviewState::Ready, PreviewState::None, PreviewState::Loading));
        assert!(rows[1].in_use && !rows[0].in_use);
        // Choosing.
        c.choose_voice(&mut w.cx(), "JBFqnCBsd6RMkjVDRZzb");
        assert!(w.sent().is_empty(), "already current");
        c.choose_voice(&mut w.cx(), "zz");
        let (req, patch) = w.sent().into_iter().find_map(|c| match c { Command::UpdateSettings { req: Some(r), patch } => Some((r, patch)), _ => None }).unwrap();
        assert_eq!(patch, json!({ "voiceId": "zz", "voiceName": "Zed" }));
        c.on_update(&mut w.cx(), &UiUpdate::SettingsSaved { req, result: Err("Error: disk full".into()) });
        assert_eq!(w.toasts.pushes.last().unwrap().0.message, "Couldn\u{2019}t select Zed: disk full");
        // A failed sample toasts and leaving the step stops the sample.
        c.on_update(&mut w.cx(), &UiUpdate::Preview(PreviewUpdate::Failed { voice_id: "aa".into(), message: "Couldn\u{2019}t play this sample.".into() }));
        assert_eq!(w.toasts.pushes.last().unwrap().0.id, "voice-preview");
        c.preview_voice(&mut w.cx(), "zz");
        assert!(w.sent().iter().any(|c| matches!(c, Command::PreviewVoice { voice_id } if voice_id == "zz")));
        c.on_update(&mut w.cx(), &UiUpdate::Preview(PreviewUpdate::Playing("zz".into())));
        c.next(&mut w.cx());
        assert!(w.sent().iter().any(|c| matches!(c, Command::StopPreview)));
    }

    #[test]
    fn demo_counts_as_can_read() {
        let (mut w, mut c) = entered();
        w.demo = true;
        c.next(&mut w.cx());
        c.next(&mut w.cx());
        assert!(w.sent().iter().any(|c| matches!(c, Command::FetchVoices { .. })));
        assert_eq!(c.build(&mut w.cx()).voices.state, LoadState::Loading);
        assert!(c.build(&mut w.cx()).try_it.can_read);
    }

    #[test]
    fn try_step_reads_the_sample_and_follows_only_its_read() {
        let (mut w, mut c) = entered();
        let t = c.build(&mut w.cx()).try_it;
        assert!(!t.can_read);
        assert_eq!(t.keys.iter().collect::<Vec<_>>(), vec!["Ctrl", "Alt", "R"]);
        assert!(t.sample.starts_with("Hi, I\u{2019}m George. Select any text"));
        c.read_sample(&mut w.cx());
        assert!(w.sent().is_empty(), "no key: nothing to read");
        w.set_key(&mut c, true);
        c.read_sample(&mut w.cx());
        let (req, options) = w.sent().into_iter().find_map(|c| match c { Command::ReadText { req: Some(r), options, .. } => Some((r, options)), _ => None }).unwrap();
        assert_eq!((options.source, options.source_app), (Some(ReadSource::Manual), None));
        assert!(c.build(&mut w.cx()).try_it.busy);
        c.on_update(&mut w.cx(), &UiUpdate::ReadStarted { req, result: ReadResult::started("r1") });
        assert_eq!(c.build(&mut w.cx()).try_it.caption, CAPTION_IDLE);
        w.player.state.read_id = Some("r1".into());
        w.player.state.status = PlayerStatus::Playing;
        assert!({ let u = UiUpdate::Player(w.player.clone()); c.on_update(&mut w.cx(), &u) });
        assert_eq!(c.build(&mut w.cx()).try_it.caption, CAPTION_PLAYING);
        w.player.state.read_id = Some("other".into());
        assert!({ let u = UiUpdate::Player(w.player.clone()); c.on_update(&mut w.cx(), &u) });
        assert_eq!(c.build(&mut w.cx()).try_it.caption, CAPTION_IDLE);
        // A transport failure toasts `read-failed`.
        c.read_sample(&mut w.cx());
        let req = w.sent().into_iter().find_map(|c| match c { Command::ReadText { req: Some(r), .. } => Some(r), _ => None }).unwrap();
        c.on_update(&mut w.cx(), &UiUpdate::ReadStarted { req, result: ReadResult::internal_error() });
        let (t, _) = w.toasts.pushes.last().unwrap();
        assert_eq!((t.id.as_str(), t.message.as_str()), ("read-failed", "Something went wrong"));
    }

    #[test]
    fn done_tips_follow_the_shortcuts() {
        let (mut w, c) = entered();
        let tips: Vec<KeyGroup> = c.build(&mut w.cx()).tips.iter().collect();
        assert_eq!(tips.iter().map(|t| t.label.as_str()).collect::<Vec<_>>(), vec!["Click the tray icon to open Vox", "Read selection", "Play / pause"]);
        assert_eq!(tips[2].keys.iter().collect::<Vec<_>>(), vec!["Ctrl", "Alt", "P"]);
        w.settings = Arc::new(w.settings.apply_patch(&json!({ "triggers": { "hotkeysEnabled": false } })));
        assert_eq!(c.build(&mut w.cx()).tips.row_count(), 1);
        w.settings = Arc::new(w.settings.apply_patch(&json!({ "triggers": { "hotkeysEnabled": true, "hotkeys": { "readSelection": "" } } })));
        assert_eq!(c.build(&mut w.cx()).tips.row_count(), 2);
    }

    #[test]
    fn finish_waits_for_its_save() {
        let (mut w, mut c) = entered();
        c.skip(&mut w.cx());
        let (req, patch) = w.sent().into_iter().find_map(|c| match c { Command::UpdateSettings { req: Some(r), patch } => Some((r, patch)), _ => None }).unwrap();
        assert_eq!(patch, json!({ "onboarded": true }));
        assert!(c.build(&mut w.cx()).finishing);
        c.skip(&mut w.cx());
        assert!(w.sent().is_empty(), "ignored while finishing");
        c.on_update(&mut w.cx(), &UiUpdate::SettingsSaved { req, result: Err("Error: disk full".into()) });
        assert!(!c.build(&mut w.cx()).finishing);
        assert_eq!(w.toasts.pushes.last().unwrap().0.message, "Couldn\u{2019}t save your setup: disk full");
        assert!(w.nav.target.is_none());
        c.skip(&mut w.cx());
        let req = w.sent().into_iter().find_map(|c| match c { Command::UpdateSettings { req: Some(r), .. } => Some(r), _ => None }).unwrap();
        c.on_update(&mut w.cx(), &UiUpdate::SettingsSaved { req, result: Ok(()) });
        assert_eq!(w.nav.target.as_ref().map(|(r, _)| r.page), Some(AppPage::Home));
    }

    #[test]
    fn settings_account_route_jumps_to_the_key_step() {
        let (mut w, mut c) = entered();
        c.on_enter(&mut w.cx(), &AppRoute::settings("account"), &NavIntent::None);
        let d = c.build(&mut w.cx());
        assert_eq!((d.step, d.furthest), (1, 1));
        // An `onboarding/voice` route opens on that step.
        let mut c = OnboardingController::default();
        c.on_enter(&mut w.cx(), &AppRoute::validated(AppPage::Onboarding, Some("voice")), &NavIntent::None);
        assert_eq!(c.build(&mut w.cx()).step, 2);
    }

    #[test]
    fn get_key_failure_toasts_and_key_notice_shows() {
        let (mut w, mut c) = entered();
        c.get_key(&mut w.cx());
        let req = w.sent().into_iter().find_map(|c| match c { Command::OpenExternal { req: Some(r), url } if url == ELEVENLABS_API_KEYS_URL => Some(r), _ => None }).unwrap();
        c.on_update(&mut w.cx(), &UiUpdate::Done { req, result: Err("no browser".into()) });
        let (t, _) = w.toasts.pushes.last().unwrap();
        assert_eq!((t.id.as_str(), t.message.as_str()), ("open-external", "Couldn\u{2019}t open the link: no browser"));
        w.app_info = Arc::new(AppInfo { api_key_notice: Some(vox_core::copy::key_migration_failed()), ..AppInfo::default() });
        assert_eq!(c.build(&mut w.cx()).key.notice, vox_core::copy::key_migration_failed());
    }
}
