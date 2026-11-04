//! [`AddonHost`]: the one speech provider behind `Provider::Addon`. It picks the add-on from the voice id
//! (`addon:<add-on id>:<voice key>`, the engine passes the part after `addon:`) and hands the chunk to that add-on's
//! helper. Listing voices and the summary never start a helper while the voices are known.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use vox_core::addons::{addon_voice_id, addon_voice_name, parse_addon_rest, ManifestVoice};
use vox_core::providers::Provider;
use vox_core::read::ReadErrorCode;
use vox_core::voices::Voice;
use vox_tts::providers::ADDON_NOT_INSTALLED;
use vox_tts::{CancelToken, SpeechProvider, SynthesisResult, TtsError};

use crate::helper::{BrokenReason, HelperHealth};
use crate::runtime::AddonRuntime;

/// What the host knows about one add-on id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeStatus {
    NotInstalled,
    Ready,
    Broken(BrokenReason),
    Incompatible,
}

type HealthHook = Arc<dyn Fn(&str) + Send + Sync>;

/// The add-on speech provider (docs/addons-design.md "How the engine finds an add-on's client").
pub struct AddonHost {
    runtimes: RwLock<BTreeMap<String, Arc<AddonRuntime>>>,
    /// Add-on names from the catalogue, for the copy about one that is not installed.
    names: RwLock<HashMap<String, String>>,
    /// Add-ons removed while Vox runs (id → name): their voices say "was removed".
    removed: RwLock<HashMap<String, String>>,
    on_health: HealthHook,
}

impl AddonHost {
    /// `on_health(id)` runs when an add-on turns Broken or Incompatible, on the thread whose request found out.
    pub fn new(on_health: impl Fn(&str) + Send + Sync + 'static) -> AddonHost {
        AddonHost {
            runtimes: RwLock::new(BTreeMap::new()),
            names: RwLock::new(HashMap::new()),
            removed: RwLock::new(HashMap::new()),
            on_health: Arc::new(on_health),
        }
    }

    /// Installs or swaps in an add-on; returns the one it replaced. Requests that already have the old one finish on
    /// it, and its helper quits when the last of them is done; new requests go to the new one.
    pub fn replace(&self, runtime: AddonRuntime) -> Option<Arc<AddonRuntime>> {
        let id = runtime.id().to_string();
        let hook = self.on_health.clone();
        let hook_id = id.clone();
        runtime.helper.set_on_health(Box::new(move || hook(&hook_id)));
        self.removed.write().remove(&id);
        self.runtimes.write().insert(id, Arc::new(runtime))
    }

    /// Takes an add-on out: its running and waiting requests fail with "was removed", and its helper is quit (up to
    /// `grace` to exit, then killed). Returns it, so the caller can delete its folder.
    pub fn remove(&self, id: &str, grace: Duration) -> Option<Arc<AddonRuntime>> {
        let runtime = self.runtimes.write().remove(id)?;
        self.removed.write().insert(id.to_string(), runtime.manifest.name.clone());
        runtime.helper.stop(grace);
        Some(runtime)
    }

    pub fn runtime(&self, id: &str) -> Option<Arc<AddonRuntime>> {
        self.runtimes.read().get(id).cloned()
    }

    pub fn runtimes(&self) -> Vec<Arc<AddonRuntime>> {
        self.runtimes.read().values().cloned().collect()
    }

    pub fn is_installed(&self, id: &str) -> bool {
        self.runtimes.read().contains_key(id)
    }

    pub fn status(&self, id: &str) -> RuntimeStatus {
        match self.runtime(id).map(|r| r.helper.health()) {
            None | Some(HelperHealth::Removed) => RuntimeStatus::NotInstalled,
            Some(HelperHealth::Ready) => RuntimeStatus::Ready,
            Some(HelperHealth::Broken(r)) => RuntimeStatus::Broken(r),
            Some(HelperHealth::Incompatible) => RuntimeStatus::Incompatible,
        }
    }

    /// Names of add-ons that may not be installed (from the catalogue).
    pub fn set_known_names(&self, names: HashMap<String, String>) {
        *self.names.write() = names;
    }

