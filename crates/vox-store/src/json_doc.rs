//! `JsonFile<T>`: a debounced durable document (persistence.md §2.5).
//!
//! `schedule()` marks dirty and (re)arms one timer to `min(debounce, first + max_wait − now)`; the write runs on the
//! document's writer thread, takes the **latest** snapshot at write time, and is serialized (never overlapping).
//! Failures mark dirty again and retry after `retry_base × failures` (≤ `max_retries` retries: 6 attempts in all with
//! the spec values 1 s / 5; no retry after the last; a later `schedule()` makes exactly one more attempt). `flush()`
//! writes now and waits; `flush_sync()` writes synchronously via `.tmp-sync` even if an async write is in flight (then
//! schedules one more async write after it). Two concurrent `flush_sync()` calls (the core's session end and the
//! system watcher's 2.5 s fallback) are serialised by a per-document lock; both are idempotent. Settings: 250 ms / 2 s,
//! pretty. History: 2.5 s / 10 s, compact. Warnings go to the log with scope `data` (`could not save <path> (attempt
//! N)`, …) instead of the console (persistence.md §2.5 note).
//!
//! Implementation notes:
//! - One state mutex + condvar per document. The writer thread never holds the state lock while it calls the
//!   snapshot closure or touches the disk, so a store may call [`JsonDoc::schedule`] while holding its own lock
//!   (the snapshot closure takes that store lock; no lock-order inversion is possible).
//! - "Write runs" are numbered: `flush()` appends a run and waits until the writer has finished it (and every run
//!   before it); a debounce or retry timer firing appends a run too.
//! - Dropping the last handle stops the writer thread **without** writing pending changes (the app always flushes at
//!   quit and session end; tests flush explicitly).
//! - If the writer thread cannot be spawned, the document works inline: `schedule()` only marks it dirty and
//!   `flush()` writes on the caller's thread.
//! - No async write **starts** while a `flush_sync()` is between its snapshot and its rename (`sync_active`): in
//!   Electron `flushSync` runs on the only JS thread, so no `write()` can begin meanwhile; here a timer or `flush()`
//!   on another thread could otherwise snapshot newer state, commit it, and then be overwritten by the older sync
//!   snapshot with nothing left dirty (persistence.md §2.3: the newest state must win). A run requested meanwhile
//!   waits and then writes the newest state right after the sync write. An async write that had already started is
//!   the `rewrite_after_in_flight` case of §2.5.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex, MutexGuard};

use crate::fsutil::{write_file_atomic, write_file_atomic_sync};

/// Produces the document text at write time (`None` = nothing to write / serialization failed).
pub type Snapshot = Arc<dyn Fn() -> Option<String> + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsonDocOptions {
    pub debounce: Duration,
    pub max_wait: Duration,
    /// Rewrite `<file>.bak` after every commit.
    pub backup: bool,
    /// Retry delay unit after a failed write (`retry_base × failures`; 1 s in production, ms in tests).
    pub retry_base: Duration,
    /// Retries after the first failed attempt (`MAX_WRITE_RETRIES` = 5 in production).
    pub max_retries: u32,
}

impl JsonDocOptions {
    pub fn settings() -> Self {
        Self {
            debounce: Duration::from_millis(vox_core::constants::SETTINGS_DEBOUNCE_MS),
            max_wait: Duration::from_millis(vox_core::constants::SETTINGS_MAX_WAIT_MS),
            backup: true,
            retry_base: Duration::from_millis(1_000),
            max_retries: vox_core::constants::MAX_WRITE_RETRIES,
        }
    }
    pub fn history() -> Self {
        Self {
            debounce: Duration::from_millis(vox_core::constants::HISTORY_DEBOUNCE_MS),
            max_wait: Duration::from_millis(vox_core::constants::HISTORY_MAX_WAIT_MS),
            backup: true,
            retry_base: Duration::from_millis(1_000),
            max_retries: vox_core::constants::MAX_WRITE_RETRIES,
        }
    }
}

