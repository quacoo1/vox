//! Durable file primitives (persistence.md §2.1–§2.4).
//!
//! - Sharing violations (`ERROR_ACCESS_DENIED` 5, `ERROR_SHARING_VIOLATION` 32, `ERROR_LOCK_VIOLATION` 33) are retried
//!   after 15, 40, 90, 180, 350 ms (6 attempts) on: opening the temp file, rename, fallback copy, `.bak` write, reads.
//! - Atomic write: `<file>.tmp` → write → `FlushFileBuffers` → close → `MoveFileExW(REPLACE_EXISTING)`; if the rename
//!   still fails, copy over the target in place and delete the temp. With `backup`, then rewrite `<file>.bak` with the
//!   same bytes (non-atomic). Async writes to one path are serialized (a process-wide per-path lock).
//! - Sync variant (log-off): `<file>.tmp-sync`, no retries, and it does **not** take the per-path lock (persistence.md
//!   §2.3: it runs immediately even while an async write of the same file is half-way through its busy retries); it
//!   can never share a temp file with the async writer; `.bak` writes of both may race (harmless).
//! - Reads: `ENOENT` → missing; any other error, empty/whitespace, invalid JSON → corrupt; BOM stripped; lone
//!   surrogate escapes repaired (`vox_core::json::parse_lenient`).
//!
//! Every warning goes to the log with scope `data` (persistence.md §2.5 note; ARCHITECTURE.md §8 D-P-13).

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::Value;
use vox_core::constants::FILE_RETRY_DELAYS_MS;

/// Outcome of reading one JSON file.
#[derive(Debug)]
pub enum JsonRead {
    Ok(Value),
    Missing,
    /// Unreadable or invalid (message for the log).
    Corrupt(String),
}

/// Outcome of loading a document with `.bak` / `.corrupt` handling.
#[derive(Debug, Default)]
pub struct LoadOutcome {
    /// The parsed value (main file, or `.bak` after a corrupt main file); `None` = use defaults.
    pub value: Option<Value>,
    /// The value came from `<file>.bak`.
    pub from_backup: bool,
    /// The main file was corrupt (it was copied to `<file>.corrupt`).
    pub main_was_corrupt: bool,
}

/// `<path><suffix>` (e.g. `settings.json` + `.tmp`), without touching the extension logic of `Path`.
pub(crate) fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s: OsString = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// Runs `op`, retrying sharing violations with the spec's delays.
pub fn retry_busy<T>(mut op: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    retry_busy_with(&FILE_RETRY_DELAYS_MS, &mut op)
}

