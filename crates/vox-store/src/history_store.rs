//! The history store (persistence.md §5, contracts.md §3.6).
//!
//! In memory: records newest first + an id index. `history.json` = `{"version":1,"records":[…]}` compact, written
//! through a [`crate::json_doc::JsonDoc`] (2.5 s / 10 s debounce, `.bak`). Load: `vox_core::history::
//! parse_history_file` (sanitize, stable sort by `createdAt` desc, de-dupe), warning `ignored N malformed history
//! record(s)`. Mutations: `add` replaces a same-id record and inserts at `insertion_index`; `update` applies a
//! typed patch (`ReadRecord::apply_patch`), returns the existing record unchanged when the result is equal (no write,
//! no notification), re-inserts when `createdAt` changed. Removal paths (`remove`, `clear`, `apply_retention`)
//! call the `on_removed` listeners **synchronously** with the removed ids (the core deletes their cached audio) and
//! delete `history.json.corrupt`. Change notifications: debounced 100 ms (max wait 400 ms) on the store's timer
//! thread; every mutation counts. Listeners run on that timer thread and must only enqueue (vox-app sends
//! `Command::HistoryChanged`).
//!
//! Open: main file corrupt and `.bak` good → the restored records are written back to `history.json` **at once**
//! (BRIEF.md Product rulings; log `restored history.json from backup; rewriting it`). A parsed value of the wrong
//! shape — neither an array nor an object whose `records` is an array (`{}`, `{"records":5}`, `null`, `42`) — is
//! treated as corrupt (`JsonRead::Corrupt("unexpected shape")`), so it goes through the `.corrupt` copy, the `.bak`
//! fallback and the immediate rewrite instead of silently loading zero records and overwriting `.bak` on the next
//! mutation (ARCHITECTURE.md §8 D-P-16; settings.json keeps the spec behaviour: wrong shape → defaults). Test: main
//! `{}` + a good `.bak` → records restored and `history.json.corrupt` exists. Records keep their offsets in UTF-16
//! units (`charCount`); the store never converts text offsets (ARCHITECTURE.md §5.6).
//!
//! Listeners (`on_removed`, and the change listener's timer thread) are invoked **after the internal lock is
//! released**, iterating a cloned listener list: a listener may call any store method (`with_all`, `get`, …) without
//! deadlocking on the non-reentrant mutex. Test: an `on_removed` listener that calls `with_all`.

use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use serde_json::Value;
use vox_core::constants::{APP_NAME, DAY_MS, HISTORY_NOTIFY_DEBOUNCE_MS, HISTORY_NOTIFY_MAX_WAIT_MS};
use vox_core::history::{
    count_expiring, insertion_index, list_records, parse_history_file, HistoryExport, HistoryFile, HistoryPage,
    HistoryQuery, ReadRecord, RecordPatch,
};
use vox_core::json::{to_js_compact, to_js_pretty};

use crate::fsutil::{load_with_backup_checked, write_file_atomic};
use crate::json_doc::{JsonDoc, JsonDocOptions};
use crate::paths::Profile;

type ChangeListener = Arc<dyn Fn() + Send + Sync>;
type RemovedListener = Arc<dyn Fn(&[String]) + Send + Sync>;

/// Records newest first + an id → position index.
#[derive(Default)]
struct Records {
    list: Vec<ReadRecord>,
    index: HashMap<String, usize>,
}

impl Records {
    fn new(list: Vec<ReadRecord>) -> Self {
        let mut r = Records { list, index: HashMap::new() };
        r.reindex();
        r
    }

    fn reindex(&mut self) {
        self.index = self.list.iter().enumerate().map(|(i, r)| (r.id.clone(), i)).collect();
    }

    fn get(&self, id: &str) -> Option<&ReadRecord> {
        self.index.get(id).map(|&i| &self.list[i])
    }