#[derive(Debug, Default)]
struct DocState {
    dirty: bool,
    /// Consecutive failed writes (reset only by a successful write).
    failures: u32,
    /// The one timer slot (debounce or retry).
    timer: Option<Instant>,
    first_scheduled: Option<Instant>,
    /// An async write has taken a snapshot that is not on disk yet.
    in_flight: bool,
    rewrite_after_in_flight: bool,
    /// A `flush_sync` is writing: async runs wait before taking their snapshot.
    sync_active: bool,
    runs_requested: u64,
    runs_done: u64,
    /// Write attempts made by the async writer (tests).
    attempts: u64,
    shutdown: bool,
    /// No writer thread: work inline.
    inline: bool,
}

struct DocInner {
    path: PathBuf,
    opts: JsonDocOptions,
    snapshot: Snapshot,
    state: Mutex<DocState>,
    cv: Condvar,
    /// Serialises `flush_sync` calls.
    sync_lock: Mutex<()>,
}

/// Stops the writer thread when the last [`JsonDoc`] handle is dropped.
struct HandleGuard {
    inner: Arc<DocInner>,
}

impl Drop for HandleGuard {
    fn drop(&mut self) {
        let mut st = self.inner.state.lock();
        st.shutdown = true;
        self.inner.cv.notify_all();
    }
}

/// Handle to one debounced document (cheap to clone; one writer thread per document).
#[derive(Clone)]
pub struct JsonDoc {
    path: PathBuf,
    inner: Arc<DocInner>,
    _guard: Arc<HandleGuard>,
}

impl JsonDoc {
    /// Spawns the writer thread for `path`.
    pub fn new(path: PathBuf, opts: JsonDocOptions, snapshot: Snapshot) -> JsonDoc {
        let inner = Arc::new(DocInner {
            path: path.clone(),
            opts,
            snapshot,
            state: Mutex::new(DocState::default()),
            cv: Condvar::new(),
            sync_lock: Mutex::new(()),
        });
        let worker = inner.clone();
        let spawned = std::thread::Builder::new().name("vox-json-doc".into()).spawn(move || writer_loop(worker));
        if let Err(e) = spawned {
            tracing::error!(target: "data", "could not start the writer for {}: {e}; writing inline", path.display());
            inner.state.lock().inline = true;
        }
        let guard = Arc::new(HandleGuard { inner: inner.clone() });
        JsonDoc { path, inner, _guard: guard }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Marks dirty and arms the debounce timer.
    pub fn schedule(&self) {
        let mut st = self.inner.state.lock();
        schedule_locked(&self.inner, &mut st);
    }

    /// Cancels the timer, writes now if dirty, waits for any in-flight write. Never fails.
    pub fn flush(&self) {
        let mut st = self.inner.state.lock();
        st.timer = None;
        if st.inline {
            run_once(&self.inner, &mut st);
            return;
        }
        st.runs_requested += 1;
        let ticket = st.runs_requested;
        self.inner.cv.notify_all();
        while st.runs_done < ticket && !st.inline {
            self.inner.cv.wait(&mut st);
        }
    }

    /// Session-end write (see module docs). Never fails.
    pub fn flush_sync(&self) {
        let inner = &self.inner;
        let _serial = inner.sync_lock.lock();
        let (was_dirty, was_in_flight) = {
            let mut st = inner.state.lock();
            st.timer = None;
            if !st.dirty && !st.in_flight {
                return;
            }
            let flags = (st.dirty, st.in_flight);
            st.dirty = false;
            st.sync_active = true;
            flags
        };
        // Lets waiting async runs start again on every path out of here.
        let _active = SyncActive { inner };
        let text = call_snapshot(inner);
        let Some(text) = text else {
            tracing::warn!(target: "data", "could not serialise {}", inner.path.display());
            inner.state.lock().dirty |= was_dirty;
            return;
        };
        let result = write_file_atomic_sync(&inner.path, text.as_bytes(), inner.opts.backup);
        let mut st = inner.state.lock();
        match result {
            Ok(()) => {
                st.failures = 0;
                if st.in_flight {
                    // That older async write may still rename its older snapshot over ours.
                    st.rewrite_after_in_flight = true;
                } else if was_in_flight {
                    // It finished while we were writing (possibly after our rename): write the newest state again.
                    schedule_locked(inner, &mut st);
                }
            }
            Err(e) => {
                st.dirty |= was_dirty;
                tracing::warn!(target: "data", "could not save {} synchronously: {e}", inner.path.display());
            }
        }
    }

    /// Deletes `<file>.corrupt`.
    pub fn discard_corrupt_copy(&self) {
        crate::fsutil::discard_corrupt_copy(&self.path)
    }

    /// Unsaved changes are pending (`JsonFile.pending`; tests).
    #[cfg(test)]
    pub(crate) fn is_dirty(&self) -> bool {
        self.inner.state.lock().dirty
    }

    /// Write attempts the async writer has made so far (tests).
    #[cfg(test)]
    pub(crate) fn write_attempts(&self) -> u64 {
        self.inner.state.lock().attempts
    }
}

/// Clears [`DocState::sync_active`] and wakes runs waiting for it.
struct SyncActive<'a> {
    inner: &'a DocInner,
}

