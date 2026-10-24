//! The settings store (persistence.md §3, contracts.md §4.3–§4.5; ARCHITECTURE.md §6.2).
//!
//! - [`SettingsStore::open`]: load `settings.json` through [`crate::fsutil::load_with_backup`] (`.corrupt` + `.bak`
//!   rules); load the key with [`crate::secrets::load_api_key`] (Electron migration included), else the
//!   `ELEVENLABS_API_KEY` fallback (never persisted; a stored key that failed is then only logged, so
//!   [`KeyStatus::problem`] is `None`); `current = Settings::from_stored(raw, has_key)`.
//!   Rewrite rules: the main file was corrupt and `.bak` was good → **write the main file immediately** (BRIEF.md
//!   "Corrupt main file + good `.bak`: rewrite the main file right away and log it": `restored settings.json from
//!   backup; rewriting it`), via `JsonDoc::schedule` + `flush`; else a file was loaded and
//!   `current.needs_rewrite(raw)` → schedule a debounced rewrite (the migration mechanism). A missing file is not
//!   written until the first change.
//! - [`SettingsStore::update`]: `current.apply_patch(patch)`; unchanged → return current (no write, no event); else
//!   replace, schedule the debounced write (250 ms / 2 s) and call every listener with a [`SettingsChange`]
//!   **synchronously on the caller's thread** (a panicking listener is caught and logged; the others still run).
//!   Listeners are invoked **after the internal lock is released**, iterating a cloned listener list, so a listener
//!   may call any store method; a re-entrant `update()` nests (persistence.md §3.3 nested commit) and its listeners
//!   run before the outer call returns. Test: a listener that calls `get()` and `update()`.
//! - [`SettingsStore::set_api_key`]: JS-trims; `""`/`None` clears. No-op if equal to the current key. Persists with
//!   [`crate::secrets::save_api_key`] / [`crate::secrets::clear_api_key`] (`secrets-dpapi.bin`; `secrets.bin` is never
//!   touched); a plain I/O failure is logged and the key stays in memory for this session (parity). If `save_api_key`
//!   fails with `SecretError::Dpapi` (secure storage unavailable), it warns `secure storage is unavailable; the API key
//!   is kept in memory for this session only` and best-effort writes the 0-byte tombstone (`clear_api_key`), so the
//!   next start asks for the key instead of silently loading an older one (persistence.md §4.1). Clears
//!   [`KeyStatus::problem`]; sets `has_api_key`; **always** emits a change with `key_changed = true` (key replaced →
//!   every surface refetches account/voices). Does not write `settings.json`.
//! - Threading: the handle is `Clone + Send + Sync`; in the app **only the core thread mutates** (update,
//!   set_api_key), so listeners run on the core thread. Reads (`get`, `api_key`) may come from any thread (the TTS
//!   client's key source reads `api_key` on worker threads).
//! - The key is never logged and never leaves vox-store except through [`SettingsStore::api_key`].

use std::fs;
use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::Value;
use vox_core::js::js_trim;
use vox_core::settings::Settings;

use crate::fsutil::{load_with_backup, parse_json_bytes, with_suffix};
use crate::json_doc::{JsonDoc, JsonDocOptions};
use crate::paths::Profile;
use crate::secrets::{clear_api_key, dpapi_protect, load_api_key, save_api_key_with, KeyLoad, KeyProblem, KeySource, Protect, SecretError};

/// One effective change.
#[derive(Debug, Clone, Copy)]
pub struct SettingsChange<'a> {
    pub next: &'a Settings,
    pub prev: &'a Settings,
    /// The API key was set, replaced or cleared by this change (`has_api_key` may be unchanged).
    pub key_changed: bool,
}

/// Settings change listener.
pub type SettingsListener = Box<dyn Fn(&SettingsChange<'_>) + Send + Sync>;

/// Where the key in memory came from, and whether a stored key could not be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyStatus {
    pub source: KeyStatusSource,
    /// Set by `open` when a stored key could not be read/migrated **and no usable key is in memory** (with the
    /// `ELEVENLABS_API_KEY` fallback in use it is `None`: the failure is only logged, persistence.md §4.4); cleared by
    /// the next `set_api_key`. Drives `AppInfo::api_key_notice` and the D-KEY-3 startup route as is.
    pub problem: Option<KeyProblem>,
}

