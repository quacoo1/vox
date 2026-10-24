//! Voices controller (ui-settings-widget.md §4): `FetchVoices` once settings are loaded (refetch when `hasApiKey`
//! changes; refresh button forces; `UiUpdate::Voices{refresh}` tells a failed refresh — toast `Couldn’t refresh
//! voices`, keep the list — from a failed first load), filters (`vox_core::voices::{filter_voices, voice_facets,
//! sort_voices_for_page}`), content states, rows (art by voice id, `voice_subtitle`, tags, caption, in-use), preview
//! states from `UiUpdate::Preview` (`PreviewUpdate::Failed` → UI-local toast id `voice-preview`), "Use voice"
//! (`UpdateSettings{req, {voiceId, voiceName}}`; on `SettingsSaved{req, Ok}` the success toast
//! `copy::reading_with(name)`, id `voice-use`). The search field's text is the main window's in-out
//! `voices-search-text`: "Clear filters" and leaving the page write `""` through `FieldWrites`. Filter selects are
//! re-asserted from `data` on every rebuild (`VoicesData.revision`, ARCHITECTURE.md §7.1).
//!
//! Details fixed here:
//! - The list of the last successful fetch is kept while the page is not shown (re-entering does not flash
//!   skeletons; it still fetches with `refresh = false` on entry, which the core's memo cache answers). Filters, the
//!   search text and the sample reset on every entry and leave (the page is re-created in the spec).
//! - `refreshing` is true while this page's fetch is in flight (any `UiUpdate::Voices` answer ends it: the core
//!   drops answers of an older key generation and always answers the current one).
//! - "Use voice" is optimistic: the row shows `In use` from the click; `SettingsSaved{req, Ok}` toasts
//!   `Reading with {name} from now on` (id `voice-use`), `Err` reverts and toasts `Couldn’t save that setting` (id
//!   `settings-save-error`, message = the cleaned error).
//! - The row subtitle puts the voice kind first for the user's own voices (`Cloned · Male · American · Podcast`,
//!   `voices.slint` header); premade voices start with the gender. Tags (3, caption keys skipped) and the caption are
//!   filled too (the onboarding voice step reuses [`voice_row_view`]).
//! - The filter's "is filtered" test is `VoiceFilter::is_active` (a whitespace-only query is not a filter).
//! - Rows are kept in one `VecModel` updated in place (`history::sync_model`), so Slint keeps row instances (focus,
//!   hover) across rebuilds.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use slint::{Image, ModelRc, VecModel};
use vox_core::app::{AppRoute, Toast};
use vox_core::copy::reading_with;
use vox_core::format::{clean_error, count_label, format_number};
use vox_core::js::js_trim;
use vox_core::voices::{
    filter_voices, sort_voices_for_page, title_case, voice_caption, voice_facets, voice_kind, voice_subtitle, voice_tags,
    Voice, VoiceFilter, CAPTION_LABEL_KEYS,
};

use super::art::ArtSize;
use super::generated::{ChoiceItem, PreviewState, VoiceRow, VoicesData, VoicesState};
use super::history::sync_model;
use super::{NavIntent, PageController, UiCtx};
use crate::command::{Command, RequestId};
use crate::update::{PreviewUpdate, UiUpdate, PREVIEW_FAILED};

/// The sample-preview failure toast id.
pub const PREVIEW_TOAST_ID: &str = "voice-preview";
/// "Use voice" success toast id.
pub const USE_TOAST_ID: &str = "voice-use";
/// A failed refresh while a list is shown.
pub const REFRESH_TOAST_ID: &str = "voices-refresh";
/// A settings write the store refused.
pub const SAVE_ERROR_TOAST_ID: &str = "settings-save-error";
/// Tags on a card (`voiceTags(v, 3, CAPTION_LABEL_KEYS)`).
const CARD_TAGS: usize = 3;

// ───────────────────────────── sample preview state (shared with onboarding) ─────────────────────────────

/// Which voice's sample is loading or playing (one at a time; the core owns the preview player).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum Sample {
    #[default]
    None,
    Loading(String),
    Playing(String),
}

impl Sample {
    /// The voice whose sample is loading or playing.
    pub(crate) fn voice_id(&self) -> Option<&str> {
        match self {
            Sample::None => None,
            Sample::Loading(id) | Sample::Playing(id) => Some(id.as_str()),
        }
    }

    /// The preview button state of `v` (no button without a sample URL).
    pub(crate) fn state_for(&self, v: &Voice) -> PreviewState {
        if !has_sample(v) {
            return PreviewState::None;
        }
        match self {
            Sample::Loading(id) if *id == v.voice_id => PreviewState::Loading,
            Sample::Playing(id) if *id == v.voice_id => PreviewState::Playing,
            _ => PreviewState::Ready,
        }
    }