impl Drop for SyncActive<'_> {
    fn drop(&mut self) {
        self.inner.state.lock().sync_active = false;
        self.inner.cv.notify_all();
    }
}

fn schedule_locked(inner: &DocInner, st: &mut MutexGuard<'_, DocState>) {
    st.dirty = true;
    if st.inline {
        return;
    }
    let now = Instant::now();
    let first = match (st.timer, st.first_scheduled) {
        // A pending timer (debounce or retry) keeps its first-scheduled time.
        (Some(_), Some(first)) => first,
        _ => now,
    };
    st.first_scheduled = Some(first);
    let max_wait = inner.opts.max_wait.max(inner.opts.debounce);
    let by_max_wait = (first + max_wait).saturating_duration_since(now);
    st.timer = Some(now + inner.opts.debounce.min(by_max_wait));
    inner.cv.notify_all();
}

fn call_snapshot(inner: &DocInner) -> Option<String> {
    catch_unwind(AssertUnwindSafe(|| (inner.snapshot)())).unwrap_or_else(|_| {
        tracing::error!(target: "data", "snapshot of {} panicked", inner.path.display());
        None
    })
}

/// One write run (`write()` of the spec). Called with the state lock held; releases it around the snapshot and the
/// disk write.
fn run_once(inner: &DocInner, st: &mut MutexGuard<'_, DocState>) {
    // Never snapshot while a sync write is in progress (module docs).
    while st.sync_active {
        inner.cv.wait(st);
    }
    if !st.dirty {
        return;
    }
    st.dirty = false;
    st.in_flight = true;
    st.attempts += 1;
    let attempt_failed = MutexGuard::unlocked(st, || {
        let Some(text) = call_snapshot(inner) else {
            // Serialisation error: the change is not retried until the next schedule().
            tracing::warn!(target: "data", "could not serialise {}", inner.path.display());
            return None;
        };
        Some(write_file_atomic(&inner.path, text.as_bytes(), inner.opts.backup).err())
    });
    match attempt_failed {
        Some(None) => st.failures = 0,
        Some(Some(e)) => {
            st.dirty = true;
            st.failures += 1;
            tracing::warn!(target: "data", "could not save {} (attempt {}): {e}", inner.path.display(), st.failures);
            if st.failures <= inner.opts.max_retries && st.timer.is_none() && !st.inline {
                st.timer = Some(Instant::now() + inner.opts.retry_base * st.failures);
            }
        }
        None => {}
    }
    st.in_flight = false;
    if st.rewrite_after_in_flight {
        st.rewrite_after_in_flight = false;
        schedule_locked(inner, st);
    }
}