/// [`KeySource`] plus the environment fallback and keys set during this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyStatusSource {
    #[default]
    None,
    /// Loaded from disk (`secrets-dpapi.bin`, or migrated from `secrets.bin`).
    Stored(KeySource),
    /// `ELEVENLABS_API_KEY` (never persisted).
    Environment,
    /// Set (validated) during this session.
    Entered,
}

/// What the store mutex guards (the settings snapshot taken at write time comes from here).
struct State {
    current: Settings,
    /// The trimmed key. Never logged.
    api_key: Option<String>,
    key_status: KeyStatus,
}

struct Inner {
    profile: Profile,
    state: Arc<Mutex<State>>,
    listeners: Mutex<Vec<Arc<SettingsListener>>>,
    doc: JsonDoc,
    /// Serialises secret writes (and lets `flush` wait for one in progress on another thread).
    secrets_lock: Mutex<()>,
    protect: Protect,
}

/// Thread-safe settings store handle.
#[derive(Clone)]
pub struct SettingsStore {
    inner: Arc<Inner>,
}

impl SettingsStore {
    /// Loads settings and the API key (with Electron migration). Never fails: unreadable data falls back to defaults.
    /// `env_api_key` is `ELEVENLABS_API_KEY` (JS-trimmed; used only when no stored key loads; never persisted).
    pub fn open(profile: &Profile, env_api_key: Option<String>) -> SettingsStore {
        let key = load_api_key(profile);
        Self::open_with(profile, env_api_key, key, JsonDocOptions::settings(), dpapi_protect)
    }

    /// [`SettingsStore::open`] with an already loaded key, document options and a DPAPI encrypt (tests).
    pub(crate) fn open_with(
        profile: &Profile,
        env_api_key: Option<String>,
        key: KeyLoad,
        opts: JsonDocOptions,
        protect: Protect,
    ) -> SettingsStore {
        let path = profile.settings_file();
        let outcome = load_with_backup(&path);

        let env_key = env_api_key.map(|k| js_trim(&k).to_string()).filter(|k| !k.is_empty());
        let (api_key, key_status) = match (key.key, env_key) {
            (Some(k), _) => (Some(k), KeyStatus { source: KeyStatusSource::Stored(key.source), problem: key.problem }),
            (None, Some(env)) => {
                // A working key is available: like Electron (persistence.md §4.4) the failed stored key only warns
                // (the reason was logged by `load_api_key`); no notice and no forced Settings → Account (D-KEY-3).
                if key.problem.is_some() {
                    tracing::warn!(target: "data", "the stored API key could not be used; using ELEVENLABS_API_KEY");
                }
                (Some(env), KeyStatus { source: KeyStatusSource::Environment, problem: None })
            }
            (None, None) => (None, KeyStatus { source: KeyStatusSource::None, problem: key.problem }),
        };
        let current = Settings::from_stored(outcome.value.as_ref().unwrap_or(&Value::Null), api_key.is_some());
        let needs_rewrite = match &outcome.value {
            Some(raw) if !outcome.from_backup => current.needs_rewrite(raw),
            _ => false,
        };

        let state = Arc::new(Mutex::new(State { current, api_key, key_status }));
        let snap_state = state.clone();
        let doc = JsonDoc::new(path, opts, Arc::new(move || Some(snap_state.lock().current.stored_json())));
        let store = SettingsStore {
            inner: Arc::new(Inner {
                profile: profile.clone(),
                state,
                listeners: Mutex::new(Vec::new()),
                doc,
                secrets_lock: Mutex::new(()),
                protect,
            }),
        };
        if outcome.from_backup {
            // R-Profile: corrupt main file + good .bak → rewrite the main file right away.
            tracing::warn!(target: "data", "restored settings.json from backup; rewriting it");
            store.inner.doc.schedule();
            store.inner.doc.flush();
        } else if needs_rewrite {
            // The migration mechanism: clamps, dropped keys, a stray hasApiKey, key order (persistence.md §3.4).
            store.inner.doc.schedule();
        }
        store
    }