    /// Removes any record with the same id, then inserts after every strictly newer record and before equal ones.
    fn insert(&mut self, rec: ReadRecord) {
        if let Some(&i) = self.index.get(&rec.id) {
            self.list.remove(i);
        }
        let at = insertion_index(&self.list, rec.created_at);
        self.list.insert(at, rec);
        self.reindex();
    }
}

/// The debounced change notification (100 ms, max wait 400 ms) on its own timer thread.
struct Notifier {
    state: Mutex<NotifyState>,
    cv: Condvar,
    listeners: Mutex<Vec<ChangeListener>>,
    debounce: Duration,
    max_wait: Duration,
}

#[derive(Default)]
struct NotifyState {
    deadline: Option<Instant>,
    first: Option<Instant>,
    shutdown: bool,
    /// No timer thread: notify synchronously.
    inline: bool,
}

impl Notifier {
    fn start(debounce: Duration, max_wait: Duration) -> Arc<Notifier> {
        let n = Arc::new(Notifier {
            state: Mutex::new(NotifyState::default()),
            cv: Condvar::new(),
            listeners: Mutex::new(Vec::new()),
            debounce,
            max_wait,
        });
        let worker = n.clone();
        if let Err(e) = std::thread::Builder::new().name("vox-history-notify".into()).spawn(move || worker.run()) {
            tracing::error!(target: "data", "could not start the history notifier: {e}; notifying inline");
            n.state.lock().inline = true;
        }
        n
    }

    fn notify(&self) {
        let mut st = self.state.lock();
        if st.inline {
            drop(st);
            self.fire();
            return;
        }
        let now = Instant::now();
        let first = match (st.deadline, st.first) {
            (Some(_), Some(first)) => first,
            _ => now,
        };
        st.first = Some(first);
        let wait = self.debounce.min((first + self.max_wait).saturating_duration_since(now));
        st.deadline = Some(now + wait);
        self.cv.notify_all();
    }

    fn fire(&self) {
        let listeners: Vec<ChangeListener> = self.listeners.lock().clone();
        for l in listeners {
            if catch_unwind(AssertUnwindSafe(|| l())).is_err() {
                tracing::warn!(target: "data", "history listener failed");
            }
        }
    }

    fn run(&self) {
        let mut st = self.state.lock();
        loop {
            if st.shutdown {
                return;
            }
            match st.deadline {
                Some(d) if Instant::now() >= d => {
                    st.deadline = None;
                    parking_lot::MutexGuard::unlocked(&mut st, || self.fire());
                }
                Some(d) => {
                    self.cv.wait_until(&mut st, d);
                }
                None => self.cv.wait(&mut st),
            }
        }
    }

    fn shutdown(&self) {
        self.state.lock().shutdown = true;
        self.cv.notify_all();
    }
}

struct Inner {
    records: Arc<Mutex<Records>>,
    doc: JsonDoc,
    notifier: Arc<Notifier>,
    removed_listeners: Mutex<Vec<RemovedListener>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.notifier.shutdown();
    }
}

/// `history.json` holds an array of records or an object whose `records` is an array (D-P-16).
fn is_history_shape(v: &Value) -> bool {
    v.is_array() || v.get("records").is_some_and(Value::is_array)
}

/// Thread-safe history store handle.
#[derive(Clone)]
pub struct HistoryStore {
    inner: Arc<Inner>,
}

impl HistoryStore {
    /// Loads `history.json` (missing/corrupt per [`crate::fsutil::load_with_backup`]).
    pub fn open(profile: &Profile) -> HistoryStore {
        Self::open_with(
            profile,
            JsonDocOptions::history(),
            Duration::from_millis(HISTORY_NOTIFY_DEBOUNCE_MS),
            Duration::from_millis(HISTORY_NOTIFY_MAX_WAIT_MS),
        )
    }