fn writer_loop(inner: Arc<DocInner>) {
    let mut st = inner.state.lock();
    loop {
        if st.runs_done < st.runs_requested {
            run_once(&inner, &mut st);
            st.runs_done += 1;
            inner.cv.notify_all();
            continue;
        }
        if st.shutdown {
            return;
        }
        match st.timer {
            Some(deadline) => {
                if Instant::now() >= deadline {
                    st.timer = None;
                    st.runs_requested += 1;
                } else {
                    inner.cv.wait_until(&mut st, deadline);
                }
            }
            None => inner.cv.wait(&mut st),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    fn opts(debounce_ms: u64, max_wait_ms: u64) -> JsonDocOptions {
        JsonDocOptions {
            debounce: Duration::from_millis(debounce_ms),
            max_wait: Duration::from_millis(max_wait_ms),
            backup: true,
            retry_base: Duration::from_millis(5),
            max_retries: 5,
        }
    }

    /// A document whose snapshot is `{"n":<counter>}`.
    fn counter_doc(path: PathBuf, o: JsonDocOptions) -> (JsonDoc, Arc<AtomicU64>) {
        let n = Arc::new(AtomicU64::new(0));
        let n2 = n.clone();
        let doc = JsonDoc::new(path, o, Arc::new(move || Some(format!("{{\"n\":{}}}", n2.load(Ordering::SeqCst)))));
        (doc, n)
    }

    fn wait_until(mut f: impl FnMut() -> bool, ms: u64) -> bool {
        let end = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < end {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        f()
    }

    #[test]
    fn the_debounce_timer_writes_by_itself() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.json");
        let (doc, n) = counter_doc(p.clone(), opts(40, 400));
        n.store(1, Ordering::SeqCst);
        doc.schedule();
        doc.schedule();
        assert!(wait_until(|| doc.write_attempts() >= 1 && !doc.inner.state.lock().in_flight, 10_000));
        assert_eq!(fs::read_to_string(&p).unwrap(), "{\"n\":1}");
        assert_eq!(fs::read_to_string(crate::fsutil::with_suffix(&p, ".bak")).unwrap(), "{\"n\":1}");
        assert!(!doc.is_dirty());
    }

    #[test]
    fn writes_take_the_snapshot_at_write_time() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.json");
        // A long debounce: nothing is written until the flush, which serialises the state current at that time.
        let (doc, n) = counter_doc(p.clone(), opts(10_000, 60_000));
        n.store(1, Ordering::SeqCst);
        doc.schedule();
        std::thread::sleep(Duration::from_millis(30));
        assert!(!p.exists(), "nothing is written before the debounce");
        n.store(2, Ordering::SeqCst);
        doc.flush();
        assert_eq!(fs::read_to_string(&p).unwrap(), "{\"n\":2}");
        assert_eq!(doc.write_attempts(), 1);
    }

    #[test]
    fn max_wait_bounds_continuous_changes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.json");
        let (doc, n) = counter_doc(p.clone(), opts(400, 600));
        let start = Instant::now();
        // Keep scheduling every 20 ms (well below the debounce): only the max wait can force a write.
        while doc.write_attempts() == 0 && start.elapsed() < Duration::from_millis(5_000) {
            n.fetch_add(1, Ordering::SeqCst);
            doc.schedule();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(doc.write_attempts() >= 1);
        assert!(start.elapsed() < Duration::from_millis(3_000), "written within the max wait (+ generous slack)");
    }

    #[test]
    fn flush_writes_now_and_waits() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.json");
        let (doc, n) = counter_doc(p.clone(), opts(10_000, 60_000));
        doc.flush(); // not dirty: nothing written
        assert!(!p.exists());
        n.store(7, Ordering::SeqCst);
        doc.schedule();
        doc.flush();
        assert_eq!(fs::read_to_string(&p).unwrap(), "{\"n\":7}");
        assert_eq!(doc.write_attempts(), 1);
        // The cancelled debounce timer does not write again.
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(doc.write_attempts(), 1);
    }

    #[test]
    fn flush_sync_writes_via_tmp_sync() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.json");
        let (doc, n) = counter_doc(p.clone(), opts(10_000, 60_000));
        doc.flush_sync(); // clean and nothing in flight: no write
        assert!(!p.exists());
        n.store(3, Ordering::SeqCst);
        doc.schedule();
        doc.flush_sync();
        assert_eq!(fs::read_to_string(&p).unwrap(), "{\"n\":3}");
        assert!(!crate::fsutil::with_suffix(&p, ".tmp-sync").exists());
        assert!(!doc.is_dirty());
        assert_eq!(doc.write_attempts(), 0, "the async writer did nothing");
        // Idempotent.
        doc.flush_sync();
        assert_eq!(doc.write_attempts(), 0);
    }

    #[test]
    fn concurrent_flush_sync_calls_are_serialised() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.json");
        let (doc, n) = counter_doc(p.clone(), opts(10_000, 60_000));
        n.store(9, Ordering::SeqCst);
        doc.schedule();
        let a = {
            let d = doc.clone();
            std::thread::spawn(move || d.flush_sync())
        };
        let b = {
            let d = doc.clone();
            std::thread::spawn(move || d.flush_sync())
        };
        a.join().unwrap();
        b.join().unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "{\"n\":9}");
    }

    #[test]
    fn flush_sync_during_an_async_write_reschedules_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.json");
        let n = Arc::new(AtomicU64::new(1));
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let entered = Arc::new(AtomicU64::new(0));
        let (n2, gate2, entered2) = (n.clone(), gate.clone(), entered.clone());
        // The first async snapshot blocks until released, simulating a slow in-flight write.
        let doc = JsonDoc::new(
            p.clone(),
            opts(1, 10),
            Arc::new(move || {
                let value = n2.load(Ordering::SeqCst);
                if entered2.fetch_add(1, Ordering::SeqCst) == 0 {
                    let (m, cv) = &*gate2;
                    let mut open = m.lock();
                    while !*open {
                        cv.wait(&mut open);
                    }
                }
                Some(format!("{{\"n\":{value}}}"))
            }),
        );
        doc.schedule();
        assert!(wait_until(|| entered.load(Ordering::SeqCst) == 1, 10_000));
        // The async write holds snapshot n=1; newer state arrives and a log-off flush writes it synchronously.
        n.store(2, Ordering::SeqCst);
        doc.flush_sync();
        assert_eq!(fs::read_to_string(&p).unwrap(), "{\"n\":2}");
        // Release the old async write: it lands n=1 over the newer file, then the newest state is rewritten.
        {
            let (m, cv) = &*gate;
            *m.lock() = true;
            cv.notify_all();
        }
        assert!(wait_until(|| doc.write_attempts() >= 2, 10_000));
        doc.flush();
        assert_eq!(fs::read_to_string(&p).unwrap(), "{\"n\":2}");
    }

    #[test]
    fn no_async_write_lands_between_the_sync_snapshot_and_its_rename() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.json");
        let n = Arc::new(AtomicU64::new(1));
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let block_next = Arc::new(AtomicBool::new(false));
        let entered = Arc::new(AtomicU64::new(0));
        let (n2, gate2, block2, entered2) = (n.clone(), gate.clone(), block_next.clone(), entered.clone());
        // When armed, the next snapshot blocks until released (after reading the state).
        let doc = JsonDoc::new(
            p.clone(),
            opts(10_000, 60_000),
            Arc::new(move || {
                let value = n2.load(Ordering::SeqCst);
                if block2.swap(false, Ordering::SeqCst) {
                    entered2.fetch_add(1, Ordering::SeqCst);
                    let (m, cv) = &*gate2;
                    let mut open = m.lock();
                    while !*open {
                        cv.wait(&mut open);
                    }
                }
                Some(format!("{{\"n\":{value}}}"))
            }),
        );
        doc.schedule();
        block_next.store(true, Ordering::SeqCst);
        // A log-off flush takes its snapshot (n=1) and is held before its rename.
        let sync = {
            let d = doc.clone();
            std::thread::spawn(move || d.flush_sync())
        };
        assert!(wait_until(|| entered.load(Ordering::SeqCst) == 1, 10_000));
        // Newer state, and an async write requested on another thread meanwhile.
        n.store(2, Ordering::SeqCst);
        doc.schedule();
        let async_flush = {
            let d = doc.clone();
            std::thread::spawn(move || d.flush())
        };
        // It must not commit {"n":2} now: the sync rename would then put the older n=1 back with nothing dirty.
        assert!(!wait_until(|| async_flush.is_finished(), 300), "an async write started during a sync write");
        assert!(!p.exists());
        {
            let (m, cv) = &*gate;
            *m.lock() = true;
            cv.notify_all();
        }
        sync.join().unwrap();
        async_flush.join().unwrap();
        // The waiting async run wrote the newest state right after the sync write.
        assert_eq!(fs::read_to_string(&p).unwrap(), "{\"n\":2}");
        assert!(!doc.is_dirty());
        assert_eq!(doc.write_attempts(), 1);
    }

    #[test]
    fn retry_schedule_makes_exactly_six_attempts_then_one_per_schedule() {
        let dir = tempfile::tempdir().unwrap();
        // The parent is a file: every write fails at once (not a busy error).
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, b"x").unwrap();
        let p = blocker.join("d.json");
        let (doc, _n) = counter_doc(p, opts(1, 10));
        doc.schedule();
        // Retries after 5, 10, 15, 20, 25 ms.
        assert!(wait_until(|| doc.write_attempts() >= 6, 10_000));
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(doc.write_attempts(), 6, "no retry after the sixth consecutive failure");
        assert!(doc.is_dirty());
        doc.schedule();
        assert!(wait_until(|| doc.write_attempts() >= 7, 10_000));
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(doc.write_attempts(), 7, "a later schedule() makes exactly one more attempt");
        doc.flush();
        assert_eq!(doc.write_attempts(), 8);
    }

    #[test]
    fn failures_reset_after_a_success() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        fs::write(&sub, b"x").unwrap(); // a file where the folder should be
        let p = sub.join("d.json");
        let (doc, _n) = counter_doc(p.clone(), opts(1, 10));
        doc.schedule();
        assert!(wait_until(|| doc.write_attempts() >= 6, 10_000));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(doc.inner.state.lock().failures, 6);
        // Make the target writable: the next write succeeds and resets the failure count.
        fs::remove_file(&sub).unwrap();
        doc.flush();
        assert!(p.exists());
        assert!(!doc.is_dirty());
        assert_eq!(doc.inner.state.lock().failures, 0);
    }

    #[test]
    fn serialisation_failure_is_not_retried() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.json");
        let doc = JsonDoc::new(p.clone(), opts(1, 10), Arc::new(|| None));
        doc.schedule();
        doc.flush();
        assert!(!p.exists());
        assert!(!doc.is_dirty());
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(doc.write_attempts(), 1);
    }

    #[test]
    fn a_panicking_snapshot_does_not_kill_the_writer() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.json");
        let calls = Arc::new(AtomicU64::new(0));
        let c2 = calls.clone();
        let doc = JsonDoc::new(
            p.clone(),
            opts(1, 10),
            Arc::new(move || {
                if c2.fetch_add(1, Ordering::SeqCst) == 0 {
                    panic!("boom");
                }
                Some("{}".into())
            }),
        );
        doc.schedule();
        doc.flush();
        doc.schedule();
        doc.flush();
        assert_eq!(fs::read_to_string(&p).unwrap(), "{}");
    }

    #[test]
    fn dropping_the_last_handle_stops_the_writer() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.json");
        let (doc, _n) = counter_doc(p, opts(10_000, 60_000));
        let weak = Arc::downgrade(&doc.inner);
        doc.schedule();
        drop(doc);
        assert!(wait_until(|| weak.upgrade().is_none(), 10_000), "the writer thread exited and released the state");
    }
}