    /// The optimistic state right after a click on `voice_id`'s preview button: the core stops the sample of the same
    /// voice and starts another voice's (`Command::PreviewVoice` toggles).
    pub(crate) fn toggle(&mut self, voice_id: &str) {
        *self = if self.voice_id() == Some(voice_id) { Sample::None } else { Sample::Loading(voice_id.to_string()) };
    }

    /// Applies a core report; returns the `voice-preview` failure toast for `Failed`.
    pub(crate) fn apply(&mut self, update: &PreviewUpdate) -> Option<Toast> {
        match update {
            PreviewUpdate::Loading(id) => *self = Sample::Loading(id.clone()),
            PreviewUpdate::Playing(id) => *self = Sample::Playing(id.clone()),
            PreviewUpdate::Stopped => *self = Sample::None,
            PreviewUpdate::Failed { message, .. } => {
                *self = Sample::None;
                let message = if js_trim(message).is_empty() { PREVIEW_FAILED.to_string() } else { message.clone() };
                return Some(Toast::error(message).with_id(PREVIEW_TOAST_ID));
            }
        }
        None
    }
}

/// The voice has a sample URL (some clones have none).
fn has_sample(v: &Voice) -> bool {
    v.preview_url.as_deref().is_some_and(|u| !js_trim(u).is_empty())
}

// ───────────────────────────── row view (pure) ─────────────────────────────

/// One voice row as plain data (compared to update the Slint model in place).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct VoiceRowView {
    pub id: String,
    pub name: String,
    pub subtitle: String,
    pub description: String,
    pub tags: Vec<String>,
    pub caption: String,
    pub in_use: bool,
    pub preview: PreviewState,
}

/// `label('use_case') || label('use case')`.
fn use_case(v: &Voice) -> &str {
    let a = v.label_trimmed("use_case");
    if a.is_empty() {
        v.label_trimmed("use case")
    } else {
        a
    }
}

/// The row subtitle: the kind for the user's own voices, then gender, accent and use case (title case); without any
/// of them `voice_subtitle`'s fallback (category, else `Voice`).
pub(crate) fn row_subtitle(v: &Voice) -> String {
    let mut parts: Vec<String> = Vec::new();
    let kind = voice_kind(v);
    if !kind.is_empty() {
        parts.push(kind);
    }
    for p in [v.label_trimmed("gender"), v.label_trimmed("accent"), use_case(v)] {
        if !p.is_empty() {
            parts.push(title_case(p));
        }
    }
    if parts.is_empty() {
        voice_subtitle(v)
    } else {
        parts.join(" \u{00B7} ")
    }
}

/// A voice row (also used by the onboarding voice step).
pub(crate) fn voice_row_view(v: &Voice, in_use: bool, preview: PreviewState) -> VoiceRowView {
    let description = v.description.as_deref().map(js_trim).unwrap_or("");
    VoiceRowView {
        id: v.voice_id.clone(),
        name: v.name.clone(),
        subtitle: row_subtitle(v),
        description: if description.is_empty() { "No description.".to_string() } else { description.to_string() },
        tags: voice_tags(v, CARD_TAGS, &CAPTION_LABEL_KEYS),
        caption: voice_caption(v),
        in_use,
        preview,
    }
}

/// The Slint row (art = voice art seeded by the voice id).
pub(crate) fn voice_row(v: &VoiceRowView, art: &Image) -> VoiceRow {
    VoiceRow {
        id: v.id.as_str().into(),
        name: v.name.as_str().into(),
        art: art.clone(),
        subtitle: v.subtitle.as_str().into(),
        description: v.description.as_str().into(),
        tags: super::convert::strings(v.tags.iter().cloned()),
        caption: v.caption.as_str().into(),
        in_use: v.in_use,
        preview: v.preview,
    }
}

// ───────────────────────────── page view (pure) ─────────────────────────────

/// `(value, label, accessible label)` of a filter option.
type Choice = (String, String, String);

/// Everything `VoicesData` shows except the art.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PageView {
    pub state: VoicesState,
    pub in_use: String,
    pub count: String,
    pub gender_options: Vec<Choice>,
    pub accent_options: Vec<Choice>,
    pub category_options: Vec<Choice>,
    pub state_title: String,
    pub state_body: String,
    pub state_button: String,
    pub rows: Vec<VoiceRowView>,
}

/// The inputs of [`page_view`].
pub(crate) struct PageInputs<'a> {
    /// The last successful list (`None` = nothing loaded yet).
    pub voices: Option<&'a [Voice]>,
    /// The cleaned error of the last failed fetch.
    pub error: Option<&'a str>,
    /// This page's fetch is in flight.
    pub fetching: bool,
    pub has_api_key: bool,
    pub filter: &'a VoiceFilter,
    /// The voice in use (the pending "Use voice" choice while it saves) and its stored name.
    pub current_id: &'a str,
    pub current_name: &'a str,
    pub sample: &'a Sample,
}

