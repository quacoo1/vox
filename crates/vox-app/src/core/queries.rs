//! History / analytics / cache / app-info queries and actions (contracts.md §5.3).
//!
//! - `QueryHistory`: `history.list(q)` (+ the unfiltered total when filtered).
//! - `QueryHome`: `compute_analytics(all, 7d, Local::now())` + the 5 newest.
//! - `QueryAnalytics`: `compute_analytics(all, range, Local::now())` (no debounce in the core; the page debounces
//!   background refreshes only).
//! - `RemoveReads` / `ClearHistory` (cached audio removed through `on_removed` → `AudioCache::remove`, index-only),
//!   `ToggleFavorite` → `FavoriteToggled{req, Ok(record)}`.
//! - `ExportHistory{req, owner}`: save dialog (`vox_win::shell::save_file_dialog(owner, …)`, title `Export history`,
//!   default `<Documents>\Vox-history-<UTC YYYY-MM-DD>.json`, filter `JSON`) on a worker → `history.export_to(path,
//!   vox_core::time::iso_utc_ms(now))` → `Exported`.
//! - `CountExpiring` (retention guard) → `ExpiringCount{req, Ok(n)}` (a worker for large histories; failure → `Err`).
//! - `QueryCacheStats`; `ClearCache` **on the core thread**: `cache.clear()` (index-only; the I/O worker deletes) →
//!   `history.set_cached(<ids of records with cached = true>, false)` → `CacheStats{req, Ok(stats)}`.
//! - `CopyText` → `Done{req}` only (the requesting controller toasts its own copy; History: id `history-copy`).
//! - `QueryAppInfo` → `AppInfo` (runtime label instead of the Electron version; hook availability from
//!   `hook_status`; Explorer verb installed; bridge running/port).

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use vox_bridge::BridgeServer;
use vox_core::analytics::AnalyticsRange;
use vox_core::app::AppInfo;
use vox_core::copy::SOMETHING_WENT_WRONG;
use vox_core::history::HistoryQuery;
use vox_core::text16::utf16_len;
use vox_store::secrets::KeyProblem;
use vox_store::SettingsStore;

use super::{now_unix_ms, spawn_worker, Core};
use crate::command::{RequestId, RevealTarget};
use crate::runtime::Runtime;
use crate::update::UiUpdate;

/// Histories larger than this count expiring reads on a worker instead of the core thread.
const COUNT_ON_WORKER_ABOVE: usize = 2_000;
/// The Home page's recent list.
const HOME_RECENT: usize = 5;
/// `requireId`'s length cap (UTF-16 units).
const MAX_ID_UNITS: usize = 128;

/// `Vox-history-YYYY-MM-DD.json` with the **UTC** date (parity, contracts.md §8 #3).
pub fn export_file_name(now: DateTime<Utc>) -> String {
    format!("{}-history-{}.json", vox_core::constants::APP_NAME, now.format("%Y-%m-%d"))
}

/// `requireId`: a non-empty string of at most 128 UTF-16 units (not trimmed).
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && utf16_len(id) <= MAX_ID_UNITS
}

/// `AppInfo` (contracts.md §3.8) from the runtime, the settings store, the bridge and the last hook status.
pub fn build_app_info(runtime: &Runtime, settings: &SettingsStore, bridge: &BridgeServer, hook_status: Option<bool>) -> AppInfo {
    let current = settings.get();
    let api_key_notice = settings.key_status().problem.map(|p| match p {
        KeyProblem::MigrationFailed => vox_core::copy::key_migration_failed(),
        KeyProblem::Unreadable => vox_core::copy::key_unreadable(),
    });
    let port = bridge.port();
    AppInfo {
        name: vox_core::constants::APP_NAME.to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        runtime: runtime.runtime_label.clone(),
        build: runtime.build_hash.clone(),
        api_key_notice,
        is_packaged: runtime.is_packaged,
        user_data_path: runtime.profile.root().to_path_buf(),
        extension_path: runtime.extension_dir.clone(),
        server_port: if port != 0 { port } else { current.server.port },
        server_running: bridge.running(),
        native_hook_available: vox_core::app::native_hook_available(hook_status),
        explorer_menu_installed: catch_unwind(vox_win::registry::is_explorer_menu_installed).unwrap_or(false),
        demo_mode: runtime.flags.demo,
    }
}