    /// The current settings (with the derived `has_api_key`).
    pub fn get(&self) -> Settings {
        self.inner.state.lock().current.clone()
    }

    /// Applies a JSON merge patch (contracts.md §4.3). Returns the resulting settings; invalid values in the patch
    /// silently keep their current value. A non-object patch is the caller's error (the core validates first:
    /// `Invalid settings patch`).
    pub fn update(&self, patch: &Value) -> Settings {
        let (next, prev) = {
            let mut st = self.inner.state.lock();
            let next = st.current.apply_patch(patch);
            if next == st.current {
                return next;
            }
            let prev = std::mem::replace(&mut st.current, next.clone());
            (next, prev)
        };
        self.inner.doc.schedule();
        self.emit(&SettingsChange { next: &next, prev: &prev, key_changed: false });
        next
    }

    /// Registers a change listener (called after every effective change, including API-key set/clear).
    pub fn subscribe(&self, listener: SettingsListener) {
        self.inner.listeners.lock().push(Arc::new(listener));
    }

    /// The API key (trimmed), if any.
    pub fn api_key(&self) -> Option<String> {
        self.inner.state.lock().api_key.clone()
    }

    pub fn has_api_key(&self) -> bool {
        self.inner.state.lock().api_key.is_some()
    }

    /// Where the key came from / why a stored key is unusable (drives `AppInfo::api_key_notice`).
    pub fn key_status(&self) -> KeyStatus {
        self.inner.state.lock().key_status
    }

    /// Stores (`Some`) or clears (`None`/blank) the key; returns the resulting settings. Persistence failures are
    /// logged; the key then stays in memory for this session only.
    pub fn set_api_key(&self, key: Option<&str>) -> Settings {
        let key = key.map(js_trim).filter(|k| !k.is_empty()).map(str::to_string);
        let (next, prev) = {
            let mut st = self.inner.state.lock();
            if st.api_key == key {
                return st.current.clone();
            }
            let prev = st.current.clone();
            st.current.has_api_key = key.is_some();
            st.key_status = KeyStatus {
                source: if key.is_some() { KeyStatusSource::Entered } else { KeyStatusSource::None },
                problem: None,
            };
            st.api_key = key.clone();
            (st.current.clone(), prev)
        };
        self.persist_key(key.as_deref());
        self.emit(&SettingsChange { next: &next, prev: &prev, key_changed: true });
        next
    }

    /// Writes `secrets-dpapi.bin` synchronously (tiny file; a log-off right after a key change keeps it, D-P-12).
    fn persist_key(&self, key: Option<&str>) {
        let inner = &self.inner;
        let _serial = inner.secrets_lock.lock();
        let result = match key {
            Some(k) => match save_api_key_with(&inner.profile, k, inner.protect) {
                Err(SecretError::Dpapi(reason)) => {
                    // D-P-19: no older key may come back at the next start; it asks for the key instead.
                    tracing::warn!(
                        target: "data",
                        "secure storage is unavailable; the API key is kept in memory for this session only ({reason})"
                    );
                    clear_api_key(&inner.profile)
                }
                other => other,
            },
            None => clear_api_key(&inner.profile),
        };
        if let Err(e) = result {
            tracing::warn!(target: "data", "could not persist the API key: {e}");
        }
    }

    fn emit(&self, change: &SettingsChange<'_>) {
        let listeners: Vec<Arc<SettingsListener>> = self.inner.listeners.lock().clone();
        for listener in listeners {
            if catch_unwind(AssertUnwindSafe(|| listener(change))).is_err() {
                tracing::warn!(target: "data", "settings listener failed");
            }
        }
    }