/// `{All…}` + one option per facet (value = the facet lower-cased, label = title case).
fn options(all_label: &str, all_accessible: &str, facets: &[String]) -> Vec<Choice> {
    let mut out = vec![(String::new(), all_label.to_string(), all_accessible.to_string())];
    out.extend(facets.iter().map(|f| {
        let label = title_case(f);
        (js_trim(f).to_lowercase(), label.clone(), label)
    }));
    out
}

pub(crate) fn page_view(i: &PageInputs) -> PageView {
    let list = i.voices.unwrap_or(&[]);
    let in_use_name =
        list.iter().find(|v| v.voice_id == i.current_id).map(|v| v.name.as_str()).unwrap_or(i.current_name);
    let mut view = PageView {
        state: VoicesState::Loading,
        in_use: if in_use_name.is_empty() { String::new() } else { format!("In use: {in_use_name}") },
        count: String::new(),
        gender_options: Vec::new(),
        accent_options: Vec::new(),
        category_options: Vec::new(),
        state_title: String::new(),
        state_body: String::new(),
        state_button: String::new(),
        rows: Vec::new(),
    };
    if list.is_empty() {
        let (state, title, body, button) = if i.fetching || (i.voices.is_none() && i.error.is_none()) {
            (VoicesState::Loading, "", String::new(), "")
        } else if let Some(error) = i.error {
            if i.has_api_key {
                (VoicesState::Error, "Couldn\u{2019}t load your voices", error.to_string(), "Try again")
            } else {
                (
                    VoicesState::NoKey,
                    "Add your ElevenLabs key to browse voices",
                    "Voices come straight from your ElevenLabs account \u{2014} the premade library plus any voices \
                     you\u{2019}ve cloned or designed."
                        .to_string(),
                    "Add API key",
                )
            }
        } else {
            (VoicesState::Empty, "No voices yet", "Add or clone a voice in ElevenLabs, then refresh this page.".to_string(), "")
        };
        view.state = state;
        view.state_title = title.to_string();
        view.state_body = body;
        view.state_button = button.to_string();
        return view;
    }

    let facets = voice_facets(list);
    if !facets.genders.is_empty() {
        view.gender_options = options("All", "All genders", &facets.genders);
    }
    if !facets.accents.is_empty() {
        view.accent_options = options("All accents", "All accents", &facets.accents);
    }
    if facets.categories.len() > 1 {
        view.category_options = options("All types", "All types", &facets.categories);
    }
    let shown = filter_voices(&sort_voices_for_page(list), i.filter);
    view.count = if i.filter.is_active() {
        format!("{} of {}", format_number(shown.len() as f64), format_number(list.len() as f64))
    } else {
        count_label(list.len() as u64, "voice", "voices")
    };
    if shown.is_empty() {
        view.state = VoicesState::NoMatch;
        view.state_title = "No voices match".to_string();
        view.state_body = "Try a different search or clear the filters.".to_string();
        view.state_button = "Clear filters".to_string();
        return view;
    }
    view.state = VoicesState::List;
    view.rows = shown.iter().map(|v| voice_row_view(v, v.voice_id == i.current_id, i.sample.state_for(v))).collect();
    view
}

fn choices(items: &[Choice]) -> ModelRc<ChoiceItem> {
    let v: Vec<ChoiceItem> = items
        .iter()
        .map(|(value, label, accessible)| ChoiceItem {
            value: value.into(),
            label: label.into(),
            accessible_label: accessible.into(),
        })
        .collect();
    ModelRc::new(VecModel::from(v))
}

// ───────────────────────────── controller ─────────────────────────────

/// Voices page state.
#[derive(Default)]
pub struct VoicesController {
    filter: VoiceFilter,
    /// Voices is the page on screen.
    active: bool,
    /// The last successful list (kept through refreshes and failures).
    voices: Option<Arc<Vec<Voice>>>,
    /// The cleaned error of the last failed fetch (cleared by the next success).
    error: Option<String>,
    /// This page's `FetchVoices` is in flight.
    fetching: bool,
    /// A forced refresh is in flight (its failure toasts while a list is shown).
    refresh_requested: bool,
    /// `settings.hasApiKey` the list was fetched for.
    has_key: Option<bool>,
    sample: Sample,
    /// The pending "Use voice": `(request, voice id, name)`.
    use_req: Option<(RequestId, String, String)>,
    revision: i32,
    rows_model: Rc<VecModel<VoiceRow>>,
    rows_prev: RefCell<Vec<(VoiceRowView, Image)>>,
}

