//! The audio cache (persistence.md §6): `audio-cache\<readId>\manifest.json` + `<index>.<ext>` chunk files.
//!
//! - Manifest (compact JSON, written atomically **last** so a missing manifest = interrupted save):
//!   `{version:1, readId, createdAt, lastAccess, chunks:[{index, textStart, textEnd, mime, alignment|null,
//!   durationSec, file, bytes}]}`. `textStart/textEnd` are UTF-16 offsets into the read's **full** text (unchanged
//!   from Electron; no conversion). Validation: [`parse_manifest`].
//! - Index in memory: `readId → {bytes, files, lastAccess, hasText}` behind one mutex.
//! - `open` scans and repairs (invalid folder names deleted; entries with a missing/corrupt manifest or missing/empty
//!   chunk files deleted; plain files at the root left alone), with up to 8 folders in parallel.
//! - `load` rewrites `lastAccess` (LRU = last save or load). `prune` evicts by ascending `lastAccess`, never the
//!   protected ids; prunes are serialized.
//!
//! **I/O worker** (ARCHITECTURE.md §2.3, §3.1; persistence.md §6.4 "per-id operations strictly in order"): the cache
//! owns one internal thread with a FIFO job queue that performs **every** disk mutation — saves, touches (the
//! `lastAccess` rewrite of a load), redactions and folder deletions — so operations on one id run in submission order
//! without per-id locks (a plain mutex is not FIFO) and the core thread never waits on disk:
//! - [`AudioCache::remove`], [`AudioCache::prune`], [`AudioCache::clear`] and [`AudioCache::remove_orphans`] update
//!   the in-memory index **synchronously** (so [`AudioCache::has`], [`AudioCache::stats`] and the returned evicted ids
//!   are right at once, like Electron dropping index entries before deleting folders) and **queue** the folder
//!   deletions (each up to ~600 ms of busy retries); failures log `cache remove failed` (scope `data`).
//! - [`AudioCache::save_async`] / [`AudioCache::load_async`] queue the job and call `done` on the worker when it
//!   finished (vox-app's callbacks only enqueue a `Command`). A save adds its id to the index only when it completed;
//!   a deletion queued after a save of the same id runs after it. A save that completes while a `remove`/`prune`/
//!   `remove_orphans` of its id or a `clear` is queued is **discarded**: it is not indexed, the queued deletion removes
//!   its folder, and `done` gets `Err` (so the caller leaves `cached = false`; `Ok` always means "indexed"). The core
//!   re-checks the record when the save's result arrives (`Command::CacheSaved`): record gone → `remove(id)` again, so
//!   no orphan survives.
//! - [`AudioCache::save`] / [`AudioCache::load`] are the blocking forms (queue + wait), for tests and tools only.
//! - [`AudioCache::flush`] waits (bounded) until the queue is empty; called by the core's quit sequence inside the
//!   3 s flush budget.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs;
use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex};
use serde::Serialize;
use serde_json::Value;
use vox_core::cache::CacheStats;
use vox_core::ids::is_valid_read_id;
use vox_core::js::js_round;
use vox_core::json::{js_f64, to_js_compact};
use vox_core::tts::{ext_for_mime, Alignment, AudioBytes, CachedChunk};

use crate::error::StoreError;
use crate::fsutil::{read_json_file, remove_dir_all_retry, retry_busy, write_file_atomic, JsonRead};
use crate::paths::Profile;

const MANIFEST: &str = "manifest.json";
const SCAN_CONCURRENCY: usize = 8;
/// The error of a save that completed after a deletion of its id (or a clear) had been queued.
const DISCARDED_SAVE: &str = "the cache entry was removed or cleared while it was being saved";

/// One chunk entry of a manifest.
#[derive(Debug, Clone, PartialEq)]
pub struct ManifestChunk {
    pub index: u32,
    pub text_start: u32,
    pub text_end: u32,
    pub mime: String,
    pub alignment: Option<Alignment>,
    pub duration_sec: f64,
    /// `^\d{1,6}\.(mp3|wav|ogg|bin)$`
    pub file: String,
    pub bytes: u64,
}

/// A parsed `manifest.json`.
#[derive(Debug, Clone, PartialEq)]
pub struct CacheManifest {
    pub read_id: String,
    pub created_at: f64,
    pub last_access: f64,
    pub chunks: Vec<ManifestChunk>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ManifestJson<'a> {
    version: u32,
    read_id: &'a str,
    #[serde(serialize_with = "js_f64")]
    created_at: f64,
    #[serde(serialize_with = "js_f64")]
    last_access: f64,
    chunks: Vec<ChunkJson<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ChunkJson<'a> {
    index: u32,
    text_start: u32,
    text_end: u32,
    mime: &'a str,
    /// `null` when absent (never omitted).
    alignment: Option<&'a Alignment>,
    #[serde(serialize_with = "js_f64")]
    duration_sec: f64,
    file: &'a str,
    bytes: u64,
}

impl CacheManifest {
    /// Compact JSON in the Electron field order.
    pub fn to_json(&self) -> String {
        to_js_compact(&ManifestJson {
            version: 1,
            read_id: &self.read_id,
            created_at: self.created_at,
            last_access: self.last_access,
            chunks: self
                .chunks
                .iter()
                .map(|c| ChunkJson {
                    index: c.index,
                    text_start: c.text_start,
                    text_end: c.text_end,
                    mime: &c.mime,
                    alignment: c.alignment.as_ref(),
                    duration_sec: c.duration_sec,
                    file: &c.file,
                    bytes: c.bytes,
                })
                .collect(),
        })
    }

    fn has_text(&self) -> bool {
        self.chunks.iter().any(|c| c.alignment.as_ref().is_some_and(Alignment::has_text))
    }
}

fn finite(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::Number(n)) => n.as_f64().filter(|x| x.is_finite()),
        _ => None,
    }
}

/// finite ? `js_round(max(0, x))` saturated : 0 (D-P-15).
fn non_neg_rounded(v: Option<&Value>, max: f64) -> f64 {
    finite(v).map(|x| js_round(x.max(0.0)).min(max)).unwrap_or(0.0)
}

fn parse_alignment(v: Option<&Value>) -> Option<Alignment> {
    let o = v?.as_object()?;
    let chars = o.get("chars")?.as_array()?;
    let starts = o.get("starts")?.as_array()?;
    let ends = o.get("ends")?.as_array()?;
    if chars.len() != starts.len() || chars.len() != ends.len() {
        return None;
    }
    let times = |a: &[Value]| a.iter().map(|x| finite(Some(x)).unwrap_or(f64::NAN)).collect::<Vec<f64>>();
    Some(Alignment {
        chars: chars.iter().map(|c| c.as_str().unwrap_or("").to_string()).collect(),
        starts: times(starts),
        ends: times(ends),
    })
}