    /// Quits helpers unused for longer than their add-on's `idle_exit_sec` (never one in use or asked for).
    pub fn reap_idle(&self, grace: Duration) {
        for r in self.runtimes() {
            if r.manifest.idle_exit_sec > 0 {
                r.helper.quit_if_idle(Duration::from_secs(r.manifest.idle_exit_sec), grace);
            }
        }
    }

    /// Vox is exiting: every helper is quit at once, each with up to `grace` to exit before it is killed.
    pub fn shutdown(&self, grace: Duration) {
        let all = self.runtimes();
        std::thread::scope(|s| {
            for r in &all {
                s.spawn(move || r.helper.stop(grace));
            }
        });
    }

    fn name_of(&self, id: &str) -> String {
        if let Some(r) = self.runtime(id) {
            return r.manifest.name.clone();
        }
        let removed = self.removed.read().get(id).cloned();
        removed.or_else(|| self.names.read().get(id).cloned()).unwrap_or_else(|| id.to_string())
    }

    fn not_installed(&self, id: &str) -> TtsError {
        let name = self.name_of(id);
        let message = if self.removed.read().contains_key(id) {
            format!("{name} was removed.")
        } else {
            format!("{name} isn’t installed. Install it on the Voices page.")
        };
        TtsError::new(ReadErrorCode::Unknown, message)
    }
}

/// A Vox voice for one add-on voice.
pub fn voice_of(runtime: &AddonRuntime, v: &ManifestVoice) -> Option<Voice> {
    let m = &runtime.manifest;
    let mut labels = Vec::new();
    for (key, value) in [("gender", &v.gender), ("accent", &v.accent), ("language", &v.language), ("use_case", &v.use_case)] {
        if !value.is_empty() {
            labels.push((key.to_string(), value.clone()));
        }
    }
    let description = if v.description.is_empty() { m.description.clone() } else { v.description.clone() };
    Some(Voice {
        voice_id: addon_voice_id(&m.id, &v.key)?,
        name: addon_voice_name(&v.name, &m.short_name),
        labels,
        category: Some(m.name.clone()),
        description: Some(description).filter(|d| !d.is_empty()),
        preview_url: None,
    })
}

impl SpeechProvider for AddonHost {
    fn provider(&self) -> Provider {
        Provider::Addon
    }

    fn synthesize(&self, text: &str, rest: &str, word_timing: bool, cancel: &CancelToken) -> Result<SynthesisResult, TtsError> {
        let Some(voice) = parse_addon_rest(rest) else { return Err(TtsError::new(ReadErrorCode::Unknown, ADDON_NOT_INSTALLED)) };
        let Some(runtime) = self.runtime(voice.addon) else { return Err(self.not_installed(voice.addon)) };
        let m = &runtime.manifest;
        let key = runtime.resolve_voice(voice.voice);
        // A voice the add-on does not have fails without starting its helper.
        if let Some(known) = runtime.known_voices() {
            if !known.iter().any(|v| v.key == key) {
                return Err(TtsError::new(ReadErrorCode::Unknown, format!("This voice isn’t in {} {}.", m.name, m.version)));
            }
        }
        runtime.helper.synthesize(key, text, word_timing, cancel).map_err(|e| e.into_tts(&m.name, m.version.as_str()))
    }

    fn list_voices(&self) -> Result<Vec<Voice>, TtsError> {
        let mut out = Vec::new();
        for r in self.runtimes() {
            match r.voices() {
                Ok(voices) => out.extend(voices.iter().filter_map(|v| voice_of(&r, v))),
                Err(e) => tracing::warn!(target: "addons", "{}: no voice list: {e:?}", r.id()),
            }
        }
        Ok(out)
    }

    fn summary(&self) -> String {
        let all = self.runtimes();
        let voices: usize = all.iter().filter_map(|r| r.known_voices()).map(|v| v.len()).sum();
        let addons = if all.len() == 1 { "1 add-on".to_string() } else { format!("{} add-ons", all.len()) };
        if voices == 1 {
            format!("{addons} · 1 voice")
        } else {
            format!("{addons} · {voices} voices")
        }
    }
}