impl VoicesController {
    pub fn search_edited(&mut self, cx: &mut UiCtx, text: &str) {
        let _ = cx;
        self.filter.query = text.to_string();
    }
    pub fn set_gender(&mut self, cx: &mut UiCtx, value: &str) {
        let _ = cx;
        self.filter.gender = value.to_string();
    }
    pub fn set_accent(&mut self, cx: &mut UiCtx, value: &str) {
        let _ = cx;
        self.filter.accent = value.to_string();
    }
    pub fn set_category(&mut self, cx: &mut UiCtx, value: &str) {
        let _ = cx;
        self.filter.category = value.to_string();
    }
    /// "Clear filters": all four filters, the search field included.
    pub fn clear_filters(&mut self, cx: &mut UiCtx) {
        self.reset_filters(cx);
    }
    /// The Refresh button and the error state's "Try again" (ignored while a fetch is in flight).
    pub fn refresh(&mut self, cx: &mut UiCtx) {
        if self.fetching {
            return;
        }
        self.fetch(cx, true);
    }
    /// A row's preview button: toggles that voice's sample (the core pauses a reading first).
    pub fn preview(&mut self, cx: &mut UiCtx, voice_id: &str) {
        let Some(voice) = self.find(voice_id) else { return };
        if !has_sample(voice) {
            return;
        }
        self.sample.toggle(voice_id);
        cx.core.send(Command::PreviewVoice { voice_id: voice_id.to_string() });
    }
    /// "Use voice": `UpdateSettings{req, {voiceId, voiceName}}` (optimistic; module docs).
    pub fn use_voice(&mut self, cx: &mut UiCtx, voice_id: &str) {
        let Some(voice) = self.find(voice_id) else { return };
        if self.current_id(&cx.settings.voice_id) == voice_id {
            return;
        }
        let name = voice.name.clone();
        let req = cx.core.request_id();
        cx.core.send(Command::UpdateSettings {
            req: Some(req),
            patch: serde_json::json!({ "voiceId": voice_id, "voiceName": name }),
        });
        self.use_req = Some((req, voice_id.to_string(), name));
    }
    /// The no-key state's "Add API key" → Settings → Account.
    pub fn add_key(&mut self, cx: &mut UiCtx) {
        cx.nav.navigate(AppRoute::settings("account"));
    }

    // ── internals ──

    fn find(&self, voice_id: &str) -> Option<&Voice> {
        self.voices.as_deref().and_then(|list| list.iter().find(|v| v.voice_id == voice_id))
    }

    /// The voice in use: the pending "Use voice" choice while it saves, else the stored one.
    fn current_id<'a>(&'a self, stored: &'a str) -> &'a str {
        self.use_req.as_ref().map(|(_, id, _)| id.as_str()).unwrap_or(stored)
    }

    fn fetch(&mut self, cx: &mut UiCtx, refresh: bool) {
        self.fetching = true;
        if refresh {
            self.refresh_requested = true;
        }
        cx.core.send(Command::FetchVoices { refresh });
    }

    fn reset_filters(&mut self, cx: &mut UiCtx) {
        self.filter = VoiceFilter::default();
        cx.fields.voices_search_text = Some(String::new());
        self.revision = self.revision.wrapping_add(1);
    }

    fn stop_sample(&mut self, cx: &mut UiCtx) {
        if self.sample.voice_id().is_some() {
            cx.core.send(Command::StopPreview);
        }
        self.sample = Sample::None;
    }

    /// `UiUpdate::Voices`.
    fn on_voices(&mut self, refresh: bool, result: &Result<Arc<Vec<Voice>>, String>) -> Option<Toast> {
        self.fetching = false;
        let requested = std::mem::take(&mut self.refresh_requested);
        match result {
            Ok(list) => {
                self.voices = Some(Arc::clone(list));
                self.error = None;
                None
            }
            Err(e) => {
                let message = clean_error(e);
                tracing::warn!(target: "ui", "voices fetch failed (refresh: {refresh}): {message}");
                self.error = Some(message.clone());
                let shown = self.voices.as_ref().is_some_and(|l| !l.is_empty());
                (refresh && requested && shown)
                    .then(|| Toast::error(message).with_title("Couldn\u{2019}t refresh voices").with_id(REFRESH_TOAST_ID))
            }
        }
    }

    /// `UiUpdate::SettingsSaved` for a pending "Use voice".
    fn on_saved(&mut self, req: RequestId, result: &Result<(), String>) -> Option<Toast> {
        if self.use_req.as_ref().map(|(r, _, _)| *r) != Some(req) {
            return None;
        }
        let (_, _, name) = self.use_req.take()?;
        Some(match result {
            Ok(()) => Toast::success(reading_with(&name)).with_id(USE_TOAST_ID),
            Err(e) => Toast::error(clean_error(e)).with_title("Couldn\u{2019}t save that setting").with_id(SAVE_ERROR_TOAST_ID),
        })
    }

    fn view(&self, has_api_key: bool, stored_id: &str, stored_name: &str) -> PageView {
        let current_name = self.use_req.as_ref().map(|(_, _, n)| n.as_str()).unwrap_or(stored_name);
        page_view(&PageInputs {
            voices: self.voices.as_deref().map(|v| v.as_slice()),
            error: self.error.as_deref(),
            fetching: self.fetching,
            has_api_key,
            filter: &self.filter,
            current_id: self.current_id(stored_id),
            current_name,
            sample: &self.sample,
        })
    }
}