    /// Writes pending settings and waits for pending secret writes (quit).
    pub fn flush(&self) {
        self.inner.doc.flush();
        drop(self.inner.secrets_lock.lock());
    }

    /// Synchronous session-end write of `settings.json` (`.tmp-sync`).
    pub fn flush_sync(&self) {
        self.inner.doc.flush_sync();
    }

    /// The early synchronous read used before the stores open (persistence.md §3.5): missing → `None` (no `.bak`);
    /// unreadable main → try `.bak`; never writes a `.corrupt` copy; sanitized with `has_api_key = false`. The Rust app
    /// needs no pre-UI settings (media keys apply live), so only tools/tests use it.
    pub fn read_settings_file_sync(profile: &Profile) -> Option<Settings> {
        let main = profile.settings_file();
        let parsed = |bytes: Vec<u8>| parse_json_bytes(&bytes).ok().map(|v| Settings::from_stored(&v, false));
        match fs::read(&main) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
            Err(_) => {}
            Ok(bytes) => {
                if let Some(s) = parsed(bytes) {
                    return Some(s);
                }
            }
        }
        fs::read(with_suffix(&main, ".bak")).ok().and_then(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn fast() -> JsonDocOptions {
        JsonDocOptions {
            debounce: Duration::from_millis(5),
            max_wait: Duration::from_millis(40),
            backup: true,
            retry_base: Duration::from_millis(5),
            max_retries: 5,
        }
    }

    fn fake_protect(plain: &[u8]) -> Result<Vec<u8>, SecretError> {
        let mut v = b"FAKE:".to_vec();
        v.extend_from_slice(plain);
        Ok(v)
    }

    fn failing_protect(_: &[u8]) -> Result<Vec<u8>, SecretError> {
        Err(SecretError::Dpapi("unavailable".into()))
    }

    fn setup() -> (tempfile::TempDir, Profile) {
        let dir = tempfile::tempdir().unwrap();
        let p = Profile::at(dir.path(), false);
        (dir, p)
    }

    fn open(p: &Profile) -> SettingsStore {
        SettingsStore::open_with(p, None, KeyLoad::default(), fast(), fake_protect)
    }

    #[test]
    fn missing_file_gives_defaults_and_is_not_written() {
        let (_d, p) = setup();
        let s = open(&p);
        assert_eq!(s.get(), Settings::default());
        s.flush();
        assert!(!p.settings_file().exists());
        assert!(!s.has_api_key());
        assert_eq!(s.api_key(), None);
        assert_eq!(s.key_status(), KeyStatus::default());
    }

    #[test]
    fn update_writes_pretty_json_and_bak() {
        let (_d, p) = setup();
        let s = open(&p);
        let next = s.update(&json!({"volume": 0.5, "app": {"theme": "night"}}));
        assert_eq!(next.volume, 0.5);
        s.flush();
        let text = fs::read_to_string(p.settings_file()).unwrap();
        assert_eq!(text, s.get().stored_json());
        assert!(text.starts_with("{\n  \"version\": 1,\n  \"onboarded\": false,"));
        assert!(text.contains("\"customX\": null"));
        assert!(!text.contains("hasApiKey"));
        assert!(!text.ends_with('\n'));
        assert_eq!(fs::read_to_string(with_suffix(&p.settings_file(), ".bak")).unwrap(), text);
        assert_eq!(SettingsStore::read_settings_file_sync(&p).unwrap().volume, 0.5);
    }

    #[test]
    fn migration_rewrites_a_non_canonical_file_once() {
        let (_d, p) = setup();
        fs::write(
            p.settings_file(),
            r#"{"hasApiKey":true,"volume":7,"unknown":1,"library":{"cacheLimitMB":12.4},"version":1}"#,
        )
        .unwrap();
        let s = open(&p);
        assert_eq!(s.get().volume, 1.0);
        assert_eq!(s.get().library.cache_limit_mb, 50);
        assert!(!s.get().has_api_key, "a stray hasApiKey is ignored");
        s.flush();
        let text = fs::read_to_string(p.settings_file()).unwrap();
        assert_eq!(text, s.get().stored_json());
    }

    #[test]
    fn a_canonical_file_is_not_rewritten_even_when_formatted_differently() {
        let (_d, p) = setup();
        let compact = vox_core::json::to_js_compact(&Settings::default());
        fs::write(p.settings_file(), &compact).unwrap();
        let s = open(&p);
        s.flush();
        assert_eq!(fs::read_to_string(p.settings_file()).unwrap(), compact);
        assert!(!with_suffix(&p.settings_file(), ".bak").exists());
    }

    #[test]
    fn wrong_shape_loads_defaults_and_is_rewritten() {
        let (_d, p) = setup();
        fs::write(p.settings_file(), "[]").unwrap();
        let s = open(&p);
        assert_eq!(s.get(), Settings::default());
        s.flush();
        assert_eq!(fs::read_to_string(p.settings_file()).unwrap(), Settings::default().stored_json());
        assert!(!with_suffix(&p.settings_file(), ".corrupt").exists());
    }

    #[test]
    fn corrupt_main_with_good_bak_is_rewritten_immediately() {
        let (_d, p) = setup();
        let good = Settings { volume: 0.25, ..Settings::default() };
        fs::write(p.settings_file(), "{ not json").unwrap();
        fs::write(with_suffix(&p.settings_file(), ".bak"), good.stored_json()).unwrap();
        let s = open(&p);
        // Written during open (no flush needed).
        assert_eq!(fs::read_to_string(p.settings_file()).unwrap(), good.stored_json());
        assert_eq!(fs::read_to_string(with_suffix(&p.settings_file(), ".corrupt")).unwrap(), "{ not json");
        assert_eq!(s.get().volume, 0.25);
    }

    #[test]
    fn corrupt_main_and_bak_run_on_defaults_until_the_first_change() {
        let (_d, p) = setup();
        fs::write(p.settings_file(), "").unwrap();
        fs::write(with_suffix(&p.settings_file(), ".bak"), "nope").unwrap();
        let s = open(&p);
        s.flush();
        assert_eq!(fs::read_to_string(p.settings_file()).unwrap(), "");
        s.update(&json!({"onboarded": true}));
        s.flush();
        assert!(s.get().onboarded);
        assert_eq!(fs::read_to_string(p.settings_file()).unwrap(), s.get().stored_json());
    }

    #[test]
    fn no_op_update_neither_writes_nor_notifies() {
        let (_d, p) = setup();
        let s = open(&p);
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        s.subscribe(Box::new(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        }));
        s.update(&json!({"volume": 1}));
        s.update(&json!({"volume": "loud"}));
        s.update(&json!({"nope": true}));
        s.update(&json!(42));
        s.flush();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(!p.settings_file().exists());
    }