    /// [`HistoryStore::open`] with explicit document options and notification timings (tests).
    pub(crate) fn open_with(
        profile: &Profile,
        opts: JsonDocOptions,
        notify_debounce: Duration,
        notify_max_wait: Duration,
    ) -> HistoryStore {
        let path = profile.history_file();
        let outcome = load_with_backup_checked(&path, is_history_shape);
        let (list, rejected) = outcome.value.as_ref().map(parse_history_file).unwrap_or_default();
        if rejected > 0 {
            tracing::warn!(target: "data", "ignored {rejected} malformed history record(s)");
        }
        let records = Arc::new(Mutex::new(Records::new(list)));
        let snap = records.clone();
        let doc = JsonDoc::new(
            path,
            opts,
            Arc::new(move || Some(to_js_compact(&HistoryFile { version: 1, records: &snap.lock().list }))),
        );
        let store = HistoryStore {
            inner: Arc::new(Inner {
                records,
                doc,
                notifier: Notifier::start(notify_debounce, notify_max_wait),
                removed_listeners: Mutex::new(Vec::new()),
            }),
        };
        if outcome.from_backup {
            // R-Profile: corrupt main file + good .bak → rewrite the main file right away.
            tracing::warn!(target: "data", "restored history.json from backup; rewriting it");
            store.inner.doc.schedule();
            store.inner.doc.flush();
        }
        store
    }

    fn mutated(&self) {
        self.inner.doc.schedule();
        self.inner.notifier.notify();
    }

    /// Adds (or replaces by id) a record; it is normalised first. A record without id/createdAt is ignored with a
    /// warning.
    pub fn add(&self, record: ReadRecord) {
        let rec = record.normalized();
        if rec.id.is_empty() || !rec.created_at.is_finite() {
            tracing::warn!(target: "data", "historyStore.add() ignored a record without id/createdAt");
            return;
        }
        self.inner.records.lock().insert(rec);
        self.mutated();
    }

    /// Patches a record. `None` = unknown id. Returns the stored record (unchanged when the patch was a no-op).
    pub fn update(&self, id: &str, patch: &RecordPatch) -> Option<ReadRecord> {
        let next = {
            let mut r = self.inner.records.lock();
            let idx = *r.index.get(id)?;
            let existing = &r.list[idx];
            let next = existing.apply_patch(patch);
            if next.same_as(existing) {
                return Some(existing.clone());
            }
            if next.created_at == existing.created_at {
                r.list[idx] = next.clone();
            } else {
                r.insert(next.clone());
            }
            next
        };
        self.mutated();
        Some(next)
    }

    pub fn get(&self, id: &str) -> Option<ReadRecord> {
        self.inner.records.lock().get(id).cloned()
    }

    /// The newest record (by `createdAt`).
    pub fn newest(&self) -> Option<ReadRecord> {
        self.inner.records.lock().list.first().cloned()
    }

    /// `history:list` (vox_core::history::list_records semantics).
    pub fn list(&self, query: &HistoryQuery) -> HistoryPage {
        list_records(&self.inner.records.lock().list, query)
    }

    /// A copy of every record, newest first.
    pub fn all(&self) -> Vec<ReadRecord> {
        self.inner.records.lock().list.clone()
    }

    /// Runs `f` over the records without copying them (analytics, reconciliation). `f` runs under the store lock:
    /// it must not call back into this store.
    pub fn with_all<R>(&self, f: impl FnOnce(&[ReadRecord]) -> R) -> R {
        f(&self.inner.records.lock().list)
    }