impl PageController for VoicesController {
    type Data = VoicesData;

    fn on_enter(&mut self, cx: &mut UiCtx, route: &AppRoute, intent: &NavIntent) {
        let _ = (route, intent);
        self.active = true;
        self.reset_filters(cx);
        self.sample = Sample::None;
        self.has_key = Some(cx.settings.has_api_key);
        self.fetch(cx, false);
    }

    fn on_leave(&mut self, cx: &mut UiCtx) {
        self.active = false;
        self.stop_sample(cx);
        self.reset_filters(cx);
        // A refresh still in flight must not toast after the page was left (its answer still updates the list).
        self.fetching = false;
        self.refresh_requested = false;
    }

    fn on_update(&mut self, cx: &mut UiCtx, update: &UiUpdate) -> bool {
        match update {
            UiUpdate::Voices { refresh, result } => {
                if let Some(toast) = self.on_voices(*refresh, result) {
                    cx.toasts.push(toast, None);
                }
                true
            }
            UiUpdate::Settings(settings) => {
                // Keyed by `hasApiKey`: a key added or removed refetches (the list stays until the answer).
                if self.has_key != Some(settings.has_api_key) {
                    self.has_key = Some(settings.has_api_key);
                    if self.active {
                        self.fetch(cx, false);
                    }
                }
                true
            }
            UiUpdate::SettingsSaved { req, result } => match self.on_saved(*req, result) {
                Some(toast) => {
                    cx.toasts.push(toast, None);
                    true
                }
                None => false,
            },
            UiUpdate::Preview(p) => {
                if let Some(toast) = self.sample.apply(p) {
                    if self.active {
                        cx.toasts.push(toast, None);
                    }
                }
                true
            }
            UiUpdate::Refresh(_) => true,
            _ => false,
        }
    }

    fn build(&self, cx: &mut UiCtx) -> VoicesData {
        let v = self.view(cx.settings.has_api_key, &cx.settings.voice_id, &cx.settings.voice_name);
        let next: Vec<(VoiceRowView, Image)> = v
            .rows
            .iter()
            .map(|row| {
                let art = cx.art.image(&row.id, ArtSize::Large);
                (row.clone(), art)
            })
            .collect();
        sync_model(&self.rows_model, &mut self.rows_prev.borrow_mut(), next, voice_row);
        VoicesData {
            state: v.state,
            in_use: v.in_use.into(),
            count: v.count.into(),
            gender: self.filter.gender.as_str().into(),
            accent: self.filter.accent.as_str().into(),
            category: self.filter.category.as_str().into(),
            gender_options: choices(&v.gender_options),
            accent_options: choices(&v.accent_options),
            category_options: choices(&v.category_options),
            refreshing: self.fetching,
            state_title: v.state_title.into(),
            state_body: v.state_body.into(),
            state_button: v.state_button.into(),
            voices: ModelRc::from(self.rows_model.clone()),
            revision: self.revision,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vox_core::locale::{with_locale, Locale};

    fn voice(id: &str, name: &str, cat: Option<&str>, labels: &[(&str, &str)], url: Option<&str>) -> Voice {
        Voice {
            voice_id: id.into(),
            name: name.into(),
            labels: labels.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            category: cat.map(Into::into),
            description: None,
            preview_url: url.map(Into::into),
        }
    }

    fn library() -> Vec<Voice> {
        let mut george = voice(
            "JBFq",
            "George",
            Some("premade"),
            &[("accent", "british"), ("age", "middle_aged"), ("gender", "male"), ("use_case", "narration")],
            Some("https://x/george.mp3"),
        );
        george.description = Some("  Warm, mature storyteller.  ".into());
        vec![
            george,
            voice("Xb7h", "Alice", Some("premade"), &[("accent", "british"), ("gender", "female"), ("use_case", "news")], Some("https://x/a.mp3")),
            voice("clone-02", "Interview voice", Some("cloned"), &[("accent", "american"), ("gender", "male"), ("use case", "podcast")], Some("https://x/c.mp3")),
            voice("clone-01", "My reading voice", Some("cloned"), &[], None),
            voice("gen-01", "Robot", Some("generated"), &[("gender", "female"), ("accent", "british")], Some(" ")),
        ]
    }

    fn inputs<'a>(voices: Option<&'a [Voice]>, filter: &'a VoiceFilter, sample: &'a Sample) -> PageInputs<'a> {
        PageInputs {
            voices,
            error: None,
            fetching: false,
            has_api_key: true,
            filter,
            current_id: "JBFq",
            current_name: "George",
            sample,
        }
    }