/// `parseManifest` (persistence.md §6.3): `None` unless an object whose `readId` equals `folder` and whose `chunks`
/// is a non-empty array of objects with a valid `file` and a string `mime`; `createdAt` falls back to `now_ms`,
/// `lastAccess` to `createdAt`.
///
/// Normalisation of values §6.3 accepts "as is" but the typed fields cannot hold (ARCHITECTURE.md §8 D-P-15):
/// `index`, `textStart`, `textEnd` → finite ? `js_round(max(0, x))` saturated to `u32` : 0; `bytes` likewise to
/// `u64`; `durationSec` → finite ? x : 0; the alignment is kept only if all three arrays exist with equal lengths,
/// where a non-string `chars` element becomes `""` and a non-number / non-finite `starts`/`ends` element becomes
/// `f64::NAN` (so `Alignment::usable_for_timing()` rejects it; `to_json` writes `null`). Touch and redact rewrite the
/// normalised values (accepted under R-Profile). Tests: `1.5`, `-3`, a `null` start, a numeric char.
pub fn parse_manifest(v: &Value, folder: &str, now_ms: f64) -> Option<CacheManifest> {
    let o = v.as_object()?;
    if o.get("readId").and_then(Value::as_str) != Some(folder) {
        return None;
    }
    let raw_chunks = o.get("chunks")?.as_array()?;
    if raw_chunks.is_empty() {
        return None;
    }
    let mut chunks = Vec::with_capacity(raw_chunks.len());
    for raw in raw_chunks {
        let c = raw.as_object()?;
        let file = c.get("file")?.as_str()?;
        if !is_valid_chunk_file(file) {
            return None;
        }
        let mime = c.get("mime")?.as_str()?;
        let u32_max = u32::MAX as f64;
        chunks.push(ManifestChunk {
            index: non_neg_rounded(c.get("index"), u32_max) as u32,
            text_start: non_neg_rounded(c.get("textStart"), u32_max) as u32,
            text_end: non_neg_rounded(c.get("textEnd"), u32_max) as u32,
            mime: mime.to_string(),
            alignment: parse_alignment(c.get("alignment")),
            duration_sec: finite(c.get("durationSec")).unwrap_or(0.0),
            file: file.to_string(),
            bytes: non_neg_rounded(c.get("bytes"), u64::MAX as f64) as u64,
        });
    }
    let created_at = finite(o.get("createdAt")).unwrap_or(now_ms);
    let last_access = finite(o.get("lastAccess")).unwrap_or(created_at);
    Some(CacheManifest { read_id: folder.to_string(), created_at, last_access, chunks })
}

/// `^\d{1,6}\.(mp3|wav|ogg|bin)$`.
pub fn is_valid_chunk_file(name: &str) -> bool {
    let Some((digits, ext)) = name.split_once('.') else { return false };
    (1..=6).contains(&digits.len())
        && digits.bytes().all(|b| b.is_ascii_digit())
        && matches!(ext, "mp3" | "wav" | "ogg" | "bin")
}

/// Σ of `values`, saturating at `u64::MAX`.
fn saturating_sum(values: impl Iterator<Item = u64>) -> u64 {
    values.fold(0, u64::saturating_add)
}

fn now_ms() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64
}

/// Completion callback of [`AudioCache::save_async`] (runs on the cache's I/O worker; must only enqueue).
pub type SaveDone = Box<dyn FnOnce(Result<(), StoreError>) + Send>;
/// Completion callback of [`AudioCache::load_async`] (`None` = missing or broken entry, which was deleted).
pub type LoadDone = Box<dyn FnOnce(Option<Vec<CachedChunk>>) + Send>;

/// One index entry: `bytes` = manifest + audio bytes; `seq` = insertion order (ties of the LRU sort).
#[derive(Debug, Clone, Copy)]
struct Entry {
    bytes: u64,
    files: u64,
    last_access: f64,
    has_text: bool,
    seq: u64,
}

#[derive(Default)]
struct Index {
    entries: HashMap<String, Entry>,
    next_seq: u64,
    /// Deletions queued and not yet run, per id. A save or load finishing while one is pending for its id (or while
    /// a clear is pending) does not index its entry: the queued deletion runs after it (FIFO) and removes the folder.
    pending_removals: HashMap<String, usize>,
    pending_clears: usize,
}

impl Index {
    /// Inserts or refreshes an entry; an existing entry keeps its insertion position.
    fn put(&mut self, id: &str, bytes: u64, files: u64, last_access: f64, has_text: bool) {
        let seq = match self.entries.get(id) {
            Some(e) => e.seq,
            None => {
                self.next_seq += 1;
                self.next_seq
            }
        };
        self.entries.insert(id.to_string(), Entry { bytes, files, last_access, has_text, seq });
    }

    fn blocked(&self, id: &str) -> bool {
        self.pending_clears > 0 || self.pending_removals.get(id).is_some_and(|n| *n > 0)
    }

    /// Drops the entry now and records the deletion that is about to be queued.
    fn evict(&mut self, id: &str) {
        self.entries.remove(id);
        *self.pending_removals.entry(id.to_string()).or_insert(0) += 1;
    }

    fn removal_done(&mut self, id: &str) {
        if let Some(n) = self.pending_removals.get_mut(id) {
            *n -= 1;
            if *n == 0 {
                self.pending_removals.remove(id);
            }
        }
    }

    /// Ids in insertion order.
    fn ordered_ids(&self) -> Vec<String> {
        let mut v: Vec<(&String, u64)> = self.entries.iter().map(|(k, e)| (k, e.seq)).collect();
        v.sort_by_key(|(_, seq)| *seq);
        v.into_iter().map(|(k, _)| k.clone()).collect()
    }

    /// Σ entry bytes (saturating: a redacted manifest may carry `bytes` saturated at `u64::MAX`, D-P-15).
    fn total_bytes(&self) -> u64 {
        saturating_sum(self.entries.values().map(|e| e.bytes))
    }

    fn stats(&self) -> CacheStats {
        CacheStats {
            reads: self.entries.len() as u64,
            files: self.entries.values().fold(0, |t, e| t.saturating_add(e.files)),
            bytes: self.total_bytes(),
        }
    }
}

enum Job {
    Save { read_id: String, chunks: Vec<CachedChunk>, done: SaveDone },
    Load { read_id: String, done: LoadDone },
    Redact { read_id: String },
    Remove { read_id: String },
    /// Delete everything inside the root plus these (indexed at the time of the call) ids.
    Clear { ids: Vec<String> },
}

#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
    busy: bool,
    shutdown: bool,
    /// No worker thread: jobs run on the caller's thread.
    inline: bool,
}

struct Shared {
    root: PathBuf,
    index: Mutex<Index>,
    queue: Mutex<Queue>,
    work_cv: Condvar,
    idle_cv: Condvar,
}

/// Stops the I/O worker (after it drained its queue) when the last handle is dropped.
struct WorkerGuard {
    shared: Arc<Shared>,
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        self.shared.queue.lock().shutdown = true;
        self.shared.work_cv.notify_all();
    }
}

/// Thread-safe audio cache handle.
#[derive(Clone)]
pub struct AudioCache {
    shared: Arc<Shared>,
    _guard: Arc<WorkerGuard>,
}

impl AudioCache {
    /// Scans and repairs `audio-cache\` (creates it if missing; on failure the cache stays empty for the session) and
    /// starts the I/O worker.
    pub fn open(profile: &Profile) -> AudioCache {
        let root = profile.audio_cache_dir();
        let mut index = Index::default();
        match fs::create_dir_all(&root) {
            Err(e) => tracing::warn!(target: "data", "could not create the audio cache directory: {e}"),
            Ok(()) => match fs::read_dir(&root) {
                Err(e) => tracing::warn!(target: "data", "could not list the audio cache: {e}"),
                Ok(rd) => {
                    let names: Vec<std::ffi::OsString> = rd
                        .flatten()
                        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                        .map(|e| e.file_name())
                        .collect();
                    let mut found = scan(&root, &names);
                    found.sort_by(|a, b| a.0.cmp(&b.0));
                    for (id, s) in found {
                        index.put(&id, s.bytes, s.files, s.last_access, s.has_text);
                    }
                }
            },
        }
        let shared = Arc::new(Shared {
            root,
            index: Mutex::new(index),
            queue: Mutex::new(Queue::default()),
            work_cv: Condvar::new(),
            idle_cv: Condvar::new(),
        });
        let worker = shared.clone();
        if let Err(e) = std::thread::Builder::new().name("vox-cache-io".into()).spawn(move || worker_loop(worker)) {
            tracing::error!(target: "data", "could not start the audio cache worker: {e}; working inline");
            shared.queue.lock().inline = true;
        }
        let guard = Arc::new(WorkerGuard { shared: shared.clone() });
        AudioCache { shared, _guard: guard }
    }