    #[test]
    fn listeners_run_in_order_with_prev_and_next() {
        let (_d, p) = setup();
        let s = open(&p);
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        for name in ["a", "b"] {
            let log = log.clone();
            s.subscribe(Box::new(move |c| {
                log.lock().push(format!("{name}:{}->{}:{}", c.prev.volume, c.next.volume, c.key_changed));
            }));
        }
        // A panicking listener does not stop the others.
        s.subscribe(Box::new(|_| panic!("listener bug")));
        let log2 = log.clone();
        s.subscribe(Box::new(move |_| log2.lock().push("d".into())));
        s.update(&json!({"volume": 0.4}));
        assert_eq!(*log.lock(), vec!["a:1->0.4:false", "b:1->0.4:false", "d"]);
    }

    #[test]
    fn a_listener_may_call_get_and_update() {
        let (_d, p) = setup();
        let s = open(&p);
        let order = Arc::new(Mutex::new(Vec::<String>::new()));
        let s2 = s.clone();
        let o = order.clone();
        s.subscribe(Box::new(move |c| {
            let now = s2.get();
            assert_eq!(now.volume, c.next.volume);
            o.lock().push(format!("saw {}", c.next.volume));
            if c.next.volume == 0.5 {
                // Re-entrant update: nests, and its listeners run before the outer call returns.
                s2.update(&json!({"volume": 0.6}));
                o.lock().push("nested returned".into());
            }
        }));
        let out = s.update(&json!({"volume": 0.5}));
        assert_eq!(out.volume, 0.5, "the outer call returns its own result");
        assert_eq!(s.get().volume, 0.6);
        assert_eq!(*order.lock(), vec!["saw 0.5", "saw 0.6", "nested returned"]);
    }