    #[test]
    fn subtitles_descriptions_tags_and_captions() {
        let lib = library();
        let rows: Vec<VoiceRowView> = lib.iter().map(|v| voice_row_view(v, false, PreviewState::None)).collect();
        assert_eq!(rows[0].subtitle, "Male \u{00B7} British \u{00B7} Narration");
        assert_eq!(rows[0].description, "Warm, mature storyteller.");
        assert_eq!(rows[0].tags, vec!["British", "Middle aged", "Male"]);
        assert_eq!(rows[0].caption, "Narration");
        assert_eq!(rows[2].subtitle, "Cloned \u{00B7} Male \u{00B7} American \u{00B7} Podcast");
        assert_eq!(rows[2].caption, "Cloned \u{00B7} Podcast");
        assert_eq!(rows[3].subtitle, "Cloned");
        assert_eq!(rows[3].description, "No description.");
        assert_eq!(rows[4].subtitle, "Designed \u{00B7} Female \u{00B7} British");
        let bare = voice("p", "Plain", Some("premade"), &[], None);
        assert_eq!(row_subtitle(&bare), "Premade");
        assert_eq!(row_subtitle(&voice("q", "Q", None, &[], None)), "Voice");
    }

    #[test]
    fn content_states_in_order() {
        with_locale(Locale::EN_US, || {
            let f = VoiceFilter::default();
            let s = Sample::None;
            // Nothing loaded yet → skeleton; a fetch in flight with an empty list → skeleton.
            assert_eq!(page_view(&inputs(None, &f, &s)).state, VoicesState::Loading);
            let mut i = inputs(Some(&[] as &[Voice]), &f, &s);
            i.fetching = true;
            assert_eq!(page_view(&i).state, VoicesState::Loading);
            // Failed with an empty list: no key / error.
            let mut i = inputs(None, &f, &s);
            i.error = Some("ElevenLabs didn\u{2019}t answer in time.");
            let v = page_view(&i);
            assert_eq!((v.state, v.state_title.as_str(), v.state_button.as_str()), (VoicesState::Error, "Couldn\u{2019}t load your voices", "Try again"));
            assert_eq!(v.state_body, "ElevenLabs didn\u{2019}t answer in time.");
            assert_eq!(v.in_use, "In use: George");
            i.has_api_key = false;
            let v = page_view(&i);
            assert_eq!((v.state, v.state_button.as_str()), (VoicesState::NoKey, "Add API key"));
            assert_eq!(v.state_title, "Add your ElevenLabs key to browse voices");
            assert!(v.state_body.starts_with("Voices come straight from your ElevenLabs account \u{2014} the premade"));
            // Loaded but empty.
            let v = page_view(&inputs(Some(&[] as &[Voice]), &f, &s));
            assert_eq!((v.state, v.state_title.as_str(), v.state_button.as_str()), (VoicesState::Empty, "No voices yet", ""));
            assert_eq!(v.count, "");
            // Filters that match nothing.
            let lib = library();
            let f = VoiceFilter { gender: "female".into(), accent: "irish".into(), ..Default::default() };
            let v = page_view(&inputs(Some(&lib[..]), &f, &s));
            assert_eq!((v.state, v.count.as_str(), v.state_button.as_str()), (VoicesState::NoMatch, "0 of 5", "Clear filters"));
            assert_eq!(v.state_body, "Try a different search or clear the filters.");
        });
    }

    #[test]
    fn list_order_filters_facets_and_counts() {
        with_locale(Locale::EN_US, || {
            let lib = library();
            let s = Sample::Playing("JBFq".into());
            let f = VoiceFilter::default();
            let v = page_view(&inputs(Some(&lib[..]), &f, &s));
            assert_eq!(v.state, VoicesState::List);
            assert_eq!(v.count, "5 voices");
            // Own voices first (cloned, generated), then premade, by name.
            let names: Vec<&str> = v.rows.iter().map(|r| r.name.as_str()).collect();
            assert_eq!(names, ["Interview voice", "My reading voice", "Robot", "Alice", "George"]);
            let george = v.rows.iter().find(|r| r.id == "JBFq").unwrap();
            assert!(george.in_use);
            assert_eq!(george.preview, PreviewState::Playing);
            assert_eq!(v.rows.iter().find(|r| r.id == "clone-01").unwrap().preview, PreviewState::None);
            assert_eq!(v.rows.iter().find(|r| r.id == "gen-01").unwrap().preview, PreviewState::None, "blank URL = no sample");
            assert_eq!(v.rows.iter().find(|r| r.id == "Xb7h").unwrap().preview, PreviewState::Ready);
            assert_eq!(
                v.gender_options,
                vec![
                    (String::new(), "All".to_string(), "All genders".to_string()),
                    ("female".to_string(), "Female".to_string(), "Female".to_string()),
                    ("male".to_string(), "Male".to_string(), "Male".to_string()),
                ]
            );
            assert_eq!(v.accent_options[0], (String::new(), "All accents".to_string(), "All accents".to_string()));
            assert_eq!(v.accent_options.len(), 3);
            assert_eq!(v.category_options[0].1, "All types");
            assert_eq!(v.category_options.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(), ["", "cloned", "generated", "premade"]);
            // Filtered.
            let f = VoiceFilter { accent: "british".into(), category: "premade".into(), ..Default::default() };
            let v = page_view(&inputs(Some(&lib[..]), &f, &s));
            assert_eq!(v.count, "2 of 5");
            assert_eq!(v.rows.len(), 2);
            let f = VoiceFilter { query: "podcast".into(), ..Default::default() };
            assert_eq!(page_view(&inputs(Some(&lib[..]), &f, &s)).rows[0].id, "clone-02");
            // One category only → no type picker; no genders → no gender pills.
            let one = [voice("a", "A", Some("cloned"), &[("accent", "american")], None)];
            let v = page_view(&inputs(Some(&one[..]), &VoiceFilter::default(), &s));
            assert!(v.category_options.is_empty() && v.gender_options.is_empty());
            assert_eq!(v.count, "1 voice");
            // In use falls back to the stored name when the voice is not in the list.
            assert_eq!(v.in_use, "In use: George");
        });
    }