    pub fn len(&self) -> usize {
        self.inner.records.lock().list.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Removes one record; `false` for an unknown id (no write, no event).
    pub fn remove(&self, id: &str) -> bool {
        !self.remove_where(|r| r.id == id).is_empty()
    }

    /// Removes every record; returns how many (0 = no-op).
    pub fn clear(&self) -> usize {
        self.remove_where(|_| true).len()
    }

    /// `removeWhere`: one mutation, one synchronous `on_removed` call (ids newest first) after the lock is released,
    /// then the `.corrupt` copy is deleted (it may still hold the removed reads).
    fn remove_where(&self, pred: impl Fn(&ReadRecord) -> bool) -> Vec<String> {
        let removed = {
            let mut r = self.inner.records.lock();
            let mut removed = Vec::new();
            r.list.retain(|rec| {
                if pred(rec) {
                    removed.push(rec.id.clone());
                    false
                } else {
                    true
                }
            });
            if !removed.is_empty() {
                r.reindex();
            }
            removed
        };
        if removed.is_empty() {
            return removed;
        }
        self.mutated();
        let listeners: Vec<RemovedListener> = self.inner.removed_listeners.lock().clone();
        for l in listeners {
            if catch_unwind(AssertUnwindSafe(|| l(&removed))).is_err() {
                tracing::warn!(target: "data", "history remove listener failed");
            }
        }
        self.inner.doc.discard_corrupt_copy();
        removed
    }

    /// Flips `favorite`; returns the updated record.
    pub fn toggle_favorite(&self, id: &str) -> Option<ReadRecord> {
        let favorite = self.inner.records.lock().get(id)?.favorite;
        self.update(id, &RecordPatch { favorite: Some(!favorite), ..RecordPatch::default() })
    }

    /// Deletes non-favorite records with `createdAt < now − days × 86 400 000`; `days == 0` → 0. Logs
    /// `retention removed N records`.
    pub fn apply_retention(&self, days: u32, now_ms: f64) -> usize {
        if days == 0 {
            return 0;
        }
        let cutoff = now_ms - days as f64 * DAY_MS;
        let n = self.remove_where(|r| r.created_at < cutoff && !r.favorite).len();
        if n > 0 {
            tracing::info!(target: "app", "retention removed {n} records");
        }
        n
    }

    /// How many reads `apply_retention(days, now)` would delete (the Settings retention guard).
    pub fn count_expiring(&self, days: u32, now_ms: f64) -> usize {
        count_expiring(&self.inner.records.lock().list, days, now_ms)
    }

    /// Sets `cached` on many records in one mutation (cache clear, eviction, reconciliation).
    pub fn set_cached(&self, ids: &[String], cached: bool) {
        let changed = {
            let mut r = self.inner.records.lock();
            let mut changed = false;
            for id in ids {
                if let Some(&i) = r.index.get(id) {
                    if r.list[i].cached != cached {
                        r.list[i].cached = cached;
                        changed = true;
                    }
                }
            }
            changed
        };
        if changed {
            self.mutated();
        }
    }

    /// Writes the export JSON (`HistoryExport`, pretty, atomic, no `.bak`) to `path`. `exported_at_iso` comes from
    /// `vox_core::time::iso_utc_ms(now)` (`2026-10-17T09:44:00.000Z`).
    pub fn export_to(&self, path: &Path, exported_at_iso: &str) -> std::io::Result<()> {
        let text = {
            let r = self.inner.records.lock();
            to_js_pretty(&HistoryExport { app: APP_NAME, version: 1, exported_at: exported_at_iso.to_string(), records: &r.list })
        };
        write_file_atomic(path, text.as_bytes(), false)
    }

    /// Debounced change notification (no payload).
    pub fn on_change(&self, listener: Box<dyn Fn() + Send + Sync>) {
        self.inner.notifier.listeners.lock().push(Arc::from(listener));
    }

    /// Synchronous removal notification with the removed ids (called on the mutating thread — the core — after the
    /// lock is released). vox-app wires it to `AudioCache::remove`, which only updates the index and queues the
    /// deletions, so a `ClearHistory` or retention pass over hundreds of cached reads never stalls the core.
    #[allow(clippy::type_complexity)] // the skeleton's public signature
    pub fn on_removed(&self, listener: Box<dyn Fn(&[String]) + Send + Sync>) {
        self.inner.removed_listeners.lock().push(Arc::from(listener));
    }

    pub fn flush(&self) {
        self.inner.doc.flush();
    }

    pub fn flush_sync(&self) {
        self.inner.doc.flush_sync();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::with_suffix;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn fast() -> JsonDocOptions {
        JsonDocOptions {
            debounce: Duration::from_millis(5),
            max_wait: Duration::from_millis(40),
            backup: true,
            retry_base: Duration::from_millis(5),
            max_retries: 5,
        }
    }

    fn setup() -> (tempfile::TempDir, Profile) {
        let dir = tempfile::tempdir().unwrap();
        let p = Profile::at(dir.path(), false);
        (dir, p)
    }

    fn open(p: &Profile) -> HistoryStore {
        HistoryStore::open_with(p, fast(), Duration::from_millis(20), Duration::from_millis(80))
    }

    fn rec(id: &str, at: f64) -> ReadRecord {
        let mut r = ReadRecord::new(id.into(), at, vox_core::history::ReadSource::Manual);
        r.text = format!("text of {id}");
        r.char_count = r.text.len() as u64;
        r
    }

    fn ids(s: &HistoryStore) -> Vec<String> {
        s.all().into_iter().map(|r| r.id).collect()
    }

    fn wait_for(counter: &AtomicUsize, n: usize) -> bool {
        let end = Instant::now() + Duration::from_secs(10);
        while Instant::now() < end {
            if counter.load(Ordering::SeqCst) >= n {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        counter.load(Ordering::SeqCst) >= n
    }

    fn change_counter(s: &HistoryStore) -> Arc<AtomicUsize> {
        let n = Arc::new(AtomicUsize::new(0));
        let c = n.clone();
        s.on_change(Box::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
        }));
        n
    }

    #[test]
    fn load_sanitizes_sorts_and_dedupes() {
        let (_d, p) = setup();
        fs::write(
            p.history_file(),
            r#"{"version":1,"records":[{"id":"a","createdAt":1},{"id":"b","createdAt":3},{"id":"a","createdAt":2},{"createdAt":5},{"id":"c"},7]}"#,
        )
        .unwrap();
        let s = open(&p);
        assert_eq!(ids(&s), vec!["b", "a"]);
        assert_eq!(s.get("a").unwrap().created_at, 2.0, "the first after sorting wins");
        assert_eq!(s.len(), 2);
        // A bare array is accepted too.
        fs::write(p.history_file(), r#"[{"id":"x","createdAt":1}]"#).unwrap();
        assert_eq!(ids(&open(&p)), vec!["x"]);
    }

    #[test]
    fn writes_compact_newest_first_and_round_trips() {
        let (_d, p) = setup();
        let s = open(&p);
        s.add(rec("old", 100.0));
        s.add(rec("new", 200.0));
        s.flush();
        let text = fs::read_to_string(p.history_file()).unwrap();
        assert!(text.starts_with(r#"{"version":1,"records":[{"id":"new","createdAt":200,"#), "{text}");
        assert_eq!(fs::read_to_string(with_suffix(&p.history_file(), ".bak")).unwrap(), text);
        let again = open(&p);
        assert_eq!(again.all(), s.all());
    }

    #[test]
    fn insertion_ties_put_the_newer_insertion_first() {
        let (_d, p) = setup();
        let s = open(&p);
        s.add(rec("a", 100.0));
        s.add(rec("b", 100.0));
        s.add(rec("c", 50.0));
        s.add(rec("d", 150.0));
        assert_eq!(ids(&s), vec!["d", "b", "a", "c"]);
        // Re-adding replaces by id (and re-inserts).
        s.add(rec("a", 100.0));
        assert_eq!(ids(&s), vec!["d", "a", "b", "c"]);
        assert_eq!(s.newest().unwrap().id, "d");
    }

    #[test]
    fn add_rejects_records_without_id_or_created_at_and_normalises() {
        let (_d, p) = setup();
        let s = open(&p);
        s.add(rec("  ", 1.0));
        s.add(rec("x", f64::NAN));
        assert!(s.is_empty());
        let mut r = rec(" padded ", 1.0);
        r.source_process = Some(" Chrome.EXE ".into());
        r.source_app = Some("   ".into());
        s.add(r);
        let got = s.get("padded").unwrap();
        assert_eq!(got.source_process.as_deref(), Some("chrome.exe"));
        assert_eq!(got.source_app, None);
    }

    #[test]
    fn update_equality_is_a_no_op() {
        let (_d, p) = setup();
        let s = open(&p);
        let changes = change_counter(&s);
        s.add(rec("a", 1.0));
        s.flush();
        assert!(wait_for(&changes, 1));
        let before = fs::read_to_string(p.history_file()).unwrap();
        let same = s.update("a", &RecordPatch { listened_sec: Some(0.0), completed: Some(false), ..RecordPatch::default() });
        assert_eq!(same.unwrap(), s.get("a").unwrap());
        assert!(s.update("missing", &RecordPatch::default()).is_none());
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(changes.load(Ordering::SeqCst), 1, "no notification");
        s.flush();
        assert_eq!(fs::read_to_string(p.history_file()).unwrap(), before);

        let next = s.update("a", &RecordPatch { listened_sec: Some(4.0), ..RecordPatch::default() }).unwrap();
        assert_eq!(next.listened_sec, 4.0);
        assert!(wait_for(&changes, 2));
    }

    #[test]
    fn update_with_a_new_created_at_re_inserts() {
        let (_d, p) = setup();
        let s = open(&p);
        s.add(rec("a", 100.0));
        s.add(rec("b", 200.0));
        s.add(rec("c", 300.0));
        s.update("a", &RecordPatch { created_at: Some(250.0), ..RecordPatch::default() });
        assert_eq!(ids(&s), vec!["c", "a", "b"]);
        // Same createdAt: replaced in place.
        s.update("b", &RecordPatch { favorite: Some(true), ..RecordPatch::default() });
        assert_eq!(ids(&s), vec!["c", "a", "b"]);
        assert!(s.get("b").unwrap().favorite);
        // The id can never change; clearable strings clear.
        let mut with_app = rec("d", 50.0);
        with_app.source_app = Some("Chrome".into());
        s.add(with_app);
        let d = s.update("d", &RecordPatch { source_app: Some(None), ..RecordPatch::default() }).unwrap();
        assert_eq!(d.source_app, None);
        assert_eq!(d.id, "d");
    }

    #[test]
    fn toggle_favorite_and_set_cached() {
        let (_d, p) = setup();
        let s = open(&p);
        s.add(rec("a", 1.0));
        s.add(rec("b", 2.0));
        assert!(s.toggle_favorite("a").unwrap().favorite);
        assert!(!s.toggle_favorite("a").unwrap().favorite);
        assert!(s.toggle_favorite("zz").is_none());
        s.set_cached(&["a".into(), "b".into(), "zz".into()], true);
        assert!(s.get("a").unwrap().cached && s.get("b").unwrap().cached);
        s.set_cached(&["a".into()], false);
        assert!(!s.get("a").unwrap().cached && s.get("b").unwrap().cached);
    }

    #[test]
    fn retention_spares_favourites_and_reports_once() {
        let (_d, p) = setup();
        let s = open(&p);
        let now = 100.0 * DAY_MS;
        s.add(rec("fresh", now - 1.0 * DAY_MS));
        s.add(rec("old", now - 10.0 * DAY_MS));
        let mut fav = rec("old-fav", now - 20.0 * DAY_MS);
        fav.favorite = true;
        s.add(fav);
        s.add(rec("older", now - 30.0 * DAY_MS));
        let removed = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let r2 = removed.clone();
        s.on_removed(Box::new(move |ids| r2.lock().push(ids.to_vec())));
        assert_eq!(s.count_expiring(7, now), 2);
        assert_eq!(s.count_expiring(0, now), 0);
        assert_eq!(s.apply_retention(0, now), 0);
        assert_eq!(s.apply_retention(7, now), 2);
        assert_eq!(ids(&s), vec!["fresh", "old-fav"]);
        assert_eq!(*removed.lock(), vec![vec!["old".to_string(), "older".to_string()]]);
        assert_eq!(s.apply_retention(7, now), 0);
        assert_eq!(removed.lock().len(), 1);
    }

    #[test]
    fn on_removed_is_synchronous_and_may_reenter() {
        let (_d, p) = setup();
        let s = open(&p);
        s.add(rec("a", 1.0));
        s.add(rec("b", 2.0));
        let seen = Arc::new(Mutex::new(Vec::<(Vec<String>, usize)>::new()));
        let (s2, seen2) = (s.clone(), seen.clone());
        s.on_removed(Box::new(move |ids| {
            // Re-entrant: the store lock is released before listeners run.
            let left = s2.with_all(|all| all.len());
            assert!(s2.get(&ids[0]).is_none());
            seen2.lock().push((ids.to_vec(), left));
        }));
        s.on_removed(Box::new(|_| panic!("listener bug")));
        assert!(s.remove("a"));
        assert_eq!(*seen.lock(), vec![(vec!["a".to_string()], 1)]);
        assert!(!s.remove("a"), "unknown id: no-op");
        assert_eq!(s.clear(), 1);
        assert_eq!(s.clear(), 0);
        assert_eq!(seen.lock().len(), 2);
        assert!(s.is_empty());
    }

    #[test]
    fn removal_deletes_the_corrupt_copy() {
        let (_d, p) = setup();
        fs::write(p.history_file(), "{ broken").unwrap();
        let s = open(&p);
        let corrupt = with_suffix(&p.history_file(), ".corrupt");
        assert!(corrupt.exists());
        assert!(s.is_empty());
        s.add(rec("a", 1.0));
        assert!(corrupt.exists(), "adding keeps it");
        s.remove("a");
        assert!(!corrupt.exists());
    }

    #[test]
    fn wrong_shape_with_a_good_bak_is_restored_and_rewritten() {
        let (_d, p) = setup();
        let good = r#"{"version":1,"records":[{"id":"kept","createdAt":5}]}"#;
        fs::write(p.history_file(), "{}").unwrap();
        fs::write(with_suffix(&p.history_file(), ".bak"), good).unwrap();
        let s = open(&p);
        assert_eq!(ids(&s), vec!["kept"]);
        assert_eq!(fs::read_to_string(with_suffix(&p.history_file(), ".corrupt")).unwrap(), "{}");
        // Rewritten at once (no flush needed).
        let main = fs::read_to_string(p.history_file()).unwrap();
        assert!(main.contains("\"kept\""), "{main}");
        for odd in [r#"{"records":5}"#, "null", "42"] {
            fs::write(p.history_file(), odd).unwrap();
            assert_eq!(ids(&open(&p)), vec!["kept"], "{odd}");
        }
    }

    #[test]
    fn wrong_shape_without_bak_loads_nothing_and_writes_nothing() {
        let (_d, p) = setup();
        fs::write(p.history_file(), "{}").unwrap();
        let s = open(&p);
        assert!(s.is_empty());
        s.flush();
        assert_eq!(fs::read_to_string(p.history_file()).unwrap(), "{}");
    }

    #[test]
    fn missing_main_does_not_fall_back_to_bak() {
        let (_d, p) = setup();
        fs::write(with_suffix(&p.history_file(), ".bak"), r#"[{"id":"a","createdAt":1}]"#).unwrap();
        assert!(open(&p).is_empty());
    }

    #[test]
    fn list_and_search() {
        let (_d, p) = setup();
        let s = open(&p);
        let mut a = rec("a", 1.0);
        a.text = "The quick brown fox".into();
        a.source_app = Some("Chrome".into());
        let mut b = rec("b", 2.0);
        b.text = "Lazy dogs".into();
        b.favorite = true;
        b.voice_name = "George".into();
        s.add(a);
        s.add(b);
        let q = |search: &str| HistoryQuery { search: Some(search.into()), ..HistoryQuery::default() };
        assert_eq!(s.list(&q("QUICK chrome")).total, 1);
        assert_eq!(s.list(&q("george lazy")).items[0].id, "b");
        assert_eq!(s.list(&q("fox george")).total, 0);
        assert_eq!(s.list(&HistoryQuery { favorites_only: true, ..HistoryQuery::default() }).total, 1);
        let page = s.list(&HistoryQuery { limit: Some(1.0), offset: Some(1.0), ..HistoryQuery::default() });
        assert_eq!((page.total, page.items[0].id.as_str()), (2, "a"));
    }

    #[test]
    fn change_notifications_are_debounced() {
        let (_d, p) = setup();
        let s = HistoryStore::open_with(&p, fast(), Duration::from_millis(400), Duration::from_millis(5_000));
        let changes = change_counter(&s);
        for i in 0..5 {
            s.add(rec(&format!("r{i}"), i as f64));
        }
        assert!(wait_for(&changes, 1));
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(changes.load(Ordering::SeqCst), 1, "one notification for the burst");
        s.remove("r1");
        assert!(wait_for(&changes, 2));
    }

    #[test]
    fn continuous_changes_notify_within_the_max_wait() {
        let (_d, p) = setup();
        let s = HistoryStore::open_with(&p, fast(), Duration::from_millis(400), Duration::from_millis(600));
        let changes = change_counter(&s);
        let start = Instant::now();
        let mut i = 0;
        // A change every 20 ms keeps re-arming the 400 ms debounce: only the max wait can fire.
        while changes.load(Ordering::SeqCst) == 0 && start.elapsed() < Duration::from_secs(5) {
            s.add(rec(&format!("r{i}"), i as f64));
            i += 1;
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(changes.load(Ordering::SeqCst) >= 1);
        assert!(start.elapsed() < Duration::from_millis(3_000), "within the max wait (+ generous slack)");
    }

    #[test]
    fn export_is_pretty_with_metadata() {
        let (_d, p) = setup();
        let s = open(&p);
        s.add(rec("a", 1.0));
        let out = p.root().join("exports").join("Vox-history-2026-10-17.json");
        s.export_to(&out, "2026-10-17T09:44:00.000Z").unwrap();
        let text = fs::read_to_string(&out).unwrap();
        assert!(text.starts_with("{\n  \"app\": \"Vox\",\n  \"version\": 1,\n  \"exportedAt\": \"2026-10-17T09:44:00.000Z\",\n  \"records\": [\n    {\n      \"id\": \"a\""), "{text}");
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["records"].as_array().unwrap().len(), 1);
        assert!(!with_suffix(&out, ".bak").exists());
        assert!(!with_suffix(&out, ".tmp").exists());
    }

    #[test]
    fn flush_sync_persists_pending_changes() {
        let (_d, p) = setup();
        let s = HistoryStore::open_with(
            &p,
            JsonDocOptions { debounce: Duration::from_secs(60), max_wait: Duration::from_secs(60), ..fast() },
            Duration::from_millis(20),
            Duration::from_millis(80),
        );
        s.add(rec("a", 1.0));
        s.flush_sync();
        assert!(fs::read_to_string(p.history_file()).unwrap().contains("\"a\""));
    }

    #[test]
    fn real_demo_history_round_trips_through_the_store() {
        let (_d, p) = setup();
        let s = open(&p);
        let now = chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2026, 10, 17, 12, 0, 0).unwrap();
        let demo = crate::demo::generate_demo_records(now, 120);
        for r in demo.clone() {
            s.add(r);
        }
        assert_eq!(s.all(), demo, "already canonical and newest first");
        s.flush();
        assert_eq!(open(&p).all(), demo);
    }
}
