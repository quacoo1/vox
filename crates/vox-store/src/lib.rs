//! # vox-store
//!
//! Everything Vox keeps on disk and everything computed from it (persistence.md). Same folder (`%APPDATA%\Vox`,
//! demo: `%APPDATA%\Vox-Demo`), same files, same formats as the Electron app, so "migration" is zero-copy except for
//! the API key: the Electron `secrets.bin` (Chromium `v10`) is decrypted once and saved as a raw DPAPI blob in the
//! new `secrets-dpapi.bin`; `secrets.bin` is never modified (BRIEF.md "API key"; ARCHITECTURE.md §6.4).
//!
//! | Module | Spec |
//! |---|---|
//! | [`paths`] | persistence.md §1 |
//! | [`fsutil`] | persistence.md §2.1–§2.4 (busy retries, atomic writes, lenient reads, `.bak` / `.corrupt`) |
//! | [`json_doc`] | persistence.md §2.5 (debounced durable document with a writer thread) |
//! | [`settings_store`] | persistence.md §3, contracts.md §4 |
//! | [`secrets`] | persistence.md §4, BRIEF.md "API key" (`secrets-dpapi.bin`, migration) |
//! | [`history_store`] | persistence.md §5, contracts.md §3.6 |
//! | [`audio_cache`] | persistence.md §6 |
//! | [`analytics`] | persistence.md §7 |
//! | [`demo`] | persistence.md §8, contracts.md §7.3 |
//! | [`log`] | persistence.md §9, shell.md §1.3 |
//! | [`ui_state`] | ui-main.md §5.1 (analytics range; a new small file, not part of settings) |
//! | [`window_state`] | BRIEF.md "Main window remembers its size, position and maximized state" (`window-state.json`) |
//!
//! Threading: every store handle is `Clone + Send + Sync` (shared state behind a mutex) and every method is
//! synchronous and short; file writes happen on per-document writer threads ([`json_doc`]) and on the audio cache's
//! I/O worker ([`audio_cache`]: saves, touches, redactions and folder deletions, FIFO). The core thread is the main
//! caller. Owner: the vox-store engineer.

pub mod analytics;
pub mod audio_cache;
pub mod demo;
pub mod error;
pub mod fsutil;
pub mod history_store;
pub mod json_doc;
pub mod log;
pub mod paths;
pub mod secrets;
pub mod settings_store;
pub mod ui_state;
pub mod window_state;

pub use audio_cache::AudioCache;
pub use error::StoreError;
pub use history_store::HistoryStore;
pub use paths::Profile;
pub use settings_store::SettingsStore;

#[cfg(test)]
mod real_profile {
    //! The manual real-profile check of ARCHITECTURE.md §9 ("Read a copy of a real Electron profile"). It reads
    //! `%APPDATA%\Vox` without modifying it: settings, history, `secrets.bin` and `Session\Local State` are copied
    //! to a temporary profile, every cache manifest is parsed in place (read-only) and a few cache entries are copied
    //! and replayed. Only aggregates are printed (never the key or any read text).
    //!
    //! Run on a machine with an Electron Vox profile, as the user who owns it:
    //! `cargo test -p vox-store -- --ignored real_profile --nocapture`

    use std::fs;
    use std::path::Path;

    use crate::audio_cache::parse_manifest;
    use crate::fsutil::{read_json_file, JsonRead};
    use crate::settings_store::KeyStatusSource;
    use crate::{AudioCache, HistoryStore, Profile, SettingsStore};

    fn copy_if_exists(from: &Path, to: &Path) {
        if from.is_file() {
            if let Some(dir) = to.parent() {
                fs::create_dir_all(dir).unwrap();
            }
            fs::copy(from, to).unwrap();
        }
    }

    fn copy_dir(from: &Path, to: &Path) {
        fs::create_dir_all(to).unwrap();
        for e in fs::read_dir(from).unwrap().flatten() {
            if e.file_type().unwrap().is_file() {
                fs::copy(e.path(), to.join(e.file_name())).unwrap();
            }
        }
    }