    #[test]
    fn sample_state_machine() {
        let mut s = Sample::default();
        s.toggle("a");
        assert_eq!(s, Sample::Loading("a".into()));
        s.toggle("a");
        assert_eq!(s, Sample::None, "a second click on the same voice stops it");
        s.toggle("a");
        s.toggle("b");
        assert_eq!(s, Sample::Loading("b".into()), "another voice supersedes");
        assert_eq!(s.apply(&PreviewUpdate::Playing("b".into())), None);
        assert_eq!(s, Sample::Playing("b".into()));
        assert_eq!(s.apply(&PreviewUpdate::Stopped), None);
        assert_eq!(s, Sample::None);
        let t = s
            .apply(&PreviewUpdate::Failed { voice_id: "b".into(), message: crate::update::PREVIEW_FAILED_CONNECTION.into() })
            .expect("a failure toasts");
        assert_eq!(t.id, PREVIEW_TOAST_ID);
        assert_eq!(t.message, "Couldn\u{2019}t play this sample. Check your connection.");
        let t = s.apply(&PreviewUpdate::Failed { voice_id: "b".into(), message: String::new() }).unwrap();
        assert_eq!(t.message, PREVIEW_FAILED);
    }

    #[test]
    fn refresh_failure_toasts_only_with_a_list_and_a_requested_refresh() {
        let mut c = VoicesController { fetching: true, ..VoicesController::default() };
        assert!(c.on_voices(false, &Err("TypeError: offline".into())).is_none(), "a failed first load shows the error state");
        assert_eq!(c.error.as_deref(), Some("offline"));
        assert!(!c.fetching);
        c.fetching = true;
        assert!(c.on_voices(false, &Ok(Arc::new(library()))).is_none());
        assert!(c.error.is_none());
        c.fetching = true;
        c.refresh_requested = true;
        let t = c.on_voices(true, &Err("Error: timed out".into())).expect("toast");
        assert_eq!((t.id.as_str(), t.message.as_str()), (REFRESH_TOAST_ID, "timed out"));
        assert_eq!(t.title.as_deref(), Some("Couldn\u{2019}t refresh voices"));
        assert_eq!(c.voices.as_ref().map(|l| l.len()), Some(5), "the old list stays");
        // A refresh answer this page did not ask for stays quiet.
        assert!(c.on_voices(true, &Err("x".into())).is_none());
    }

    #[test]
    fn use_voice_is_optimistic_and_toasts_on_save() {
        with_locale(Locale::EN_US, || {
            let mut c = VoicesController { voices: Some(Arc::new(library())), ..VoicesController::default() };
            let req = RequestId(7);
            c.use_req = Some((req, "Xb7h".into(), "Alice".into()));
            let v = c.view(true, "JBFq", "George");
            assert_eq!(v.in_use, "In use: Alice");
            assert!(v.rows.iter().find(|r| r.id == "Xb7h").unwrap().in_use);
            assert!(!v.rows.iter().find(|r| r.id == "JBFq").unwrap().in_use);
            assert!(c.on_saved(RequestId(8), &Ok(())).is_none(), "another request");
            let t = c.on_saved(req, &Ok(())).unwrap();
            assert_eq!((t.id.as_str(), t.message.as_str()), (USE_TOAST_ID, "Reading with Alice from now on"));
            assert!(c.use_req.is_none());
            // A refused write reverts (the stored voice shows again) and toasts the settings error.
            c.use_req = Some((req, "Xb7h".into(), "Alice".into()));
            let t = c.on_saved(req, &Err("Error: disk full".into())).unwrap();
            assert_eq!((t.id.as_str(), t.message.as_str()), (SAVE_ERROR_TOAST_ID, "disk full"));
            assert_eq!(t.title.as_deref(), Some("Couldn\u{2019}t save that setting"));
            assert_eq!(c.view(true, "JBFq", "George").in_use, "In use: George");
        });
    }