    fn enqueue(&self, job: Job) {
        let mut q = self.shared.queue.lock();
        if q.inline {
            drop(q);
            self.shared.run_job(job);
            return;
        }
        q.jobs.push_back(job);
        self.shared.work_cv.notify_all();
    }

    /// Queues a save of a completed read's chunks (valid index, non-empty audio; de-duplicated by index, last wins;
    /// sorted); replaces any existing entry. On any I/O error the folder is deleted and `done(Err)` called. The id
    /// enters the index only when the save completed; a save discarded by a deletion or clear queued meanwhile also
    /// reports `Err` (see the module docs). `Ok` means the entry is indexed (or there was nothing valid to save).
    pub fn save_async(&self, read_id: String, chunks: Vec<CachedChunk>, done: SaveDone) {
        self.enqueue(Job::Save { read_id, chunks, done });
    }

    /// Queues a load (sorted by stored index) that also touches `lastAccess`; a broken entry is deleted → `None`.
    pub fn load_async(&self, read_id: String, done: LoadDone) {
        self.enqueue(Job::Load { read_id, done });
    }

    /// Blocking [`AudioCache::save_async`] (tests and tools; never on the core or UI thread).
    pub fn save(&self, read_id: &str, chunks: &[CachedChunk]) -> Result<(), StoreError> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.save_async(
            read_id.to_string(),
            chunks.to_vec(),
            Box::new(move |r| {
                let _ = tx.send(r);
            }),
        );
        rx.recv().unwrap_or_else(|_| Err(StoreError::Other("the audio cache worker stopped".into())))
    }

    /// Blocking [`AudioCache::load_async`] (tests and tools; never on the core or UI thread).
    pub fn load(&self, read_id: &str) -> Option<Vec<CachedChunk>> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.load_async(
            read_id.to_string(),
            Box::new(move |r| {
                let _ = tx.send(r);
            }),
        );
        rx.recv().ok().flatten()
    }

    /// The index has an entry (index-only, fast).
    pub fn has(&self, read_id: &str) -> bool {
        is_valid_read_id(read_id) && self.shared.index.lock().entries.contains_key(read_id)
    }

    /// Some alignment char of the entry is non-empty (index-only).
    pub fn has_text(&self, read_id: &str) -> bool {
        is_valid_read_id(read_id) && self.shared.index.lock().entries.get(read_id).is_some_and(|e| e.has_text)
    }

    /// Queues blanking every alignment char of an entry (preview-only records); the index's `hasText` is cleared at
    /// once. Never fails loudly.
    pub fn redact_text(&self, read_id: &str) {
        if !is_valid_read_id(read_id) {
            return;
        }
        {
            let mut idx = self.shared.index.lock();
            match idx.entries.get_mut(read_id) {
                Some(e) if e.has_text => e.has_text = false,
                _ => return,
            }
        }
        self.enqueue(Job::Redact { read_id: read_id.to_string() });
    }

    /// Drops the index entry now and queues the folder deletion (after any queued save of the same id). Never fails
    /// loudly.
    pub fn remove(&self, read_id: &str) {
        if !is_valid_read_id(read_id) {
            return;
        }
        self.shared.index.lock().evict(read_id);
        self.enqueue(Job::Remove { read_id: read_id.to_string() });
    }

    /// Empties the index now and queues deleting every entry and anything else inside `audio-cache\`; returns the
    /// stats afterwards (zero). Fast: safe on the core thread.
    pub fn clear(&self) -> CacheStats {
        let (ids, stats) = {
            let mut idx = self.shared.index.lock();
            let ids = idx.ordered_ids();
            idx.entries.clear();
            idx.pending_clears += 1;
            (ids, idx.stats())
        };
        self.enqueue(Job::Clear { ids });
        stats
    }

    /// Totals from the index.
    pub fn stats(&self) -> CacheStats {
        self.shared.index.lock().stats()
    }

    /// Evicts least-recently-used entries from the index until the total is ≤ `max_bytes`, never touching `protect`,
    /// and queues their deletion; returns the evicted ids (the caller clears their records' `cached` flag).
    pub fn prune(&self, max_bytes: u64, protect: &[String]) -> Vec<String> {
        let evicted = {
            let mut idx = self.shared.index.lock();
            let mut total = idx.total_bytes();
            if total <= max_bytes {
                return Vec::new();
            }
            let keep: HashSet<&str> = protect.iter().map(String::as_str).collect();
            let mut candidates: Vec<(String, Entry)> =
                idx.entries.iter().filter(|(id, _)| !keep.contains(id.as_str())).map(|(id, e)| (id.clone(), *e)).collect();
            candidates.sort_by(|a, b| {
                a.1.last_access.partial_cmp(&b.1.last_access).unwrap_or(std::cmp::Ordering::Equal).then(a.1.seq.cmp(&b.1.seq))
            });
            let mut evicted = Vec::new();
            for (id, entry) in candidates {
                if total <= max_bytes {
                    break;
                }
                idx.evict(&id);
                total = total.saturating_sub(entry.bytes);
                evicted.push(id);
            }
            evicted
        };
        for id in &evicted {
            self.enqueue(Job::Remove { read_id: id.clone() });
        }
        evicted
    }

    /// Removes entries whose id is not in `keep`; returns them. Candidates = the index ids **at call time** minus
    /// `keep`, computed under the index lock before any deletion is queued; ids saved later are never touched. vox-app
    /// runs it on the core thread during core startup, before handling any queued `Cli`, bridge or selection command
    /// (index-only and fast thanks to the I/O worker). Test: save a new id after the snapshot → the entry survives.
    pub fn remove_orphans(&self, keep: &HashSet<String>) -> Vec<String> {
        let orphans = {
            let mut idx = self.shared.index.lock();
            let orphans: Vec<String> = idx.ordered_ids().into_iter().filter(|id| !keep.contains(id)).collect();
            for id in &orphans {
                idx.evict(id);
            }
            orphans
        };
        for id in &orphans {
            self.enqueue(Job::Remove { read_id: id.clone() });
        }
        orphans
    }

    /// Every cached read id.
    pub fn ids(&self) -> Vec<String> {
        self.shared.index.lock().ordered_ids()
    }

    /// Waits up to `timeout` for the I/O queue to drain; `false` when it did not (quit sequence).
    pub fn flush(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut q = self.shared.queue.lock();
        while !q.jobs.is_empty() || q.busy {
            if self.shared.idle_cv.wait_until(&mut q, deadline).timed_out() {
                return q.jobs.is_empty() && !q.busy;
            }
        }
        true
    }
}

fn worker_loop(shared: Arc<Shared>) {
    loop {
        let job = {
            let mut q = shared.queue.lock();
            loop {
                if let Some(job) = q.jobs.pop_front() {
                    q.busy = true;
                    break job;
                }
                if q.shutdown {
                    return;
                }
                shared.work_cv.wait(&mut q);
            }
        };
        shared.run_job(job);
        let mut q = shared.queue.lock();
        q.busy = false;
        if q.jobs.is_empty() {
            shared.idle_cv.notify_all();
        }
    }
}