impl Core {
    pub(super) fn query_history(&mut self, req: RequestId, query: &HistoryQuery, with_grand_total: bool) {
        let page = self.services.history.list(query);
        let all = if with_grand_total { self.services.history.len() } else { page.total };
        self.publish(UiUpdate::HistoryPage { req, result: Ok((page, all)) });
    }

    pub(super) fn query_home(&mut self, req: RequestId) {
        let summary = self.services.history.with_all(|records| vox_store::analytics::compute_analytics(records, AnalyticsRange::D7, chrono::Local::now()));
        let recent = self.services.history.list(&HistoryQuery { limit: Some(HOME_RECENT as f64), ..HistoryQuery::default() }).items;
        self.publish(UiUpdate::Home { req, result: Ok((Arc::new(summary), recent)) });
    }

    pub(super) fn query_analytics(&mut self, req: RequestId, range: AnalyticsRange) {
        let summary = self.services.history.with_all(|records| vox_store::analytics::compute_analytics(records, range, chrono::Local::now()));
        self.publish(UiUpdate::Analytics { req, result: Ok(Arc::new(summary)) });
    }

    /// History delete-with-undo commit (one or more ids). Unknown ids are no-ops; an invalid id fails the request.
    pub(super) fn remove_reads(&mut self, req: RequestId, ids: &[String]) {
        if let Some(bad) = ids.iter().find(|id| !valid_id(id)) {
            tracing::debug!(target: "history", "remove refused an invalid id ({} units)", utf16_len(bad));
            self.publish(UiUpdate::Done { req, result: Err("Invalid id".into()) });
            return;
        }
        let removed = ids.iter().filter(|id| self.services.history.remove(id)).count();
        tracing::debug!(target: "history", "removed {removed} of {} reads", ids.len());
        self.publish(UiUpdate::Done { req, result: Ok(()) });
    }

    pub(super) fn clear_history(&mut self, req: RequestId) {
        let n = self.services.history.clear();
        tracing::info!(target: "history", "history cleared ({n} reads)");
        self.publish(UiUpdate::Done { req, result: Ok(()) });
    }

    pub(super) fn toggle_favorite(&mut self, req: RequestId, id: &str) {
        let result = if valid_id(id) { Ok(self.services.history.toggle_favorite(id)) } else { Err("Invalid id".to_string()) };
        self.publish(UiUpdate::FavoriteToggled { req, result });
    }