    #[test]
    #[ignore = "reads the real %APPDATA%\\Vox profile of the user running the tests (on a temporary copy)"]
    fn real_profile_copy_reads() {
        let real = Profile::resolve(false).unwrap();
        assert!(real.settings_file().exists() || real.history_file().exists(), "no Electron profile at {}", real.root().display());
        let tmp = tempfile::tempdir().unwrap();
        let copy = Profile::at(tmp.path(), false);
        for name in ["settings.json", "settings.json.bak", "history.json", "history.json.bak", "secrets.bin"] {
            copy_if_exists(&real.root().join(name), &copy.root().join(name));
        }
        for (src, dst) in real.local_state_candidates().iter().zip(copy.local_state_candidates().iter()) {
            copy_if_exists(src, dst);
        }

        // Settings + key migration.
        assert!(SettingsStore::read_settings_file_sync(&copy).is_some());
        let settings = SettingsStore::open(&copy, None);
        let status = settings.key_status();
        let key = settings.api_key();
        eprintln!(
            "settings: canonical={} key source={:?} problem={:?} key looks valid={}",
            match read_json_file(&real.settings_file()) {
                JsonRead::Ok(v) => !settings.get().needs_rewrite(&v),
                _ => false,
            },
            status.source,
            status.problem,
            key.as_deref().is_some_and(|k| k.starts_with("sk_") && k.len() > 20)
        );
        if real.secrets_file().exists() {
            assert_eq!(status.problem, None, "the Electron key migrates for the user who owns it");
            assert!(matches!(status.source, KeyStatusSource::Stored(_)));
            assert!(copy.secrets_dpapi_file().exists());
        }
        assert_eq!(fs::read(real.secrets_file()).ok(), fs::read(copy.secrets_file()).ok(), "secrets.bin untouched");

        // History: every record loads; the compact re-serialisation is compared with the Electron bytes.
        let history = HistoryStore::open(&copy);
        let original = fs::read_to_string(real.history_file()).unwrap_or_default();
        let ours = vox_core::json::to_js_compact(&vox_core::history::HistoryFile { version: 1, records: &history.all() });
        eprintln!("history: {} records, byte-identical re-serialisation={}", history.len(), ours == original);
        if let JsonRead::Ok(v) = read_json_file(&real.history_file()) {
            let (records, rejected) = vox_core::history::parse_history_file(&v);
            assert_eq!(rejected, 0);
            assert_eq!(records.len(), history.len());
        }
        let summary = crate::analytics::compute_analytics(&history.all(), vox_core::analytics::AnalyticsRange::All, chrono::Local::now());
        eprintln!(
            "analytics(all): reads={} listened={}s days={} streak={:?}",
            summary.totals.reads,
            summary.totals.listened_sec,
            summary.daily.len(),
            summary.streak
        );

        // Audio cache: parse every manifest in place (read-only), then replay a few copied entries.
        let mut valid = Vec::new();
        let mut invalid = 0;
        if let Ok(rd) = fs::read_dir(real.audio_cache_dir()) {
            for e in rd.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())) {
                let name = e.file_name().to_string_lossy().into_owned();
                match read_json_file(&e.path().join("manifest.json")) {
                    JsonRead::Ok(v) => match parse_manifest(&v, &name, 0.0) {
                        Some(m) => {
                            let again: serde_json::Value = serde_json::from_str(&m.to_json()).unwrap();
                            assert_eq!(parse_manifest(&again, &name, 0.0).as_ref(), Some(&m));
                            valid.push((name, m));
                        }
                        None => invalid += 1,
                    },
                    _ => invalid += 1,
                }
            }
        }
        eprintln!("audio cache: {} valid manifests, {} invalid", valid.len(), invalid);
        for (name, m) in valid.iter().take(3) {
            copy_dir(&real.audio_cache_dir().join(name), &copy.audio_cache_dir().join(name));
            assert!(m.chunks.iter().all(|c| c.text_end >= c.text_start));
        }
        let cache = AudioCache::open(&copy);
        for (name, m) in valid.iter().take(3) {
            assert!(cache.has(name));
            let chunks = cache.load(name).unwrap();
            assert_eq!(chunks.len(), m.chunks.len());
            if let Some(rec) = history.get(name) {
                let len16 = rec.char_count as u32;
                assert!(chunks.iter().all(|c| c.text_end <= len16 || rec.text_truncated), "UTF-16 offsets fit the text");
            }
        }
        assert!(cache.flush(std::time::Duration::from_secs(10)));
    }
}