fn guarded<T>(what: &str, f: impl FnOnce() -> T) -> Option<T> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => Some(v),
        Err(_) => {
            tracing::error!(target: "data", "audio cache {what} panicked");
            None
        }
    }
}

/// What the startup scan learned about a valid entry.
struct Scanned {
    bytes: u64,
    files: u64,
    last_access: f64,
    has_text: bool,
}

/// Validates an entry on disk and measures it, or `None` when it is incomplete (persistence.md §6.5 `scanEntry`).
fn scan_entry(dir: &Path, id: &str, now: f64) -> Option<Scanned> {
    let manifest_path = dir.join(MANIFEST);
    let m = match read_json_file(&manifest_path) {
        JsonRead::Ok(v) => parse_manifest(&v, id, now)?,
        _ => return None,
    };
    let mut bytes = fs::metadata(&manifest_path).ok()?.len();
    for c in &m.chunks {
        let md = fs::metadata(dir.join(&c.file)).ok()?;
        if !md.is_file() || md.len() == 0 {
            return None;
        }
        bytes += md.len();
    }
    Some(Scanned { bytes, files: m.chunks.len() as u64, last_access: m.last_access, has_text: m.has_text() })
}

/// Runs `work` on up to `threads` threads: the caller plus scoped helpers (each `work` pulls items from a shared
/// cursor until none are left). A helper that cannot be spawned is skipped; the caller always works, so nothing is
/// left undone. `work` must not panic (callers wrap each item in [`guarded`]).
fn run_parallel(threads: usize, work: &(dyn Fn() + Sync)) {
    std::thread::scope(|s| {
        for _ in 1..threads {
            if std::thread::Builder::new().name("vox-cache-pool".into()).spawn_scoped(s, work).is_err() {
                break;
            }
        }
        work();
    });
}

/// The startup scan with up to [`SCAN_CONCURRENCY`] threads pulling names from a shared cursor: invalid folder
/// names and incomplete entries are deleted, valid entries returned.
fn scan(root: &Path, names: &[std::ffi::OsString]) -> Vec<(String, Scanned)> {
    let cursor = AtomicUsize::new(0);
    let found = Mutex::new(Vec::new());
    let now = now_ms();
    run_parallel(SCAN_CONCURRENCY.min(names.len()), &|| loop {
        let i = cursor.fetch_add(1, Ordering::SeqCst);
        let Some(name) = names.get(i) else { break };
        let dir = root.join(name);
        let _ = guarded("scan", || match name.to_str().filter(|n| is_valid_read_id(n)) {
            None => {
                remove_dir_all_retry(&dir);
            }
            Some(id) => match scan_entry(&dir, id, now) {
                Some(entry) => found.lock().push((id.to_string(), entry)),
                None => {
                    remove_dir_all_retry(&dir);
                }
            },
        });
    });
    found.into_inner()
}

impl Shared {
    fn dir(&self, read_id: &str) -> PathBuf {
        self.root.join(read_id)
    }

    fn run_job(&self, job: Job) {
        match job {
            Job::Save { read_id, chunks, done } => {
                let result = guarded("save", || self.do_save(&read_id, &chunks))
                    .unwrap_or_else(|| Err(StoreError::Other("audio cache save failed unexpectedly".into())));
                let _ = guarded("save callback", || done(result));
            }
            Job::Load { read_id, done } => {
                let result = guarded("load", || self.do_load(&read_id)).flatten();
                let _ = guarded("load callback", || done(result));
            }
            Job::Redact { read_id } => {
                let _ = guarded("redact", || self.do_redact(&read_id));
            }
            Job::Remove { read_id } => {
                let ok = guarded("remove", || remove_dir_all_retry(&self.dir(&read_id))).unwrap_or(false);
                if !ok {
                    tracing::warn!(target: "data", "cache remove failed for {read_id}");
                }
                self.index.lock().removal_done(&read_id);
            }
            Job::Clear { ids } => {
                let _ = guarded("clear", || self.do_clear(&ids));
                let mut idx = self.index.lock();
                idx.pending_clears = idx.pending_clears.saturating_sub(1);
            }
        }
    }

    /// Drops the index entry and deletes the folder (inline on the worker).
    fn drop_entry(&self, read_id: &str) {
        self.index.lock().entries.remove(read_id);
        remove_dir_all_retry(&self.dir(read_id));
    }

    fn do_save(&self, read_id: &str, chunks: &[CachedChunk]) -> Result<(), StoreError> {
        if !is_valid_read_id(read_id) {
            tracing::warn!(target: "data", "audioCache.save() ignored an invalid readId: {read_id}");
            return Ok(());
        }
        // De-duplicate by index (last wins), keep non-empty audio, sort by index.
        let mut by_index: BTreeMap<u32, &CachedChunk> = BTreeMap::new();
        for c in chunks.iter().filter(|c| !c.audio.is_empty()) {
            by_index.insert(c.index, c);
        }
        if by_index.is_empty() {
            return Ok(());
        }
        let dir = self.dir(read_id);
        self.index.lock().entries.remove(read_id);
        // A failed delete only warns: the save carries on and overwrites.
        remove_dir_all_retry(&dir);
        let written = (|| -> Result<(u64, CacheManifest), StoreError> {
            fs::create_dir_all(&dir)?;
            let mut audio_bytes = 0u64;
            let mut manifest_chunks = Vec::with_capacity(by_index.len());
            for c in by_index.values() {
                let file = format!("{}.{}", c.index, ext_for_mime(&c.mime));
                fs::write(dir.join(&file), &*c.audio)?;
                let len = c.audio.len() as u64;
                audio_bytes += len;
                manifest_chunks.push(ManifestChunk {
                    index: c.index,
                    text_start: c.text_start,
                    text_end: c.text_end,
                    mime: c.mime.clone(),
                    alignment: c.alignment.clone(),
                    duration_sec: c.duration_sec,
                    file,
                    bytes: len,
                });
            }
            let now = now_ms();
            let manifest =
                CacheManifest { read_id: read_id.to_string(), created_at: now, last_access: now, chunks: manifest_chunks };
            let json = manifest.to_json();
            // Written last: a missing manifest marks an interrupted save.
            write_file_atomic(&dir.join(MANIFEST), json.as_bytes(), false)?;
            Ok((audio_bytes + json.len() as u64, manifest))
        })();
        match written {
            Ok((bytes, manifest)) => {
                let mut idx = self.index.lock();
                if idx.blocked(read_id) {
                    // A remove/prune/clear of this id was queued while the save waited or ran: the entry is not
                    // indexed and that queued deletion (FIFO, after this job) removes the folder. Report it as a
                    // failure so the caller never marks the record `cached` (persistence.md §5.7, §6.6).
                    return Err(StoreError::Other(DISCARDED_SAVE.into()));
                }
                idx.put(read_id, bytes, manifest.chunks.len() as u64, manifest.last_access, manifest.has_text());
                Ok(())
            }
            Err(e) => {
                remove_dir_all_retry(&dir);
                Err(e)
            }
        }
    }

    fn read_manifest(&self, read_id: &str) -> Option<CacheManifest> {
        match read_json_file(&self.dir(read_id).join(MANIFEST)) {
            JsonRead::Ok(v) => parse_manifest(&v, read_id, now_ms()),
            _ => None,
        }
    }