    /// Save dialog + export on a worker (the dialog blocks its caller until the user closes it).
    pub(super) fn export_history(&mut self, req: RequestId, owner: Option<isize>) {
        let history = self.services.history.clone();
        let ui = self.ui.clone();
        let fallback_dir = self.services.runtime.profile.root().to_path_buf();
        let spawned = spawn_worker("vox-export", move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                let now = Utc::now();
                let dir: PathBuf = vox_win::shell::documents_folder().unwrap_or(fallback_dir);
                let default_path = dir.join(export_file_name(now));
                let Some(path) = vox_win::shell::save_file_dialog(owner, "Export history", &default_path, "JSON", "json") else {
                    return Ok(None);
                };
                let exported_at = vox_core::time::iso_utc_ms(vox_core::time::to_ms(&now));
                match history.export_to(&path, &exported_at) {
                    Ok(()) => {
                        tracing::info!(target: "history", "history exported");
                        Ok(Some(path))
                    }
                    Err(e) => {
                        tracing::warn!(target: "history", "history export failed: {e}");
                        Err(e.to_string())
                    }
                }
            }))
            .unwrap_or_else(|_| Err(SOMETHING_WENT_WRONG.to_string()));
            ui.send(UiUpdate::Exported { req, result });
        });
        if !spawned {
            self.publish(UiUpdate::Exported { req, result: Err(SOMETHING_WENT_WRONG.to_string()) });
        }
    }

    /// The Settings retention guard's count (a worker for large histories).
    pub(super) fn count_expiring(&mut self, req: RequestId, days: u32) {
        let now = now_unix_ms();
        if self.services.history.len() <= COUNT_ON_WORKER_ABOVE {
            let n = self.services.history.count_expiring(days, now);
            self.publish(UiUpdate::ExpiringCount { req, result: Ok(n) });
            return;
        }
        let history = self.services.history.clone();
        let ui = self.ui.clone();
        let spawned = spawn_worker("vox-count-expiring", move || {
            let result = catch_unwind(AssertUnwindSafe(|| history.count_expiring(days, now))).map_err(|_| SOMETHING_WENT_WRONG.to_string());
            ui.send(UiUpdate::ExpiringCount { req, result });
        });
        if !spawned {
            self.publish(UiUpdate::ExpiringCount { req, result: Err(SOMETHING_WENT_WRONG.to_string()) });
        }
    }

    /// `cache:clear` on the core thread (index-only; the I/O worker deletes), then clear every `cached` flag.
    pub(super) fn clear_cache(&mut self, req: RequestId) {
        let stats = self.services.cache.clear();
        let cached: Vec<String> = self.services.history.with_all(|records| records.iter().filter(|r| r.cached).map(|r| r.id.clone()).collect());
        if !cached.is_empty() {
            self.services.history.set_cached(&cached, false);
        }
        tracing::info!(target: "cache", "audio cache cleared ({} reads)", cached.len());
        self.publish(UiUpdate::CacheStats { req, result: Ok(stats) });
    }

    /// History "Copy text", copy chips.
    pub(super) fn copy_text(&mut self, req: RequestId, text: String) {
        let result = match catch_unwind(AssertUnwindSafe(|| vox_win::clipboard::write_clipboard_text(&text))) {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => {
                tracing::warn!(target: "clipboard", "copy failed: {e}");
                Err(e.to_string())
            }
            Err(_) => Err(SOMETHING_WENT_WRONG.to_string()),
        };
        self.publish(UiUpdate::Done { req, result });
    }

    /// `app:open-external` (http(s) only).
    pub(super) fn open_external(&mut self, url: String, req: Option<RequestId>) {
        let result = if !vox_core::read::is_http_url(&url) {
            tracing::warn!(target: "app", "refused to open a link that is not http(s)");
            Err("Only http(s) links can be opened".to_string())
        } else {
            match catch_unwind(AssertUnwindSafe(|| vox_win::shell::open_external(&url))) {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => {
                    tracing::warn!(target: "app", "could not open a link: {e}");
                    Err(e.to_string())
                }
                Err(_) => Err(SOMETHING_WENT_WRONG.to_string()),
            }
        };
        if let Some(req) = req {
            self.publish(UiUpdate::Done { req, result });
        }
    }

    /// `app:reveal-path` (failures only logged).
    pub(super) fn reveal_path(&mut self, target: RevealTarget) {
        let path = match target {
            RevealTarget::UserData => self.services.runtime.profile.root().to_path_buf(),
            RevealTarget::Extension => self.services.runtime.extension_dir.clone(),
        };
        match catch_unwind(AssertUnwindSafe(|| vox_win::shell::reveal_path(&path))) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(target: "app", "could not open the folder: {e}"),
            Err(_) => tracing::warn!(target: "app", "could not open the folder (panicked)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn export_names_use_the_utc_date() {
        let t = Utc.with_ymd_and_hms(2026, 10, 9, 23, 59, 59).unwrap();
        assert_eq!(export_file_name(t), "Vox-history-2026-10-09.json");
        let t = Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0).unwrap();
        assert_eq!(export_file_name(t), "Vox-history-2027-01-01.json");
    }

    #[test]
    fn require_id_rules() {
        assert!(!valid_id(""));
        assert!(valid_id(" "), "not trimmed");
        assert!(valid_id(&"a".repeat(128)));
        assert!(!valid_id(&"a".repeat(129)));
        assert!(!valid_id(&"😀".repeat(65)), "UTF-16 units, not chars");
        assert!(valid_id(&"😀".repeat(64)));
    }
}
