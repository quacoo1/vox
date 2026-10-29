//! Catalog, key and preview work (contracts.md §5.3; tts.md §2.7–§2.11; ui-settings-widget.md §4.6). Every network call
//! runs on a short-lived worker thread and reports `Command::Catalog(..)`.
//!
//! - Voices / models / account: the client's memo caches; demo mode **without** a key serves
//!   `vox_store::demo::{demo_voices, demo_models, demo_account}`. Before a fetch the core re-reads the Windows proxy
//!   when the last check is ≥ 30 s old (D-NET-1).
//! - **Key generation** (ui-settings-widget.md §1.2 keyed fetch): the core keeps `key_generation: u64`, bumped on every
//!   key change (set, replace, clear), stamps it on each worker job (`CatalogResult::{Voices, Models, Account}
//!   .generation`) and drops results from an older generation, so the old key's list can never land after the new
//!   key's. Answers: `UiUpdate::Voices { refresh, result }` / `Models { result }` (`refresh` lets the
//!   controllers tell a failed refresh — toast `Couldn’t refresh voices`, keep the list — from a failed first load).
//! - `SetApiKey`: the worker runs `client.validate_api_key(key)` (≤ 15 s, never retried) and reports
//!   `CatalogResult::ApiKey { req, key, result }`; the **core** then (only the core thread mutates settings) calls
//!   `settings.set_api_key(Some(key))` when `result.ok` (→ settings change with `key_changed` → forced credits
//!   refresh + voices/models refetch, `AppInfo.api_key_notice` cleared). Ordering: the core publishes
//!   `UiUpdate::Settings` and `UiUpdate::AppInfo` **before** `UiUpdate::ApiKey{req, result}` (so the Settings form
//!   closes on a state that already has `hasApiKey = true`). `ClearApiKey` runs on the core directly
//!   (`set_api_key(None)` writes the `secrets-dpapi.bin` tombstone), then `Settings`, `AppInfo`, then `Done{req}`.
//! - Previews: `client.fetch_preview(url)` → one-chunk session on the preview player at `settings.volume` (a later
//!   `volume` change applies to the sample in progress, reaction #12); starting a sample pauses a reading (`Pause` if
//!   the reader is playing/loading); a reader transition **into** `playing` stops the sample; only one sample at a
//!   time; a superseded download is cancelled. A download or preview-player failure → `PreviewUpdate::Failed` (the UI
//!   toasts `voice-preview`; never a core toast).

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use vox_core::constants::MAX_API_KEY_LEN;
use vox_core::copy::SOMETHING_WENT_WRONG;
use vox_core::ids::new_uuid;
use vox_core::js::js_trim;
use vox_core::player::{PlayerCommand, PlayerSnapshot, PlayerStatus};
use vox_core::read::eleven_messages;
use vox_core::text16::utf16_len;
use vox_core::tts::{AudioBytes, TtsChunk, TtsSessionDone, TtsSessionStart};
use vox_core::voices::{ApiKeyResult, Voice};
use vox_core::providers::{provider_of, split_voice_id, Provider, ProviderCredentials};
use vox_tts::CancelToken;

use super::reader::Reader;
use super::{spawn_worker, system_proxy_config, Core};
use crate::command::{CatalogResult, Command, RequestId};
use crate::update::{PreviewUpdate, UiUpdate, PREVIEW_FAILED, PREVIEW_FAILED_CONNECTION};

/// The `previewUrl` of a voice id, if known.
pub fn preview_url<'a>(voices: &'a [Voice], voice_id: &str) -> Option<&'a str> {
    voices.iter().find(|v| v.voice_id == voice_id).and_then(|v| v.preview_url.as_deref())
}

/// The voice sample in progress (one at a time).
#[derive(Debug, Clone)]
pub struct PreviewState {
    pub voice_id: String,
    /// The preview player's session once the sample downloaded (`None` while loading).
    pub session_id: Option<String>,
    /// `PreviewUpdate::Playing` was published.
    pub playing: bool,
    /// Cancels the download (a superseded or stopped sample never reports).
    pub(super) cancel: CancelToken,
}

/// A failed key validation answer.
fn key_failure(error: impl Into<String>) -> ApiKeyResult {
    ApiKeyResult { ok: false, error: Some(error.into()), warning: None, account: None }
}