    fn do_load(&self, read_id: &str) -> Option<Vec<CachedChunk>> {
        if !is_valid_read_id(read_id) {
            return None;
        }
        let Some(manifest) = self.read_manifest(read_id) else {
            // Absent, or an interrupted save: nothing usable remains.
            self.drop_entry(read_id);
            return None;
        };
        let dir = self.dir(read_id);
        let read = (|| -> io::Result<Vec<CachedChunk>> {
            let mut out = Vec::with_capacity(manifest.chunks.len());
            for c in &manifest.chunks {
                let audio = retry_busy(|| fs::read(dir.join(&c.file)))?;
                if audio.is_empty() {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, format!("empty chunk {}", c.file)));
                }
                out.push(CachedChunk {
                    index: c.index,
                    text_start: c.text_start,
                    text_end: c.text_end,
                    mime: c.mime.clone(),
                    alignment: c.alignment.clone(),
                    duration_sec: c.duration_sec,
                    audio: AudioBytes::from(audio),
                });
            }
            Ok(out)
        })();
        match read {
            Ok(mut chunks) => {
                chunks.sort_by_key(|c| c.index);
                let now = now_ms();
                let has_text = manifest.has_text();
                let touched = CacheManifest { last_access: now, ..manifest };
                let json = touched.to_json();
                if let Err(e) = write_file_atomic(&dir.join(MANIFEST), json.as_bytes(), false) {
                    tracing::warn!(target: "data", "could not touch cache manifest: {e}");
                }
                let bytes = saturating_sum(chunks.iter().map(|c| c.audio.len() as u64)).saturating_add(json.len() as u64);
                let mut idx = self.index.lock();
                if !idx.blocked(read_id) {
                    idx.put(read_id, bytes, chunks.len() as u64, now, has_text);
                }
                Some(chunks)
            }
            Err(e) => {
                if e.kind() != io::ErrorKind::NotFound {
                    tracing::warn!(target: "data", "dropping unreadable cache entry {read_id}: {e}");
                }
                self.drop_entry(read_id);
                None
            }
        }
    }

    fn do_redact(&self, read_id: &str) {
        let Some(mut manifest) = self.read_manifest(read_id) else { return };
        for c in &mut manifest.chunks {
            if let Some(a) = &c.alignment {
                c.alignment = Some(a.redacted());
            }
        }
        let json = manifest.to_json();
        match write_file_atomic(&self.dir(read_id).join(MANIFEST), json.as_bytes(), false) {
            Ok(()) => {
                let mut idx = self.index.lock();
                if let Some(e) = idx.entries.get_mut(read_id) {
                    // Manifest `bytes` may be saturated at u64::MAX (D-P-15): never overflow.
                    e.bytes = saturating_sum(manifest.chunks.iter().map(|c| c.bytes)).saturating_add(json.len() as u64);
                    e.has_text = false;
                }
            }
            Err(e) => tracing::warn!(target: "data", "could not redact cache manifest {read_id}: {e}"),
        }
    }

    fn do_clear(&self, ids: &[String]) {
        let mut targets: Vec<PathBuf> = ids.iter().map(|id| self.dir(id)).collect();
        match fs::read_dir(&self.root) {
            Ok(rd) => targets.extend(rd.flatten().map(|e| e.path())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(target: "data", "could not list the audio cache: {e}"),
        }
        let mut seen = HashSet::new();
        targets.retain(|p| seen.insert(p.clone()));
        let cursor = AtomicUsize::new(0);
        run_parallel(SCAN_CONCURRENCY.min(targets.len()), &|| loop {
            let i = cursor.fetch_add(1, Ordering::SeqCst);
            let Some(path) = targets.get(i) else { break };
            let _ = guarded("clear", || remove_dir_all_retry(path));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ID_A: &str = "02d84816-59fd-4cf1-8f73-86cd58eff9d3";
    const ID_B: &str = "7c134be0-e9c6-4e9e-a122-0a916f3827ff";
    const ID_C: &str = "1157c6bc-ea84-475a-b04b-d46941f67a8d";

    fn setup() -> (tempfile::TempDir, Profile) {
        let dir = tempfile::tempdir().unwrap();
        let p = Profile::at(dir.path(), false);
        (dir, p)
    }

    fn chunk(index: u32, audio: &[u8], with_text: bool) -> CachedChunk {
        let alignment = Alignment {
            chars: if with_text { vec!["H".into(), "i".into()] } else { vec![String::new(), String::new()] },
            starts: vec![0.0, 0.2],
            ends: vec![0.2, 0.4],
        };
        CachedChunk {
            index,
            text_start: index * 10,
            text_end: index * 10 + 2,
            mime: "audio/mpeg".into(),
            alignment: Some(alignment),
            duration_sec: 1.5,
            audio: AudioBytes::from(audio.to_vec()),
        }
    }

    fn flush(c: &AudioCache) {
        assert!(c.flush(Duration::from_secs(10)));
    }

    /// Blocks the I/O worker until the returned sender is dropped or sent to.
    fn block_worker(c: &AudioCache) -> crossbeam_channel::Sender<()> {
        let (tx, rx) = crossbeam_channel::bounded::<()>(0);
        let (started_tx, started_rx) = crossbeam_channel::bounded::<()>(1);
        c.load_async(
            "blocker".into(),
            Box::new(move |_| {
                let _ = started_tx.send(());
                let _ = rx.recv();
            }),
        );
        started_rx.recv().unwrap();
        tx
    }

    #[test]
    fn chunk_file_names() {
        for ok in ["0.mp3", "123456.wav", "7.ogg", "42.bin"] {
            assert!(is_valid_chunk_file(ok), "{ok}");
        }
        for bad in ["1234567.mp3", ".mp3", "a.mp3", "1.MP3", "1.mp4", "1.mp3.tmp", "manifest.json", "1", "-1.mp3", "１.mp3"] {
            assert!(!is_valid_chunk_file(bad), "{bad}");
        }
    }

    #[test]
    fn read_ids() {
        assert!(is_valid_read_id(ID_A));
        assert!(is_valid_read_id("a"));
        assert!(is_valid_read_id(&"a".repeat(128)));
        for bad in ["", "-a", "_a", "a b", "..", "a/b", "a\\b", "é", &"a".repeat(129)] {
            assert!(!is_valid_read_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn manifest_json_field_order() {
        let m = CacheManifest {
            read_id: ID_A.into(),
            created_at: 1791700000000.0,
            last_access: 1791700000000.0,
            chunks: vec![ManifestChunk {
                index: 0,
                text_start: 0,
                text_end: 412,
                mime: "audio/mpeg".into(),
                alignment: Some(Alignment { chars: vec!["H".into()], starts: vec![0.0], ends: vec![f64::NAN] }),
                duration_sec: 27.4,
                file: "0.mp3".into(),
                bytes: 231593,
            }],
        };
        assert_eq!(
            m.to_json(),
            r#"{"version":1,"readId":"02d84816-59fd-4cf1-8f73-86cd58eff9d3","createdAt":1791700000000,"lastAccess":1791700000000,"chunks":[{"index":0,"textStart":0,"textEnd":412,"mime":"audio/mpeg","alignment":{"chars":["H"],"starts":[0],"ends":[null]},"durationSec":27.4,"file":"0.mp3","bytes":231593}]}"#
        );
        let no_alignment = CacheManifest {
            chunks: vec![ManifestChunk { alignment: None, ..m.chunks[0].clone() }],
            ..m.clone()
        };
        assert!(no_alignment.to_json().contains(r#""alignment":null"#));
        // Round trip.
        let v: Value = serde_json::from_str(&no_alignment.to_json()).unwrap();
        assert_eq!(parse_manifest(&v, ID_A, 0.0).unwrap(), no_alignment);
    }

    #[test]
    fn parse_manifest_validation() {
        let base = || {
            json!({"version":1,"readId":ID_A,"createdAt":5,"lastAccess":6,"chunks":[
                {"index":0,"textStart":0,"textEnd":3,"mime":"audio/mpeg","alignment":null,"durationSec":1,"file":"0.mp3","bytes":10}
            ]})
        };
        assert!(parse_manifest(&base(), ID_A, 0.0).is_some());
        assert!(parse_manifest(&base(), ID_B, 0.0).is_none(), "readId must equal the folder");
        assert!(parse_manifest(&base(), &ID_A.to_uppercase(), 0.0).is_none(), "case-sensitive");
        let mut v = base();
        v["chunks"] = json!([]);
        assert!(parse_manifest(&v, ID_A, 0.0).is_none(), "empty chunks");
        let mut v = base();
        v["chunks"] = json!({});
        assert!(parse_manifest(&v, ID_A, 0.0).is_none());
        let mut v = base();
        v["chunks"][0]["file"] = json!("../x.mp3");
        assert!(parse_manifest(&v, ID_A, 0.0).is_none(), "bad file");
        let mut v = base();
        v["chunks"][0]["mime"] = json!(5);
        assert!(parse_manifest(&v, ID_A, 0.0).is_none(), "non-string mime");
        let mut v = base();
        v["chunks"] = json!([5]);
        assert!(parse_manifest(&v, ID_A, 0.0).is_none(), "non-object chunk");
        assert!(parse_manifest(&json!([1]), ID_A, 0.0).is_none());
        assert!(parse_manifest(&json!(null), ID_A, 0.0).is_none());
        // createdAt → now, lastAccess → createdAt.
        let mut v = base();
        v.as_object_mut().unwrap().remove("createdAt");
        v["lastAccess"] = json!("soon");
        let m = parse_manifest(&v, ID_A, 99.0).unwrap();
        assert_eq!((m.created_at, m.last_access), (99.0, 99.0));
        // version is not checked.
        let mut v = base();
        v["version"] = json!(7);
        assert!(parse_manifest(&v, ID_A, 0.0).is_some());
    }

    #[test]
    fn parse_manifest_normalises_odd_values() {
        let v = json!({"readId":ID_A,"createdAt":1,"lastAccess":2,"chunks":[
            {"index":1.5,"textStart":-3,"textEnd":"x","mime":"audio/mpeg","durationSec":"long","file":"2.mp3","bytes":-7,
             "alignment":{"chars":["a",7,null],"starts":[0,null,"x"],"ends":[0.1,0.2,0.3]}},
            {"index":4294967296.0,"textStart":1e300,"textEnd":2,"mime":"audio/wav","file":"3.wav","bytes":12.4,
             "alignment":{"chars":["a"],"starts":[0,1],"ends":[1]}}
        ]});
        let m = parse_manifest(&v, ID_A, 0.0).unwrap();
        let c = &m.chunks[0];
        assert_eq!((c.index, c.text_start, c.text_end, c.bytes, c.duration_sec), (2, 0, 0, 0, 0.0));
        let a = c.alignment.as_ref().unwrap();
        assert_eq!(a.chars, vec!["a", "", ""]);
        assert_eq!(a.starts[0], 0.0);
        assert!(a.starts[1].is_nan() && a.starts[2].is_nan());
        assert!(!a.usable_for_timing());
        let c = &m.chunks[1];
        assert_eq!((c.index, c.text_start, c.bytes), (u32::MAX, u32::MAX, 12));
        assert_eq!(c.alignment, None, "unequal lengths");
        // The rewrite writes the normalised values (NaN → null).
        assert!(m.to_json().contains(r#""starts":[0,null,null]"#));
    }

    #[test]
    fn save_load_touch_round_trip() {
        let (_d, p) = setup();
        let c = AudioCache::open(&p);
        assert_eq!(c.stats(), CacheStats::default());
        let chunks = vec![chunk(1, b"second", true), chunk(0, b"first!", true)];
        c.save(ID_A, &chunks).unwrap();
        assert!(c.has(ID_A));
        assert!(c.has_text(ID_A));
        let dir = p.audio_cache_dir().join(ID_A);
        assert_eq!(fs::read(dir.join("0.mp3")).unwrap(), b"first!");
        assert_eq!(fs::read(dir.join("1.mp3")).unwrap(), b"second");
        let manifest_text = fs::read_to_string(dir.join(MANIFEST)).unwrap();
        let stats = c.stats();
        assert_eq!((stats.reads, stats.files), (1, 2));
        assert_eq!(stats.bytes, 12 + manifest_text.len() as u64);
        let saved = parse_manifest(&serde_json::from_str(&manifest_text).unwrap(), ID_A, 0.0).unwrap();
        assert_eq!(saved.created_at, saved.last_access);

        std::thread::sleep(Duration::from_millis(5));
        let loaded = c.load(ID_A).unwrap();
        assert_eq!(loaded.iter().map(|c| c.index).collect::<Vec<_>>(), vec![0, 1]);
        assert_eq!(&*loaded[0].audio, b"first!");
        assert_eq!(loaded[1].alignment, chunks[0].alignment);
        assert_eq!((loaded[1].text_start, loaded[1].text_end, loaded[1].duration_sec), (10, 12, 1.5));
        let touched =
            parse_manifest(&serde_json::from_str(&fs::read_to_string(dir.join(MANIFEST)).unwrap()).unwrap(), ID_A, 0.0)
                .unwrap();
        assert!(touched.last_access > saved.last_access, "load touches lastAccess");
        assert_eq!(touched.created_at, saved.created_at);
        assert_eq!(c.ids(), vec![ID_A.to_string()]);
        // Reopen: the scan indexes the same entry.
        let again = AudioCache::open(&p);
        assert_eq!(again.stats(), c.stats());
        assert!(again.has_text(ID_A));
    }

    #[test]
    fn save_dedupes_filters_and_replaces() {
        let (_d, p) = setup();
        let c = AudioCache::open(&p);
        let mut wav = chunk(0, b"wav", false);
        wav.mime = "audio/wav".into();
        c.save(ID_A, &[chunk(0, b"old", true), chunk(1, b"", true), wav, chunk(3, b"three", false)]).unwrap();
        let dir = p.audio_cache_dir().join(ID_A);
        assert!(dir.join("0.wav").exists() && dir.join("3.mp3").exists());
        assert!(!dir.join("0.mp3").exists() && !dir.join("1.mp3").exists());
        assert!(!c.has_text(ID_A), "only redacted alignments survived");
        // Replacing deletes the old files.
        c.save(ID_A, &[chunk(5, b"five", true)]).unwrap();
        assert!(dir.join("5.mp3").exists());
        assert!(!dir.join("0.wav").exists() && !dir.join("3.mp3").exists());
        assert_eq!(c.stats().files, 1);
        // Nothing valid: no disk access and the existing entry stays.
        c.save(ID_A, &[chunk(0, b"", true)]).unwrap();
        assert!(c.has(ID_A) && dir.join("5.mp3").exists());
        // An invalid id is ignored (success).
        c.save("../escape", &[chunk(0, b"x", true)]).unwrap();
        assert!(!p.root().join("escape").exists());
        assert_eq!(c.stats().reads, 1);
    }

    #[test]
    fn save_failure_reports_and_leaves_no_entry() {
        let (_d, p) = setup();
        let c = AudioCache::open(&p);
        // The cache root is replaced by a file: the entry folder cannot be created.
        fs::remove_dir_all(p.audio_cache_dir()).unwrap();
        fs::write(p.audio_cache_dir(), b"not a folder").unwrap();
        assert!(c.save(ID_A, &[chunk(0, b"x", true)]).is_err());
        assert!(!c.has(ID_A));
    }

    #[test]
    fn load_misses_delete_broken_entries() {
        let (_d, p) = setup();
        let c = AudioCache::open(&p);
        assert!(c.load(ID_A).is_none(), "absent");
        assert!(c.load("not valid!").is_none());
        c.save(ID_B, &[chunk(0, b"a", true), chunk(1, b"b", true)]).unwrap();
        fs::remove_file(p.audio_cache_dir().join(ID_B).join("1.mp3")).unwrap();
        assert!(c.load(ID_B).is_none());
        assert!(!c.has(ID_B));
        assert!(!p.audio_cache_dir().join(ID_B).exists());
        // An empty chunk file is broken too.
        c.save(ID_C, &[chunk(0, b"a", true)]).unwrap();
        fs::write(p.audio_cache_dir().join(ID_C).join("0.mp3"), b"").unwrap();
        assert!(c.load(ID_C).is_none());
        assert!(!p.audio_cache_dir().join(ID_C).exists());
    }

    #[test]
    fn load_indexes_an_entry_the_scan_did_not_see() {
        let (_d, p) = setup();
        let c = AudioCache::open(&p);
        let other = AudioCache::open(&p);
        other.save(ID_A, &[chunk(0, b"abc", true)]).unwrap();
        assert!(!c.has(ID_A));
        assert!(c.load(ID_A).is_some());
        assert!(c.has(ID_A));
    }

    #[test]
    fn async_callbacks_run_on_the_worker_in_fifo_order() {
        let (_d, p) = setup();
        let c = AudioCache::open(&p);
        let order = Arc::new(Mutex::new(Vec::<String>::new()));
        let push = |o: &Arc<Mutex<Vec<String>>>, what: &str| {
            let (o, what) = (o.clone(), what.to_string());
            move |n: usize| o.lock().push(format!("{what} {n}"))
        };
        // Saved and then removed before the worker ran either job.
        let release = block_worker(&c);
        let f = push(&order, "save ok");
        c.save_async(ID_A.into(), vec![chunk(0, b"x", true)], Box::new(move |r| f(usize::from(r.is_ok()))));
        let f = push(&order, "load");
        c.load_async(ID_A.into(), Box::new(move |r| f(r.map_or(0, |v| v.len()))));
        c.remove(ID_A);
        let f = push(&order, "load");
        c.load_async(ID_A.into(), Box::new(move |r| f(r.map_or(0, |v| v.len()))));
        drop(release);
        flush(&c);
        // The save ran first (its files are there for the next load), but the remove queued meanwhile discards it.
        assert_eq!(*order.lock(), vec!["save ok 0", "load 1", "load 0"]);
        assert!(!c.has(ID_A));
        // Without a deletion queued meanwhile the save reports Ok.
        let f = push(&order, "save ok");
        c.save_async(ID_B.into(), vec![chunk(0, b"y", true)], Box::new(move |r| f(usize::from(r.is_ok()))));
        flush(&c);
        assert_eq!(order.lock().last().map(String::as_str), Some("save ok 1"));
        assert!(c.has(ID_B));
    }

    #[test]
    fn remove_after_a_queued_save_leaves_no_entry() {
        let (_d, p) = setup();
        let c = AudioCache::open(&p);
        let release = block_worker(&c);
        c.save_async(ID_A.into(), vec![chunk(0, b"x", true)], Box::new(|_| {}));
        c.remove(ID_A);
        assert!(!c.has(ID_A));
        drop(release);
        flush(&c);
        assert!(!c.has(ID_A));
        assert!(!p.audio_cache_dir().join(ID_A).exists());
        // And remove-then-save keeps the new entry.
        c.remove(ID_B);
        c.save(ID_B, &[chunk(0, b"y", true)]).unwrap();
        flush(&c);
        assert!(c.has(ID_B) && p.audio_cache_dir().join(ID_B).join("0.mp3").exists());
    }

    #[test]
    fn flush_times_out_while_the_worker_is_busy() {
        let (_d, p) = setup();
        let c = AudioCache::open(&p);
        let release = block_worker(&c);
        assert!(!c.flush(Duration::from_millis(30)));
        drop(release);
        assert!(c.flush(Duration::from_secs(5)));
    }

    #[test]
    fn redact_blanks_chars_and_keeps_timing() {
        let (_d, p) = setup();
        let c = AudioCache::open(&p);
        c.save(ID_A, &[chunk(0, b"abc", true)]).unwrap();
        let release = block_worker(&c);
        c.redact_text(ID_A);
        assert!(!c.has_text(ID_A), "cleared at once");
        drop(release);
        flush(&c);
        let text = fs::read_to_string(p.audio_cache_dir().join(ID_A).join(MANIFEST)).unwrap();
        assert!(text.contains(r#""chars":["",""],"starts":[0,0.2],"ends":[0.2,0.4]"#), "{text}");
        assert_eq!(c.stats().bytes, 3 + text.len() as u64);
        let loaded = c.load(ID_A).unwrap();
        assert!(!loaded[0].alignment.as_ref().unwrap().has_text());
        // Without text there is nothing to do.
        c.redact_text(ID_B);
        c.redact_text(ID_A);
        flush(&c);
    }

    #[test]
    fn prune_evicts_least_recently_used_and_respects_protect() {
        let (_d, p) = setup();
        let c = AudioCache::open(&p);
        for id in [ID_A, ID_B, ID_C] {
            c.save(id, &[chunk(0, &[1u8; 1000], true)]).unwrap();
            std::thread::sleep(Duration::from_millis(3));
        }
        // Touch A: now B is the least recently used.
        c.load(ID_A).unwrap();
        let each = c.stats().bytes / 3;
        assert_eq!(c.prune(u64::MAX, &[]), Vec::<String>::new());
        assert_eq!(c.prune(c.stats().bytes, &[]), Vec::<String>::new(), "at the limit: nothing");
        let evicted = c.prune(2 * each + 10, &[ID_B.to_string()]);
        assert_eq!(evicted, vec![ID_C.to_string()], "B is protected, C is the next oldest");
        assert!(!c.has(ID_C));
        let evicted = c.prune(0, &[ID_A.to_string()]);
        assert_eq!(evicted, vec![ID_B.to_string()]);
        assert_eq!(c.ids(), vec![ID_A.to_string()], "protected entries stay even over the limit");
        flush(&c);
        assert!(!p.audio_cache_dir().join(ID_B).exists() && !p.audio_cache_dir().join(ID_C).exists());
    }

    #[test]
    fn remove_orphans_uses_a_snapshot_at_call_time() {
        let (_d, p) = setup();
        let c = AudioCache::open(&p);
        c.save(ID_A, &[chunk(0, b"a", true)]).unwrap();
        c.save(ID_B, &[chunk(0, b"b", true)]).unwrap();
        let release = block_worker(&c);
        // Saved "after the snapshot": queued now, indexed only when the worker gets to it.
        c.save_async(ID_C.into(), vec![chunk(0, b"c", true)], Box::new(|_| {}));
        let keep: HashSet<String> = [ID_A.to_string()].into_iter().collect();
        assert_eq!(c.remove_orphans(&keep), vec![ID_B.to_string()]);
        drop(release);
        flush(&c);
        assert!(c.has(ID_A) && c.has(ID_C) && !c.has(ID_B));
        assert!(!p.audio_cache_dir().join(ID_B).exists());
    }

    #[test]
    fn clear_empties_the_index_now_and_the_folder_later() {
        let (_d, p) = setup();
        let c = AudioCache::open(&p);
        c.save(ID_A, &[chunk(0, b"a", true)]).unwrap();
        c.save(ID_B, &[chunk(0, b"b", true)]).unwrap();
        fs::write(p.audio_cache_dir().join("stray.txt"), b"x").unwrap();
        fs::create_dir(p.audio_cache_dir().join("Not An Id")).unwrap();
        let release = block_worker(&c);
        // A save queued before the clear never resurfaces in the index.
        c.save_async(ID_C.into(), vec![chunk(0, b"c", true)], Box::new(|_| {}));
        assert_eq!(c.clear(), CacheStats::default());
        assert_eq!(c.stats(), CacheStats::default());
        drop(release);
        flush(&c);
        assert_eq!(c.stats(), CacheStats::default());
        assert!(p.audio_cache_dir().is_dir());
        assert_eq!(fs::read_dir(p.audio_cache_dir()).unwrap().count(), 0);
        // A save after the clear works.
        c.save(ID_A, &[chunk(0, b"a", true)]).unwrap();
        assert!(c.has(ID_A));
    }

    /// Queues `save_async(id)` and returns a receiver for its result.
    fn save_result(c: &AudioCache, id: &str) -> crossbeam_channel::Receiver<Result<(), StoreError>> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        c.save_async(
            id.into(),
            vec![chunk(0, b"x", true)],
            Box::new(move |r| {
                let _ = tx.send(r);
            }),
        );
        rx
    }

    #[test]
    fn a_save_discarded_by_a_queued_clear_reports_err() {
        let (_d, p) = setup();
        let c = AudioCache::open(&p);
        let release = block_worker(&c);
        let result = save_result(&c, ID_A);
        c.clear();
        drop(release);
        flush(&c);
        // `Ok` would make the core mark the record `cached` although the entry is gone.
        let r = result.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(matches!(&r, Err(StoreError::Other(m)) if m == DISCARDED_SAVE), "{r:?}");
        assert!(!c.has(ID_A));
        assert!(!p.audio_cache_dir().join(ID_A).exists());
    }

    #[test]
    fn a_save_discarded_by_a_queued_remove_reports_err() {
        let (_d, p) = setup();
        let c = AudioCache::open(&p);
        let release = block_worker(&c);
        let discarded = save_result(&c, ID_A);
        c.remove(ID_A);
        // A save queued after the remove of its id is kept and reports Ok.
        c.remove(ID_B);
        let kept = save_result(&c, ID_B);
        drop(release);
        flush(&c);
        assert!(discarded.recv_timeout(Duration::from_secs(10)).unwrap().is_err());
        assert!(!c.has(ID_A) && !p.audio_cache_dir().join(ID_A).exists());
        assert!(kept.recv_timeout(Duration::from_secs(10)).unwrap().is_ok());
        assert!(c.has(ID_B) && p.audio_cache_dir().join(ID_B).join("0.mp3").exists());
    }

    #[test]
    fn huge_manifest_byte_counts_saturate_instead_of_overflowing() {
        let (_d, p) = setup();
        {
            let c = AudioCache::open(&p);
            c.save(ID_A, &[chunk(0, b"abc", true)]).unwrap();
            flush(&c);
        }
        // A manifest whose chunk `bytes` normalises to u64::MAX (D-P-15); the scan measures the real files.
        let manifest = p.audio_cache_dir().join(ID_A).join(MANIFEST);
        let text = fs::read_to_string(&manifest).unwrap().replace(r#""bytes":3"#, r#""bytes":1e20"#);
        assert!(text.contains(r#""bytes":1e20"#));
        fs::write(&manifest, text).unwrap();
        let c = AudioCache::open(&p);
        assert!(c.has_text(ID_A));
        // Redact sums the manifest's `bytes` (persistence.md §6.8).
        c.redact_text(ID_A);
        flush(&c);
        assert_eq!(c.stats().bytes, u64::MAX);
        // Totals over several entries saturate too.
        c.save(ID_B, &[chunk(0, b"b", true)]).unwrap();
        assert_eq!(c.stats(), CacheStats { reads: 2, files: 2, bytes: u64::MAX });
        assert_eq!(c.prune(1_000_000, &[ID_B.to_string()]), vec![ID_A.to_string()]);
        assert!(c.stats().bytes < 1_000);
    }

    #[test]
    fn startup_scan_repairs_the_folder() {
        let (_d, p) = setup();
        let root = p.audio_cache_dir();
        {
            let c = AudioCache::open(&p);
            c.save(ID_A, &[chunk(0, b"good", true)]).unwrap();
            c.save(ID_B, &[chunk(0, b"b", true), chunk(1, b"bb", true)]).unwrap();
            c.save(ID_C, &[chunk(0, b"c", true)]).unwrap();
            flush(&c);
        }
        let good_manifest = fs::metadata(root.join(ID_A).join(MANIFEST)).unwrap().len();
        // Stray files inside a good entry are neither counted nor deleted.
        fs::write(root.join(ID_A).join("manifest.json.tmp"), b"junk").unwrap();
        fs::write(root.join(ID_A).join("9.mp3"), b"unlisted").unwrap();
        // Broken entries.
        fs::remove_file(root.join(ID_B).join("1.mp3")).unwrap();
        fs::write(root.join(ID_C).join(MANIFEST), b"{ corrupt").unwrap();
        fs::create_dir(root.join("no-manifest")).unwrap();
        fs::write(root.join("no-manifest").join("0.mp3"), b"x").unwrap();
        fs::create_dir(root.join("zero-chunk")).unwrap();
        fs::write(root.join("zero-chunk").join("0.mp3"), b"").unwrap();
        fs::write(
            root.join("zero-chunk").join(MANIFEST),
            r#"{"readId":"zero-chunk","chunks":[{"file":"0.mp3","mime":"audio/mpeg"}]}"#,
        )
        .unwrap();
        fs::create_dir(root.join("bad name")).unwrap();
        fs::write(root.join("plain.txt"), b"left alone").unwrap();

        let c = AudioCache::open(&p);
        assert_eq!(c.ids(), vec![ID_A.to_string()]);
        let stats = c.stats();
        assert_eq!((stats.reads, stats.files, stats.bytes), (1, 1, good_manifest + 4));
        for gone in [ID_B, ID_C, "no-manifest", "zero-chunk", "bad name"] {
            assert!(!root.join(gone).exists(), "{gone}");
        }
        assert!(root.join("plain.txt").exists());
        assert!(root.join(ID_A).join("manifest.json.tmp").exists());
        assert!(root.join(ID_A).join("9.mp3").exists());
    }

    #[test]
    fn open_creates_the_folder_and_survives_a_file_in_its_place() {
        let (_d, p) = setup();
        let c = AudioCache::open(&p);
        assert!(p.audio_cache_dir().is_dir());
        drop(c);
        let (_d2, p2) = setup();
        fs::write(p2.audio_cache_dir(), b"file").unwrap();
        let c = AudioCache::open(&p2);
        assert_eq!(c.stats(), CacheStats::default());
        assert!(c.load(ID_A).is_none());
    }

    #[test]
    fn a_panicking_callback_does_not_kill_the_worker() {
        let (_d, p) = setup();
        let c = AudioCache::open(&p);
        c.save_async(ID_A.into(), vec![chunk(0, b"x", true)], Box::new(|_| panic!("callback bug")));
        flush(&c);
        assert!(c.has(ID_A));
        assert!(c.load(ID_A).is_some());
    }
}