    #[test]
    fn key_status_sources() {
        let (_d, p) = setup();
        let stored = KeyLoad { key: Some("sk_stored".into()), source: KeySource::Migrated, problem: None };
        let s = SettingsStore::open_with(&p, Some("sk_env".into()), stored, fast(), fake_protect);
        assert_eq!(s.api_key().as_deref(), Some("sk_stored"));
        assert_eq!(s.key_status().source, KeyStatusSource::Stored(KeySource::Migrated));
        assert!(s.get().has_api_key);

        let s = SettingsStore::open_with(&p, Some("  sk_env \n".into()), KeyLoad::default(), fast(), fake_protect);
        assert_eq!(s.api_key().as_deref(), Some("sk_env"));
        assert_eq!(s.key_status(), KeyStatus { source: KeyStatusSource::Environment, problem: None });

        // A stored key that failed, with the environment key working: no problem is reported (no notice and no
        // forced Settings → Account at every start while reads work).
        for problem in [KeyProblem::MigrationFailed, KeyProblem::Unreadable] {
            let failed = KeyLoad { key: None, source: KeySource::None, problem: Some(problem) };
            let s = SettingsStore::open_with(&p, Some("sk_env".into()), failed, fast(), fake_protect);
            assert_eq!(s.api_key().as_deref(), Some("sk_env"));
            assert_eq!(s.key_status(), KeyStatus { source: KeyStatusSource::Environment, problem: None });
        }

        let failed = KeyLoad { key: None, source: KeySource::None, problem: Some(KeyProblem::MigrationFailed) };
        let s = SettingsStore::open_with(&p, Some("   ".into()), failed, fast(), fake_protect);
        assert_eq!(s.api_key(), None);
        assert_eq!(s.key_status(), KeyStatus { source: KeyStatusSource::None, problem: Some(KeyProblem::MigrationFailed) });
        // Setting a key clears the problem.
        s.set_api_key(Some("sk_new"));
        assert_eq!(s.key_status(), KeyStatus { source: KeyStatusSource::Entered, problem: None });
    }

    #[test]
    fn set_api_key_persists_emits_and_never_touches_settings_json() {
        let (_d, p) = setup();
        fs::write(p.secrets_file(), b"electron-blob").unwrap();
        let s = open(&p);
        let changes = Arc::new(Mutex::new(Vec::<(bool, bool, bool)>::new()));
        let c = changes.clone();
        s.subscribe(Box::new(move |ch| c.lock().push((ch.prev.has_api_key, ch.next.has_api_key, ch.key_changed))));

        let next = s.set_api_key(Some("  sk_first  "));
        assert!(next.has_api_key);
        assert_eq!(s.api_key().as_deref(), Some("sk_first"));
        assert_eq!(fs::read(p.secrets_dpapi_file()).unwrap(), b"FAKE:sk_first");
        // Same key: no-op.
        s.set_api_key(Some("sk_first"));
        // Replaced: has_api_key unchanged, but a change is still emitted.
        s.set_api_key(Some("sk_second"));
        assert_eq!(fs::read(p.secrets_dpapi_file()).unwrap(), b"FAKE:sk_second");
        // Blank clears and writes the tombstone.
        s.set_api_key(Some("   "));
        assert!(!s.has_api_key());
        assert_eq!(fs::read(p.secrets_dpapi_file()).unwrap().len(), 0);
        s.set_api_key(None); // already clear: no-op
        assert_eq!(*changes.lock(), vec![(false, true, true), (true, true, true), (true, false, true)]);
        s.flush();
        assert!(!p.settings_file().exists(), "the key never writes settings.json");
        assert_eq!(fs::read(p.secrets_file()).unwrap(), b"electron-blob", "secrets.bin untouched");
    }