/// The MIME of a downloaded sample (ElevenLabs serves MP3; a RIFF header means WAV).
fn sample_mime(audio: &[u8]) -> &'static str {
    if audio.starts_with(b"RIFF") {
        "audio/wav"
    } else {
        "audio/mpeg"
    }
}

/// A first duration estimate for a sample (128 kbps MP3); the player replaces it with the decoded duration.
fn sample_duration_estimate(bytes: usize) -> f64 {
    (bytes as f64 * 8.0 / 128_000.0).max(0.1)
}

/// The one-chunk session that plays a downloaded sample on the preview player.
pub fn preview_session(session_id: &str, voice_id: &str, audio: AudioBytes, volume: f64) -> (TtsSessionStart, TtsChunk, TtsSessionDone) {
    let duration = sample_duration_estimate(audio.len());
    let mime = sample_mime(&audio).to_string();
    let start = TtsSessionStart {
        session_id: session_id.to_string(),
        read_id: format!("preview-{voice_id}"),
        text: Arc::from(""),
        title: String::new(),
        voice_name: String::new(),
        voice_id: voice_id.to_string(),
        source_app: None,
        estimated_duration_sec: duration,
        chunk_count: 1,
        rate: 1.0,
        volume: if volume.is_finite() { volume.clamp(0.0, 1.0) } else { 1.0 },
        highlight_words: false,
        from_cache: true,
        sentences: Arc::from(vec![0u32]),
    };
    let chunk = TtsChunk {
        session_id: session_id.to_string(),
        index: 0,
        text_start: 0,
        text_end: 0,
        audio,
        mime,
        alignment: None,
        duration_sec: duration,
    };
    let done = TtsSessionDone { session_id: session_id.to_string(), total_chunks: 1 };
    (start, chunk, done)
}

/// What a preview-player report means for the sample in progress (pure; tested).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewStep {
    Nothing,
    /// The sample started playing: publish `Playing`.
    Playing,
    /// The sample ended or was stopped: publish `Stopped`.
    Stopped,
    /// The sample failed in the player: publish `Failed`.
    Failed,
}

/// Maps a preview-player snapshot onto the sample in progress.
pub fn preview_step(state: &PreviewState, snapshot: &PlayerSnapshot) -> PreviewStep {
    let Some(sid) = state.session_id.as_deref() else { return PreviewStep::Nothing };
    if snapshot.state.session_id.as_deref() != Some(sid) {
        return PreviewStep::Nothing;
    }
    match snapshot.state.status {
        PlayerStatus::Playing if !state.playing => PreviewStep::Playing,
        PlayerStatus::Ended | PlayerStatus::Idle => PreviewStep::Stopped,
        PlayerStatus::Error => PreviewStep::Failed,
        _ => PreviewStep::Nothing,
    }
}