/// [`retry_busy`] with explicit delays (tests use short ones).
pub(crate) fn retry_busy_with<T>(delays_ms: &[u64], op: &mut impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let mut attempt = 0;
    loop {
        match op() {
            Ok(v) => return Ok(v),
            Err(e) if is_busy_error(&e) && attempt < delays_ms.len() => {
                std::thread::sleep(Duration::from_millis(delays_ms[attempt]));
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// True for the Windows "file busy" errors that are retried.
pub fn is_busy_error(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(5) | Some(32) | Some(33))
}

/// Process-wide per-path locks for the async atomic writer (persistence.md §2.2: two writers to one path never run
/// concurrently, they would share `<file>.tmp`). Entries are dropped once nobody holds or waits for them.
static PATH_LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn with_path_lock<T>(path: &Path, f: impl FnOnce() -> T) -> T {
    let lock = {
        let mut map = PATH_LOCKS.lock();
        map.entry(path.to_path_buf()).or_insert_with(|| Arc::new(Mutex::new(()))).clone()
    };
    let out = {
        let _held = lock.lock();
        f()
    };
    let mut map = PATH_LOCKS.lock();
    // Two references left (the map's and ours) = nobody else holds or waits for this path.
    if Arc::strong_count(&lock) == 2 {
        map.remove(path);
    }
    out
}

fn write_temp(tmp: &Path, data: &[u8], retry: bool) -> io::Result<()> {
    let mut file = if retry { retry_busy(|| File::create(tmp))? } else { File::create(tmp)? };
    file.write_all(data)?;
    // `sync_all` is `FlushFileBuffers` on Windows.
    file.sync_all()?;
    Ok(())
}

fn ensure_parent(path: &Path) -> io::Result<()> {
    match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => fs::create_dir_all(dir),
        _ => Ok(()),
    }
}

fn atomic_write_inner(path: &Path, data: &[u8], backup: bool, tmp_suffix: &str, retry: bool) -> io::Result<()> {
    ensure_parent(path)?;
    let tmp = with_suffix(path, tmp_suffix);
    write_temp(&tmp, data, retry)?;
    let renamed = if retry { retry_busy(|| fs::rename(&tmp, path)) } else { fs::rename(&tmp, path) };
    if let Err(e) = renamed {
        // Some scanners open files without FILE_SHARE_DELETE, which blocks replacing them: copy in place instead.
        tracing::warn!(target: "data", "atomic rename failed for {}, copying in place: {e}", path.display());
        if retry {
            retry_busy(|| fs::copy(&tmp, path))?;
        } else {
            fs::copy(&tmp, path)?;
        }
        let _ = fs::remove_file(&tmp);
    }
    if backup {
        let bak = with_suffix(path, ".bak");
        let written = if retry { retry_busy(|| fs::write(&bak, data)) } else { fs::write(&bak, data) };
        if let Err(e) = written {
            tracing::warn!(target: "data", "could not update {}: {e}", bak.display());
        }
    }
    Ok(())
}

/// `writeFileAtomic(path, data, {backup})`.
pub fn write_file_atomic(path: &Path, data: &[u8], backup: bool) -> io::Result<()> {
    with_path_lock(path, || atomic_write_inner(path, data, backup, ".tmp", true))
}

/// `writeFileAtomicSync` (session end): `<file>.tmp-sync`, no retries, no per-path lock (module docs); a `.bak` failure
/// only warns.
pub fn write_file_atomic_sync(path: &Path, data: &[u8], backup: bool) -> io::Result<()> {
    atomic_write_inner(path, data, backup, ".tmp-sync", false)
}

/// Lossy UTF-8 → lenient JSON (BOM, empty file, lone surrogates, out-of-range numbers).
pub(crate) fn parse_json_bytes(bytes: &[u8]) -> Result<Value, String> {
    let text = String::from_utf8_lossy(bytes);
    vox_core::json::parse_lenient(&text).map_err(|e| e.to_string())
}

/// `readJsonFile`. Valid JSON of any shape (`null`, `[]`, `42`) is `Ok` — no `.corrupt` copy (the history store
/// applies its own shape check, D-P-16).
pub fn read_json_file(path: &Path) -> JsonRead {
    match retry_busy(|| fs::read(path)) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => JsonRead::Missing,
        Err(e) => JsonRead::Corrupt(e.to_string()),
        Ok(bytes) => match parse_json_bytes(&bytes) {
            Ok(v) => JsonRead::Ok(v),
            Err(msg) => JsonRead::Corrupt(msg),
        },
    }
}

/// `JsonFile.load()`: main ok → it; missing → defaults (**no** `.bak` fallback: deleting is a reset); corrupt → copy
/// to `<file>.corrupt` (best effort), then try `<file>.bak`.
pub fn load_with_backup(path: &Path) -> LoadOutcome {
    load_with_backup_checked(path, |_| true)
}

/// [`load_with_backup`] where a parsed value that fails `shape_ok` counts as corrupt (`unexpected shape`), for the
/// main file and for `.bak` alike (history, ARCHITECTURE.md §8 D-P-16).
pub(crate) fn load_with_backup_checked(path: &Path, shape_ok: impl Fn(&Value) -> bool) -> LoadOutcome {
    let read = |p: &Path| match read_json_file(p) {
        JsonRead::Ok(v) if !shape_ok(&v) => JsonRead::Corrupt("unexpected shape".into()),
        other => other,
    };
    match read(path) {
        JsonRead::Ok(v) => LoadOutcome { value: Some(v), from_backup: false, main_was_corrupt: false },
        JsonRead::Missing => LoadOutcome::default(),
        JsonRead::Corrupt(err) => {
            tracing::warn!(target: "data", "{} is unreadable, trying backup: {err}", path.display());
            // Plain copy, overwrites an older `.corrupt`, no retry, failure ignored.
            let _ = fs::copy(path, with_suffix(path, ".corrupt"));
            let bak = with_suffix(path, ".bak");
            match read(&bak) {
                JsonRead::Ok(v) => {
                    tracing::warn!(target: "data", "restored {} from backup", path.display());
                    LoadOutcome { value: Some(v), from_backup: true, main_was_corrupt: true }
                }
                _ => LoadOutcome { value: None, from_backup: false, main_was_corrupt: true },
            }
        }
    }
}

/// Deletes `<file>.corrupt` (ignores not-found).
pub fn discard_corrupt_copy(path: &Path) {
    let corrupt = with_suffix(path, ".corrupt");
    if let Err(e) = fs::remove_file(&corrupt) {
        if e.kind() != io::ErrorKind::NotFound {
            tracing::warn!(target: "data", "could not delete {}: {e}", corrupt.display());
        }
    }
}

/// Errors Node's `rm` retries (`EBUSY`, `EMFILE`, `ENFILE`, `ENOTEMPTY`, `EPERM`).
fn is_remove_retryable(e: &io::Error) -> bool {
    is_busy_error(e)
        || matches!(e.raw_os_error(), Some(4) | Some(145))
        || matches!(e.kind(), io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::PermissionDenied)
}

fn remove_once(path: &Path) -> io::Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let ft = meta.file_type();
    let r = if ft.is_symlink() {
        // A directory symlink / junction is removed with RemoveDirectory, a file symlink with DeleteFile; never follow.
        fs::remove_dir(path).or_else(|_| fs::remove_file(path))
    } else if ft.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    match r {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Recursive delete with up to 4 retries (60 ms × attempt); returns false (and logs) on final failure.
pub fn remove_dir_all_retry(path: &Path) -> bool {
    remove_dir_all_retry_with(path, 60)
}

pub(crate) fn remove_dir_all_retry_with(path: &Path, step_ms: u64) -> bool {
    let mut attempt: u64 = 0;
    loop {
        match remove_once(path) {
            Ok(()) => return true,
            Err(e) if is_remove_retryable(&e) && attempt < 4 => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(step_ms * attempt));
            }
            Err(e) => {
                tracing::warn!(target: "data", "could not delete {}: {e}", path.display());
                return false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn busy() -> io::Error {
        io::Error::from_raw_os_error(32)
    }

    #[test]
    fn busy_errors() {
        assert!(is_busy_error(&io::Error::from_raw_os_error(5)));
        assert!(is_busy_error(&io::Error::from_raw_os_error(32)));
        assert!(is_busy_error(&io::Error::from_raw_os_error(33)));
        assert!(!is_busy_error(&io::Error::from_raw_os_error(2)));
        assert!(!is_busy_error(&io::Error::other("x")));
    }

    #[test]
    fn retry_busy_retries_sharing_violations_then_succeeds() {
        let calls = Cell::new(0);
        let r = retry_busy_with(&[1, 1, 1, 1, 1], &mut || {
            calls.set(calls.get() + 1);
            if calls.get() < 4 {
                Err(busy())
            } else {
                Ok(42)
            }
        });
        assert_eq!(r.unwrap(), 42);
        assert_eq!(calls.get(), 4);
    }

    #[test]
    fn retry_busy_gives_up_after_six_attempts() {
        let calls = Cell::new(0);
        let r: io::Result<()> = retry_busy_with(&[1, 1, 1, 1, 1], &mut || {
            calls.set(calls.get() + 1);
            Err(busy())
        });
        assert!(r.is_err());
        assert_eq!(calls.get(), 6);
    }

    #[test]
    fn retry_busy_does_not_retry_other_errors() {
        let calls = Cell::new(0);
        let r: io::Result<()> = retry_busy(|| {
            calls.set(calls.get() + 1);
            Err(io::Error::new(io::ErrorKind::NotFound, "gone"))
        });
        assert!(r.is_err());
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn atomic_write_with_backup_mirror() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub").join("doc.json");
        write_file_atomic(&p, b"{\"a\":1}", true).unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"{\"a\":1}");
        assert_eq!(fs::read(with_suffix(&p, ".bak")).unwrap(), b"{\"a\":1}");
        assert!(!with_suffix(&p, ".tmp").exists());
        write_file_atomic(&p, b"{\"a\":2}", true).unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"{\"a\":2}");
        assert_eq!(fs::read(with_suffix(&p, ".bak")).unwrap(), b"{\"a\":2}");
        // Without backup the .bak keeps the last committed mirror.
        write_file_atomic(&p, b"{\"a\":3}", false).unwrap();
        assert_eq!(fs::read(with_suffix(&p, ".bak")).unwrap(), b"{\"a\":2}");
    }

    #[test]
    fn sync_write_uses_its_own_temp_name() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("doc.json");
        // A stale async temp file must not be touched by the sync writer.
        fs::write(with_suffix(&p, ".tmp"), b"stale").unwrap();
        write_file_atomic_sync(&p, b"[1]", true).unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"[1]");
        assert_eq!(fs::read(with_suffix(&p, ".bak")).unwrap(), b"[1]");
        assert_eq!(fs::read(with_suffix(&p, ".tmp")).unwrap(), b"stale");
        assert!(!with_suffix(&p, ".tmp-sync").exists());
    }

    #[test]
    fn rename_failure_falls_back_to_an_in_place_copy() {
        // The target is a directory: the rename fails (not busy), then the copy fails too → error, temp left.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("target");
        fs::create_dir(&p).unwrap();
        fs::write(p.join("inner"), b"x").unwrap();
        assert!(write_file_atomic(&p, b"data", false).is_err());
        assert!(with_suffix(&p, ".tmp").exists());
    }

    #[test]
    #[cfg(windows)]
    fn rename_blocked_by_open_handle_without_share_delete_copies_in_place() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("doc.json");
        fs::write(&p, b"old").unwrap();
        // Hold the target open with read+write sharing but without FILE_SHARE_DELETE (like some scanners).
        let _holder = fs::OpenOptions::new().read(true).share_mode(0x1 | 0x2).open(&p).unwrap();
        let start = std::time::Instant::now();
        write_file_atomic(&p, b"new", false).unwrap();
        assert!(start.elapsed() >= Duration::from_millis(600), "busy retries ran before the copy fallback");
        drop(_holder);
        assert_eq!(fs::read(&p).unwrap(), b"new");
        assert!(!with_suffix(&p, ".tmp").exists());
    }

    #[test]
    fn read_json_outcomes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.json");
        assert!(matches!(read_json_file(&p), JsonRead::Missing));
        fs::write(&p, "").unwrap();
        assert!(matches!(read_json_file(&p), JsonRead::Corrupt(m) if m.contains("empty")));
        fs::write(&p, " \n\t ").unwrap();
        assert!(matches!(read_json_file(&p), JsonRead::Corrupt(_)));
        fs::write(&p, "{\"a\":").unwrap();
        assert!(matches!(read_json_file(&p), JsonRead::Corrupt(_)));
        fs::write(&p, "\u{FEFF}{\"a\":1}").unwrap();
        assert!(matches!(read_json_file(&p), JsonRead::Ok(v) if v["a"] == 1));
        for any in ["null", "[]", "42", "\"text\""] {
            fs::write(&p, any).unwrap();
            assert!(matches!(read_json_file(&p), JsonRead::Ok(_)), "{any}");
        }
        // Lone surrogate escapes and out-of-range numbers do not fail the whole file.
        fs::write(&p, r#"{"t":"\ud83d","n":1e400}"#).unwrap();
        match read_json_file(&p) {
            JsonRead::Ok(v) => {
                assert_eq!(v["t"], "\u{FFFD}");
                assert!(!v["n"].is_number());
            }
            other => panic!("{other:?}"),
        }
        // Invalid UTF-8 is decoded lossily.
        fs::write(&p, b"{\"t\":\"a\xffb\"}").unwrap();
        assert!(matches!(read_json_file(&p), JsonRead::Ok(v) if v["t"] == "a\u{FFFD}b"));
    }

    #[test]
    fn read_json_on_odd_shapes_never_writes_corrupt_copies() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.json");
        for any in ["null", "[]", "42"] {
            fs::write(&p, any).unwrap();
            let out = load_with_backup(&p);
            assert!(out.value.is_some());
            assert!(!out.main_was_corrupt);
            assert!(!with_suffix(&p, ".corrupt").exists());
        }
    }

    #[test]
    fn load_with_backup_rules() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("settings.json");
        let bak = with_suffix(&p, ".bak");
        let corrupt = with_suffix(&p, ".corrupt");

        // Missing main file: a deliberate reset, the .bak is not consulted.
        fs::write(&bak, "{\"from\":\"bak\"}").unwrap();
        let out = load_with_backup(&p);
        assert!(out.value.is_none() && !out.from_backup && !out.main_was_corrupt);

        // Good main file.
        fs::write(&p, "{\"from\":\"main\"}").unwrap();
        let out = load_with_backup(&p);
        assert_eq!(out.value.unwrap()["from"], "main");

        // Corrupt main + good .bak → .corrupt copy and the .bak value.
        fs::write(&p, "{broken").unwrap();
        let out = load_with_backup(&p);
        assert_eq!(out.value.unwrap()["from"], "bak");
        assert!(out.from_backup && out.main_was_corrupt);
        assert_eq!(fs::read_to_string(&corrupt).unwrap(), "{broken");

        // Empty main + corrupt .bak → nothing.
        fs::write(&p, "").unwrap();
        fs::write(&bak, "nope").unwrap();
        let out = load_with_backup(&p);
        assert!(out.value.is_none() && !out.from_backup && out.main_was_corrupt);
        assert_eq!(fs::read_to_string(&corrupt).unwrap(), "");

        // A shape check turns a valid-but-wrong value into "corrupt".
        fs::write(&p, "{}").unwrap();
        fs::write(&bak, "[1]").unwrap();
        let out = load_with_backup_checked(&p, |v| v.is_array());
        assert!(out.from_backup && out.main_was_corrupt);
        assert_eq!(out.value.unwrap(), serde_json::json!([1]));
        assert_eq!(fs::read_to_string(&corrupt).unwrap(), "{}");
    }

    #[test]
    fn discard_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("history.json");
        discard_corrupt_copy(&p); // missing: fine
        fs::write(with_suffix(&p, ".corrupt"), "x").unwrap();
        discard_corrupt_copy(&p);
        assert!(!with_suffix(&p, ".corrupt").exists());
    }

    #[test]
    fn remove_dir_all_retry_handles_dirs_files_and_missing_paths() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("entry");
        fs::create_dir_all(d.join("nested")).unwrap();
        fs::write(d.join("nested").join("0.mp3"), b"x").unwrap();
        assert!(remove_dir_all_retry(&d));
        assert!(!d.exists());
        let f = dir.path().join("file.bin");
        fs::write(&f, b"x").unwrap();
        assert!(remove_dir_all_retry(&f));
        assert!(!f.exists());
        assert!(remove_dir_all_retry(&dir.path().join("missing")));
    }

    #[test]
    #[cfg(windows)]
    fn remove_dir_all_retry_reports_a_held_file() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("entry");
        fs::create_dir(&d).unwrap();
        let f = d.join("0.mp3");
        fs::write(&f, b"x").unwrap();
        // Opened without any sharing: deleting it fails with a sharing violation every time.
        let holder = fs::OpenOptions::new().read(true).share_mode(0).open(&f).unwrap();
        assert!(!remove_dir_all_retry_with(&d, 1));
        drop(holder);
        assert!(remove_dir_all_retry_with(&d, 1));
    }

    #[test]
    fn path_locks_are_released() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.json");
        write_file_atomic(&p, b"1", false).unwrap();
        assert!(!PATH_LOCKS.lock().contains_key(&p));
    }

    #[test]
    fn concurrent_async_writes_to_one_path_never_collide() {
        let dir = tempfile::tempdir().unwrap();
        let p = Arc::new(dir.path().join("c.json"));
        let threads: Vec<_> = (0..8)
            .map(|i| {
                let p = p.clone();
                std::thread::spawn(move || {
                    for j in 0..10 {
                        write_file_atomic(&p, format!("[{i},{j}]").as_bytes(), true).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let text = fs::read_to_string(&*p).unwrap();
        assert!(serde_json::from_str::<Value>(&text).is_ok());
        assert!(!with_suffix(&p, ".tmp").exists());
    }
}