    #[test]
    fn controller_entry_preview_use_and_leave() {
        use super::super::analytics::test_world::World;
        use vox_core::app::AppPage;
        let mut w = World::new();
        let mut c = VoicesController::default();
        c.set_gender(&mut w.cx(), "male");
        c.on_enter(&mut w.cx(), &AppRoute::page(AppPage::Voices), &NavIntent::None);
        assert!(matches!(w.sent().as_slice(), [Command::FetchVoices { refresh: false }]));
        assert_eq!(w.fields.voices_search_text.take(), Some(String::new()));
        assert_eq!(c.filter, VoiceFilter::default(), "a fresh page starts unfiltered");
        assert_eq!(c.view(true, "JBFq", "George").state, VoicesState::Loading);
        // Refresh is ignored while the first fetch is in flight.
        c.refresh(&mut w.cx());
        assert!(w.sent().is_empty());
        let list = UiUpdate::Voices { refresh: false, result: Ok(Arc::new(library())) };
        assert!(c.on_update(&mut w.cx(), &list));
        assert!(!c.fetching);
        // Preview: toggles optimistically; voices without a sample are ignored.
        c.preview(&mut w.cx(), "Xb7h");
        assert!(matches!(w.sent().as_slice(), [Command::PreviewVoice { voice_id }] if voice_id == "Xb7h"));
        assert_eq!(c.sample, Sample::Loading("Xb7h".into()));
        c.preview(&mut w.cx(), "clone-01");
        c.preview(&mut w.cx(), "nope");
        assert!(w.sent().is_empty());
        assert!(c.on_update(&mut w.cx(), &UiUpdate::Preview(PreviewUpdate::Playing("Xb7h".into()))));
        assert_eq!(c.view(true, "JBFq", "George").rows.iter().find(|r| r.id == "Xb7h").unwrap().preview, PreviewState::Playing);
        // A failure toasts on this page.
        let failed = UiUpdate::Preview(PreviewUpdate::Failed { voice_id: "Xb7h".into(), message: PREVIEW_FAILED.into() });
        assert!(c.on_update(&mut w.cx(), &failed));
        assert_eq!(w.toasts.pushes.pop().map(|(t, _)| t.id), Some(PREVIEW_TOAST_ID.to_string()));
        // Use voice: the patch carries id and name; the stored voice is not re-sent.
        c.use_voice(&mut w.cx(), "Xb7h");
        match w.sent().as_slice() {
            [Command::UpdateSettings { req: Some(_), patch }] => {
                assert_eq!(patch, &serde_json::json!({ "voiceId": "Xb7h", "voiceName": "Alice" }));
            }
            other => panic!("unexpected {other:?}"),
        }
        let req = c.use_req.as_ref().map(|(r, _, _)| *r).unwrap();
        // The core publishes the new settings, then the save result.
        w.settings = Arc::new(vox_core::settings::Settings {
            voice_id: "Xb7h".into(),
            voice_name: "Alice".into(),
            ..(*w.settings).clone()
        });
        let saved = UiUpdate::Settings(Arc::clone(&w.settings));
        assert!(c.on_update(&mut w.cx(), &saved));
        assert!(w.sent().is_empty(), "same key: no refetch");
        assert!(c.on_update(&mut w.cx(), &UiUpdate::SettingsSaved { req, result: Ok(()) }));
        assert_eq!(w.toasts.pushes.pop().map(|(t, _)| t.message), Some("Reading with Alice from now on".to_string()));
        assert_eq!(c.view(true, &w.settings.voice_id, &w.settings.voice_name).in_use, "In use: Alice");
        c.use_voice(&mut w.cx(), "Xb7h");
        assert!(w.sent().is_empty(), "Alice is already in use");
        // A key added or removed refetches.
        let no_key = Arc::new(vox_core::settings::Settings { has_api_key: false, ..(*w.settings).clone() });
        assert!(c.on_update(&mut w.cx(), &UiUpdate::Settings(no_key)));
        assert!(matches!(w.sent().as_slice(), [Command::FetchVoices { refresh: false }]));
        // Leaving stops the sample and clears the search.
        c.on_update(&mut w.cx(), &UiUpdate::Preview(PreviewUpdate::Loading("JBFq".into())));
        c.search_edited(&mut w.cx(), "brit");
        c.on_leave(&mut w.cx());
        assert!(matches!(w.sent().as_slice(), [Command::StopPreview]));
        assert_eq!(c.sample, Sample::None);
        assert_eq!(c.filter.query, "");
        assert_eq!(w.fields.voices_search_text.take(), Some(String::new()));
        // Refresh after re-entry.
        c.on_enter(&mut w.cx(), &AppRoute::page(AppPage::Voices), &NavIntent::None);
        let _ = w.sent();
        c.on_update(&mut w.cx(), &list);
        c.refresh(&mut w.cx());
        assert!(matches!(w.sent().as_slice(), [Command::FetchVoices { refresh: true }]));
        c.add_key(&mut w.cx());
        assert_eq!(w.nav.target, Some((AppRoute::settings("account"), NavIntent::None)));
    }
}