impl Core {
    /// `FetchVoices{refresh}` (contracts.md §5.3 `eleven:voices`).
    pub(super) fn fetch_voices(&mut self, refresh: bool) {
        let generation = self.key_generation;
        if self.demo() && !self.services.settings.has_api_key() {
            let voices = Arc::new(vox_store::demo::demo_voices());
            self.on_catalog(CatalogResult::Voices { generation, refresh, result: Ok(voices) });
            return;
        }
        self.maybe_recheck_proxy(false);
        let client = self.services.client.clone();
        let others = self.services.engine.providers();
        let has_key = self.services.settings.has_api_key();
        let core = self.handle.clone();
        let spawned = spawn_worker("vox-voices", move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                // ElevenLabs when there is a key (or no other provider to show), then every connected provider.
                let eleven = (has_key || others.is_empty()).then(|| client.list_voices(refresh).map_err(|e| e.message));
                let rest = others.iter().map(|p| (p.provider(), p.list_voices().map_err(|e| e.message))).collect();
                merge_voice_lists(eleven, rest).map(Arc::new)
            }))
            .unwrap_or_else(|_| Err(SOMETHING_WENT_WRONG.to_string()));
            core.send(Command::Catalog(CatalogResult::Voices { generation, refresh, result }));
        });
        if !spawned {
            self.publish(UiUpdate::Voices { refresh, result: Err(SOMETHING_WENT_WRONG.to_string()) });
        }
    }

    /// `FetchModels{refresh}` (contracts.md §5.3 `eleven:models`).
    pub(super) fn fetch_models(&mut self, refresh: bool) {
        let generation = self.key_generation;
        if self.demo() && !self.services.settings.has_api_key() {
            let models = Arc::new(vox_store::demo::demo_models());
            self.on_catalog(CatalogResult::Models { generation, result: Ok(models) });
            return;
        }
        self.maybe_recheck_proxy(false);
        let client = self.services.client.clone();
        let core = self.handle.clone();
        let spawned = spawn_worker("vox-models", move || {
            let result = catch_unwind(AssertUnwindSafe(|| client.list_models(refresh).map(Arc::new).map_err(|e| e.message)))
                .unwrap_or_else(|_| Err(SOMETHING_WENT_WRONG.to_string()));
            core.send(Command::Catalog(CatalogResult::Models { generation, result }));
        });
        if !spawned {
            self.publish(UiUpdate::Models { result: Err(SOMETHING_WENT_WRONG.to_string()) });
        }
    }

    /// `SetApiKey` (ARCHITECTURE.md §4.11 step 2): the length guard at once, else validation on a worker.
    pub(super) fn set_api_key(&mut self, req: RequestId, key: String) {
        if utf16_len(js_trim(&key)) > MAX_API_KEY_LEN {
            self.publish(UiUpdate::ApiKey { req, result: key_failure(eleven_messages::KEY_TOO_LONG) });
            return;
        }
        self.maybe_recheck_proxy(false);
        let client = self.services.client.clone();
        let core = self.handle.clone();
        let spawned = spawn_worker("vox-key-check", move || {
            let result = catch_unwind(AssertUnwindSafe(|| client.validate_api_key(&key))).unwrap_or_else(|_| key_failure(SOMETHING_WENT_WRONG));
            core.send(Command::Catalog(CatalogResult::ApiKey { req, key, result }));
        });
        if !spawned {
            self.publish(UiUpdate::ApiKey { req, result: key_failure(SOMETHING_WENT_WRONG) });
        }
    }

    /// `ClearApiKey`: tombstone, then `Settings`, `AppInfo`, `Done{req}` (in that order). Once the key is cleared the
    /// answer is `Ok`: the reactions and the `AppInfo` run guarded, so a panic there cannot turn a done removal into a
    /// failure answer (D-ARCH-3).
    pub(super) fn clear_api_key(&mut self, req: RequestId) {
        let prev = self.settings();
        let next = self.services.settings.set_api_key(None);
        tracing::info!(target: "settings", "API key removed");
        self.publish(UiUpdate::Settings(Arc::new(next.clone())));
        let key_changed = next.has_api_key != prev.has_api_key;
        self.react(&next, &prev, false);
        if !key_changed {
            self.guarded("app info", |core| core.publish_app_info());
        }
        self.publish(UiUpdate::Done { req, result: Ok(()) });
    }

    /// `PreviewVoice{voice_id}` (ARCHITECTURE.md §4.10): toggles the sample of `voice_id`.
    pub(super) fn preview_voice(&mut self, voice_id: String) {
        if self.preview.as_ref().is_some_and(|p| p.voice_id == voice_id) {
            self.stop_preview();
            return;
        }
        // Only one sample at a time: the previous one ends without its own `Stopped` (the `Loading` below replaces it).
        if let Some(previous) = self.preview.take() {
            self.end_sample(&previous);
        }
        let reading = self.reader.last_snapshot.state.status;
        if matches!(reading, PlayerStatus::Playing | PlayerStatus::Loading) {
            self.command(PlayerCommand::Pause);
        }
        if provider_of(&voice_id) != Provider::ElevenLabs {
            self.preview_provider_voice(voice_id);
            return;
        }
        let known_url = self.voices.as_ref().and_then(|v| preview_url(v, &voice_id)).map(str::to_string);
        let cancel = CancelToken::new();
        self.preview = Some(PreviewState { voice_id: voice_id.clone(), session_id: None, playing: false, cancel: cancel.clone() });
        self.publish(UiUpdate::Preview(PreviewUpdate::Loading(voice_id.clone())));
        let client = self.services.client.clone();
        let core = self.handle.clone();
        let id = voice_id.clone();
        let spawned = spawn_worker("vox-preview", move || {
            let fetched = catch_unwind(AssertUnwindSafe(|| {
                // The list is memoised by the client, so a lookup here is normally free.
                let url = known_url.or_else(|| {
                    client.list_voices(false).ok().and_then(|voices| preview_url(&voices, &id).map(str::to_string))
                });
                match url {
                    Some(url) => client.fetch_preview(&url, &cancel).map_err(|e| e.message),
                    None => Err("This voice has no sample".to_string()),
                }
            }))
            .unwrap_or_else(|_| Err(SOMETHING_WENT_WRONG.to_string()));
            if cancel.is_cancelled() {
                return; // superseded or stopped: nobody waits for it
            }
            core.send(Command::Catalog(CatalogResult::Preview { voice_id: id, result: fetched }));
        });
        if !spawned {
            self.preview = None;
            self.publish(UiUpdate::Preview(PreviewUpdate::Failed { voice_id, message: PREVIEW_FAILED_CONNECTION.to_string() }));
        }
    }

    /// The sample of a non-ElevenLabs voice: a short sentence synthesized on the spot (they have no sample files).
    fn preview_provider_voice(&mut self, voice_id: String) {
        let cancel = CancelToken::new();
        self.preview = Some(PreviewState { voice_id: voice_id.clone(), session_id: None, playing: false, cancel: cancel.clone() });
        self.publish(UiUpdate::Preview(PreviewUpdate::Loading(voice_id.clone())));
        let client = self.services.engine.provider(provider_of(&voice_id));
        let name = self.voices.as_ref().and_then(|v| v.iter().find(|x| x.voice_id == voice_id)).map(|v| v.name.clone());
        let core = self.handle.clone();
        let id = voice_id.clone();
        let spawned = spawn_worker("vox-preview", move || {
            let fetched = catch_unwind(AssertUnwindSafe(|| {
                let (Some(client), Some((provider, rest))) = (client, split_voice_id(&id)) else {
                    return Err(vox_tts::not_connected(provider_of(&id)).message);
                };
                let who = name.map(|n| n.split(" (").next().unwrap_or(&n).to_string()).unwrap_or_else(|| provider.display_name().to_string());
                let sample = format!("Hi, I’m {who}. This is how I sound when Vox reads to you.");
                client.synthesize(&sample, rest, false, &cancel).map(|r| r.audio).map_err(|e| e.message)
            }))
            .unwrap_or_else(|_| Err(SOMETHING_WENT_WRONG.to_string()));
            if cancel.is_cancelled() {
                return;
            }
            core.send(Command::Catalog(CatalogResult::Preview { voice_id: id, result: fetched }));
        });
        if !spawned {
            self.preview = None;
            self.publish(UiUpdate::Preview(PreviewUpdate::Failed { voice_id, message: PREVIEW_FAILED_CONNECTION.to_string() }));
        }
    }

    /// `ConnectProvider`: checks the fields at once, then the credentials on a worker.
    pub(super) fn connect_provider(&mut self, req: RequestId, provider: Provider, creds: ProviderCredentials) {
        if !provider.needs_credentials() || provider == Provider::ElevenLabs {
            self.publish(UiUpdate::Done { req, result: Ok(()) });
            return;
        }
        let creds = match creds.validated(provider) {
            Ok(c) => c,
            Err(message) => {
                self.publish(UiUpdate::Done { req, result: Err(message) });
                return;
            }
        };
        self.maybe_recheck_proxy(false);
        let core = self.handle.clone();
        let spawned = spawn_worker("vox-provider-check", move || {
            let client = vox_tts::connect_provider(provider, creds.clone(), system_proxy_config().as_ref(), &vox_tts::user_agent());
            let result = catch_unwind(AssertUnwindSafe(|| match client {
                Some(c) => c.validate().map_err(|e| e.message),
                None => Err(SOMETHING_WENT_WRONG.to_string()),
            }))
            .unwrap_or_else(|_| Err(SOMETHING_WENT_WRONG.to_string()));
            core.send(Command::Catalog(CatalogResult::Provider { req, provider, creds, result }));
        });
        if !spawned {
            self.publish(UiUpdate::Done { req, result: Err(SOMETHING_WENT_WRONG.to_string()) });
        }
    }

    fn on_provider_validated(&mut self, req: RequestId, provider: Provider, creds: ProviderCredentials, result: Result<usize, String>) {
        let name = provider.display_name();
        if let Err(message) = result {
            tracing::info!(target: "settings", "{name} credentials rejected");
            self.publish(UiUpdate::Done { req, result: Err(message) });
            return;
        }
        if let Err(e) = vox_store::provider_secrets::save(&self.services.runtime.profile, provider, &creds) {
            tracing::warn!(target: "settings", "could not store the {name} credentials: {e}");
            self.publish(UiUpdate::Done { req, result: Err(format!("Couldn’t save the {name} credentials.")) });
            return;
        }
        tracing::info!(target: "settings", "{name} connected");
        let client = vox_tts::connect_provider(provider, creds, system_proxy_config().as_ref(), &vox_tts::user_agent());
        self.services.engine.set_provider(provider, client);
        self.guarded("app info", |core| core.publish_app_info());
        self.fetch_voices(true);
        self.publish(UiUpdate::Done { req, result: Ok(()) });
    }

    /// `DisconnectProvider`: forgets the credentials; that provider's voices stop working until reconnected.
    pub(super) fn disconnect_provider(&mut self, req: RequestId, provider: Provider) {
        if !provider.needs_credentials() {
            self.publish(UiUpdate::Done { req, result: Ok(()) });
            return;
        }
        if let Err(e) = vox_store::provider_secrets::clear(&self.services.runtime.profile, provider) {
            tracing::warn!(target: "settings", "could not remove the {} credentials: {e}", provider.display_name());
        }
        self.services.engine.set_provider(provider, None);
        tracing::info!(target: "settings", "{} disconnected", provider.display_name());
        self.guarded("app info", |core| core.publish_app_info());
        self.fetch_voices(true);
        self.publish(UiUpdate::Done { req, result: Ok(()) });
    }

    /// `StopPreview`, leaving the page, a reading that starts playing: ends the sample (publishes `Stopped` only when
    /// one was in progress).
    pub(super) fn stop_preview(&mut self) {
        if let Some(previous) = self.preview.take() {
            self.end_sample(&previous);
            self.publish(UiUpdate::Preview(PreviewUpdate::Stopped));
        }
    }

    /// Cancels the download and stops the preview player's session.
    fn end_sample(&self, sample: &PreviewState) {
        sample.cancel.cancel();
        if sample.session_id.is_some() {
            self.services.preview_player.command(PlayerCommand::Stop);
        }
    }

    /// A preview-player report.
    pub(super) fn on_preview_report(&mut self, snapshot: &PlayerSnapshot) {
        let Some(state) = self.preview.as_mut() else { return };
        match preview_step(state, snapshot) {
            PreviewStep::Nothing => {}
            PreviewStep::Playing => {
                state.playing = true;
                let id = state.voice_id.clone();
                self.publish(UiUpdate::Preview(PreviewUpdate::Playing(id)));
            }
            PreviewStep::Stopped => {
                self.preview = None;
                self.publish(UiUpdate::Preview(PreviewUpdate::Stopped));
            }
            PreviewStep::Failed => {
                let voice_id = state.voice_id.clone();
                self.preview = None;
                tracing::warn!(target: "preview", "voice sample could not be played");
                self.publish(UiUpdate::Preview(PreviewUpdate::Failed { voice_id, message: PREVIEW_FAILED.to_string() }));
            }
        }
    }

    /// The preview player failed and restarted itself.
    pub(super) fn on_preview_failed(&mut self, message: &str) {
        tracing::warn!(target: "preview", "preview player failed: {message}");
        if let Some(sample) = self.preview.take() {
            sample.cancel.cancel();
            self.publish(UiUpdate::Preview(PreviewUpdate::Failed { voice_id: sample.voice_id, message: PREVIEW_FAILED.to_string() }));
        }
    }

    /// A worker's catalog result.
    pub(super) fn on_catalog(&mut self, result: CatalogResult) {
        match result {
            CatalogResult::Voices { generation, refresh, result } => {
                if generation != self.key_generation {
                    tracing::debug!(target: "catalog", "dropped a voices list of a previous key");
                    return;
                }
                if let Err(e) = &result {
                    tracing::warn!(target: "catalog", "voices failed: {e}");
                }
                if let Ok(list) = &result {
                    self.voices = Some(list.clone());
                }
                self.publish(UiUpdate::Voices { refresh, result });
            }
            CatalogResult::Models { generation, result } => {
                if generation != self.key_generation {
                    tracing::debug!(target: "catalog", "dropped a models list of a previous key");
                    return;
                }
                if let Err(e) = &result {
                    tracing::warn!(target: "catalog", "models failed: {e}");
                }
                self.publish(UiUpdate::Models { result });
            }
            CatalogResult::Account { generation, forced, seq, result } => {
                if generation != self.key_generation {
                    tracing::debug!(target: "credits", "account result of a previous key");
                }
                tracing::debug!(target: "credits", "account refresh settled (forced={forced})");
                if let Err(e) = &result {
                    tracing::warn!(target: "credits", "credits refresh failed: {e}");
                }
                self.settle_credits(seq, result);
            }
            CatalogResult::ApiKey { req, key, result } => self.on_key_validated(req, &key, result),
            CatalogResult::Preview { voice_id, result } => self.on_preview_downloaded(voice_id, result),
            CatalogResult::Provider { req, provider, creds, result } => self.on_provider_validated(req, provider, creds, result),
        }
    }

    /// ARCHITECTURE.md §4.11 step 3. Owes `ApiKey{req}` (`owed_answer` covers this `Catalog(ApiKey)` hop); once the
    /// key is saved the reactions and the `AppInfo` run guarded, so the answer is the validation's.
    fn on_key_validated(&mut self, req: RequestId, key: &str, result: ApiKeyResult) {
        if !result.ok {
            self.publish(UiUpdate::ApiKey { req, result });
            return;
        }
        let prev = self.settings();
        let replaced = prev.has_api_key && self.services.settings.api_key().as_deref() != Some(js_trim(key));
        let next = self.services.settings.set_api_key(Some(key));
        if !next.has_api_key {
            // The store refused an (unexpectedly) blank key.
            self.publish(UiUpdate::ApiKey { req, result: key_failure(eleven_messages::PASTE_KEY_FIRST) });
            return;
        }
        tracing::info!(target: "settings", "API key {}", if replaced { "replaced" } else { "saved" });
        if let Some(account) = result.account.clone() {
            // The validated account is the first `Ready` value (the forced refresh below keeps showing it).
            self.credits.seed(account);
        }
        self.publish(UiUpdate::Settings(Arc::new(next.clone())));
        let key_changed = next.has_api_key != prev.has_api_key || replaced;
        self.react(&next, &prev, replaced);
        if !key_changed {
            self.guarded("app info", |core| core.publish_app_info());
        }
        self.publish(UiUpdate::ApiKey { req, result });
    }

    /// The sample downloaded (or failed): play it on the preview player if it is still wanted.
    fn on_preview_downloaded(&mut self, voice_id: String, result: Result<AudioBytes, String>) {
        let wanted = self.preview.as_ref().is_some_and(|p| p.voice_id == voice_id && p.session_id.is_none());
        if !wanted {
            return;
        }
        match result {
            Err(e) => {
                tracing::warn!(target: "preview", "voice sample download failed: {e}");
                self.preview = None;
                self.publish(UiUpdate::Preview(PreviewUpdate::Failed { voice_id, message: PREVIEW_FAILED_CONNECTION.to_string() }));
            }
            Ok(audio) => {
                let session_id = new_uuid();
                let (start, chunk, done) = preview_session(&session_id, &voice_id, audio, self.settings().volume);
                let player = &self.services.preview_player;
                player.start_session(start);
                player.add_chunk(chunk);
                player.finish_session(done);
                if let Some(state) = self.preview.as_mut() {
                    state.session_id = Some(session_id);
                }
            }
        }
    }
}