    #[test]
    fn dpapi_failure_keeps_the_key_in_memory_and_writes_the_tombstone() {
        let (_d, p) = setup();
        fs::write(p.secrets_dpapi_file(), b"an older key blob").unwrap();
        let s = SettingsStore::open_with(&p, None, KeyLoad::default(), fast(), failing_protect);
        s.set_api_key(Some("sk_session_only"));
        assert_eq!(s.api_key().as_deref(), Some("sk_session_only"));
        assert!(s.get().has_api_key);
        assert_eq!(fs::read(p.secrets_dpapi_file()).unwrap().len(), 0, "the older key cannot come back");
    }

    #[test]
    fn env_key_same_value_is_a_no_op() {
        let (_d, p) = setup();
        let s = SettingsStore::open_with(&p, Some("sk_env".into()), KeyLoad::default(), fast(), fake_protect);
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        s.subscribe(Box::new(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        }));
        s.set_api_key(Some("sk_env"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(!p.secrets_dpapi_file().exists(), "an environment key is never persisted");
    }

    #[test]
    fn flush_sync_writes_settings() {
        let (_d, p) = setup();
        let s = SettingsStore::open_with(
            &p,
            None,
            KeyLoad::default(),
            JsonDocOptions { debounce: Duration::from_secs(60), max_wait: Duration::from_secs(60), ..fast() },
            fake_protect,
        );
        s.update(&json!({"onboarded": true}));
        s.flush_sync();
        assert_eq!(fs::read_to_string(p.settings_file()).unwrap(), s.get().stored_json());
    }

    #[test]
    fn early_sync_read_rules() {
        let (_d, p) = setup();
        let bak = with_suffix(&p.settings_file(), ".bak");
        fs::write(&bak, r#"{"volume":0.3}"#).unwrap();
        // Missing main file: no .bak fallback.
        assert_eq!(SettingsStore::read_settings_file_sync(&p), None);
        // Unparsable main → .bak.
        fs::write(p.settings_file(), "").unwrap();
        assert_eq!(SettingsStore::read_settings_file_sync(&p).unwrap().volume, 0.3);
        // Wrong shape is not a failure: all defaults.
        fs::write(p.settings_file(), "null").unwrap();
        assert_eq!(SettingsStore::read_settings_file_sync(&p), Some(Settings::default()));
        // BOM + valid.
        fs::write(p.settings_file(), "\u{FEFF}{\"volume\":0.9,\"hasApiKey\":true}").unwrap();
        let s = SettingsStore::read_settings_file_sync(&p).unwrap();
        assert_eq!((s.volume, s.has_api_key), (0.9, false));
        // Both bad → None, and no .corrupt copy.
        fs::write(p.settings_file(), "x").unwrap();
        fs::write(&bak, "y").unwrap();
        assert_eq!(SettingsStore::read_settings_file_sync(&p), None);
        assert!(!with_suffix(&p.settings_file(), ".corrupt").exists());
    }

    #[test]
    #[cfg(windows)]
    fn open_migrates_with_real_dpapi() {
        let (_d, p) = setup();
        fs::write(p.secrets_file(), crate::secrets::dpapi_protect(b"sk_from_electron_0123456789").unwrap()).unwrap();
        let s = SettingsStore::open(&p, Some("sk_env_ignored".into()));
        assert_eq!(s.api_key().as_deref(), Some("sk_from_electron_0123456789"));
        assert_eq!(s.key_status().source, KeyStatusSource::Stored(KeySource::Migrated));
        assert!(s.get().has_api_key);
        let s = SettingsStore::open(&p, None);
        assert_eq!(s.key_status().source, KeyStatusSource::Stored(KeySource::Dpapi));
        s.set_api_key(None);
        let s = SettingsStore::open(&p, None);
        assert_eq!(s.api_key(), None, "the tombstone stops a re-migration");
    }
}