/// Combines the ElevenLabs list (`None` = not asked) with the other providers' lists: ElevenLabs voices first, then
/// each provider in order. A failing provider is skipped (and logged) as long as something else answered; when
/// nothing succeeded, the first error is reported.
pub fn merge_voice_lists(eleven: Option<Result<Vec<Voice>, String>>, others: Vec<(Provider, Result<Vec<Voice>, String>)>) -> Result<Vec<Voice>, String> {
    let mut out = Vec::new();
    let mut first_err = None;
    let mut any_ok = false;
    if let Some(r) = eleven {
        match r {
            Ok(v) => {
                any_ok = true;
                out.extend(v);
            }
            Err(e) => {
                tracing::warn!(target: "tts", "ElevenLabs voices unavailable: {e}");
                first_err = Some(e);
            }
        }
    }
    for (provider, r) in others {
        match r {
            Ok(v) => {
                any_ok = true;
                out.extend(v);
            }
            Err(e) => {
                tracing::warn!(target: "tts", "{} voices unavailable: {e}", provider.display_name());
                first_err.get_or_insert(e);
            }
        }
    }
    match (any_ok, first_err) {
        (false, Some(e)) => Err(e),
        _ => Ok(out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn voice(id: &str, url: Option<&str>) -> Voice {
        Voice {
            voice_id: id.into(),
            name: id.into(),
            labels: Vec::new(),
            category: None,
            description: None,
            preview_url: url.map(str::to_string),
        }
    }

    #[test]
    fn preview_urls_by_voice_id() {
        let voices = [voice("a", Some("https://x/a.mp3")), voice("b", None)];
        assert_eq!(preview_url(&voices, "a"), Some("https://x/a.mp3"));
        assert_eq!(preview_url(&voices, "b"), None);
        assert_eq!(preview_url(&voices, "zzz"), None);
    }

    #[test]
    fn the_sample_session_is_one_cached_chunk_at_the_settings_volume() {
        let audio: AudioBytes = Arc::from(vec![0u8; 32_000]);
        let (start, chunk, done) = preview_session("s1", "v1", audio, 0.4);
        assert_eq!((start.session_id.as_str(), start.chunk_count, start.rate, start.volume), ("s1", 1, 1.0, 0.4));
        assert!(start.from_cache);
        assert_eq!(&*start.sentences, &[0u32]);
        assert_eq!(chunk.mime, "audio/mpeg");
        assert_eq!((chunk.index, chunk.session_id.as_str()), (0, "s1"));
        assert!((chunk.duration_sec - 2.0).abs() < 1e-9, "32 000 bytes at 128 kbps");
        assert_eq!(done, TtsSessionDone { session_id: "s1".into(), total_chunks: 1 });
        let (start, chunk, _) = preview_session("s2", "v", Arc::from(b"RIFF....".to_vec()), f64::NAN);
        assert_eq!(start.volume, 1.0);
        assert_eq!(chunk.mime, "audio/wav");
    }

    fn snap(session: Option<&str>, status: PlayerStatus) -> PlayerSnapshot {
        let mut s = PlayerSnapshot::default();
        s.state.session_id = session.map(str::to_string);
        s.state.status = status;
        s
    }

    #[test]
    fn preview_reports_map_onto_the_sample() {
        let loading = PreviewState { voice_id: "v".into(), session_id: None, playing: false, cancel: CancelToken::new() };
        assert_eq!(preview_step(&loading, &snap(Some("s"), PlayerStatus::Playing)), PreviewStep::Nothing, "still downloading");
        let mut started = PreviewState { session_id: Some("s".into()), ..loading };
        assert_eq!(preview_step(&started, &snap(Some("s"), PlayerStatus::Loading)), PreviewStep::Nothing);
        assert_eq!(preview_step(&started, &snap(Some("s"), PlayerStatus::Playing)), PreviewStep::Playing);
        started.playing = true;
        assert_eq!(preview_step(&started, &snap(Some("s"), PlayerStatus::Playing)), PreviewStep::Nothing, "published once");
        assert_eq!(preview_step(&started, &snap(Some("s"), PlayerStatus::Ended)), PreviewStep::Stopped);
        assert_eq!(preview_step(&started, &snap(Some("s"), PlayerStatus::Idle)), PreviewStep::Stopped);
        assert_eq!(preview_step(&started, &snap(Some("s"), PlayerStatus::Error)), PreviewStep::Failed);
        assert_eq!(preview_step(&started, &snap(Some("old"), PlayerStatus::Error)), PreviewStep::Nothing, "another session");
        assert_eq!(preview_step(&started, &snap(None, PlayerStatus::Idle)), PreviewStep::Nothing, "sessionless report");
    }
}
