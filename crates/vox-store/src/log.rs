//! Logging (persistence.md §9, shell.md §1.3): a `tracing` subscriber that writes
//! `<profile>\logs\main.log` lines `ISO-8601-UTC-ms LEVEL(padded to 5) [scope] text`, rotates at 1 MiB to
//! `main.1.log` (one generation; a failed rename keeps appending), masks API keys on every line, drops `debug`
//! unless enabled, echoes to stderr in debug builds, and never panics or fails the app. The tracing *target* is the
//! scope (`tracing::info!(target: "reader", …)`). Rule for every crate: never log the text being read or keys.
//!
//! Structured fields other than `message` are appended as ` name=value` (their `Debug` form cut to 2 000 UTF-16
//! units + `…`, like the old logger's argument cut). [`log_layer`] exposes the layer itself so vox-app can compose it
//! with other layers (the shots harness collects WARN/ERROR events); [`init_logging`] installs it alone.

use std::borrow::Cow;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

use parking_lot::Mutex;
use regex::Regex;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Metadata, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::prelude::*;
use vox_core::constants::{LOG_ARG_MAX_CHARS, LOG_ROTATE_BYTES};
use vox_core::text16::{prefix16, utf16_len};

use crate::paths::Profile;

static KEY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?-u:\b)sk_[A-Za-z0-9]{16,}(?-u:\b)").expect("the key mask is a valid regex"));
static HEADER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)(xi-api-key["':\s=]+)[^\s"',}]+"#).expect("the header mask is a valid regex")
});

/// Keeps the file writer alive; dropping it flushes.
pub struct LogGuard {
    file: Option<Arc<Mutex<LogFile>>>,
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        if let Some(file) = &self.file {
            file.lock().flush();
        }
    }
}

/// The `main.log` writer: append-only, rotated to `main.1.log` when the next line would pass the limit.
struct LogFile {
    path: PathBuf,
    max_bytes: u64,
    /// Bytes this process appended since the last rotation (plus the size found at start).
    size: u64,
    file: Option<File>,
    /// `logs\` could not be created: file logging is off for the process (never retried).
    disabled: bool,
}

impl LogFile {
    fn open(path: PathBuf, max_bytes: u64) -> LogFile {
        let disabled = match path.parent() {
            Some(dir) => fs::create_dir_all(dir).is_err(),
            None => false,
        };
        let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        LogFile { path, max_bytes, size, file: None, disabled }
    }

    /// `main.log` → `main.1.log` (a trailing `.log` replaced by `.1.log`).
    fn rotated_path(&self) -> PathBuf {
        let name = self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let rotated = match name.strip_suffix(".log") {
            Some(stem) => format!("{stem}.1.log"),
            None => format!("{name}.1.log"),
        };
        self.path.with_file_name(rotated)
    }

    fn rotate(&mut self) {
        self.file = None;
        let old = self.rotated_path();
        let deleted = match fs::remove_file(&old) {
            Ok(()) => true,
            Err(e) => e.kind() == io::ErrorKind::NotFound,
        };
        if deleted {
            // Another process may hold the file: then keep appending to the big file rather than lose lines.
            let _ = fs::rename(&self.path, &old);
        }
        self.size = 0;
    }

    /// Appends one line (with its newline). All errors are swallowed.
    fn append(&mut self, line: &str) {
        if self.disabled {
            return;
        }
        let bytes = line.len() as u64;
        if self.size + bytes > self.max_bytes {
            self.rotate();
        }
        if self.file.is_none() {
            self.file = OpenOptions::new().create(true).append(true).open(&self.path).ok();
        }
        let written = match self.file.as_mut() {
            Some(f) => f.write_all(line.as_bytes()).is_ok(),
            None => false,
        };
        if written {
            self.size += bytes;
        } else {
            self.file = None;
        }
    }

    fn flush(&mut self) {
        if let Some(f) = self.file.as_mut() {
            let _ = f.flush();
        }
    }
}

/// The vox-store logging layer (see the module docs).
pub struct VoxLogLayer {
    file: Arc<Mutex<LogFile>>,
    debug: bool,
    echo: bool,
}

fn level_enabled(level: &Level, debug: bool) -> bool {
    debug || *level <= Level::INFO
}

impl<S: Subscriber> Layer<S> for VoxLogLayer {
    fn enabled(&self, metadata: &Metadata<'_>, _ctx: Context<'_, S>) -> bool {
        level_enabled(metadata.level(), self.debug)
    }

    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        if !level_enabled(meta.level(), self.debug) {
            return;
        }
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let line = format_line(chrono::Utc::now().timestamp_millis(), *meta.level(), meta.target(), &visitor.finish());
        if self.echo {
            let _ = writeln!(io::stderr(), "{line}");
        }
        let mut with_newline = line;
        with_newline.push('\n');
        self.file.lock().append(&with_newline);
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: String,
}

fn cut_arg(s: String) -> String {
    if utf16_len(&s) > LOG_ARG_MAX_CHARS {
        format!("{}…", prefix16(&s, LOG_ARG_MAX_CHARS))
    } else {
        s
    }
}

impl MessageVisitor {
    fn push_field(&mut self, name: &str, value: &str) {
        if !self.fields.is_empty() {
            self.fields.push(' ');
        }
        self.fields.push_str(name);
        self.fields.push('=');
        self.fields.push_str(value);
    }

    fn finish(self) -> String {
        match (self.message.is_empty(), self.fields.is_empty()) {
            (_, true) => self.message,
            (true, false) => self.fields,
            (false, false) => format!("{} {}", self.message, self.fields),
        }
    }
}

impl Visit for MessageVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            self.push_field(field.name(), value);
        }
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.push_field(field.name(), &cut_arg(value.to_string()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            self.push_field(field.name(), &cut_arg(format!("{value:?}")));
        }
    }
}

/// The layer and its guard, for composing with other layers (vox-app's shots harness). The file is
/// `profile.log_file()`; `debug` enables DEBUG/TRACE lines (`VOX_DEBUG`); lines are echoed to stderr in debug builds.
pub fn log_layer(profile: &Profile, debug: bool) -> (VoxLogLayer, LogGuard) {
    let file = Arc::new(Mutex::new(LogFile::open(profile.log_file(), LOG_ROTATE_BYTES)));
    // Unit tests keep stderr quiet (the harness does not capture direct stderr writes).
    let layer = VoxLogLayer { file: file.clone(), debug, echo: cfg!(debug_assertions) && !cfg!(test) };
    (layer, LogGuard { file: Some(file) })
}

/// Installs the global subscriber. Safe to call once; later calls are ignored.
pub fn init_logging(profile: &Profile, debug: bool) -> LogGuard {
    let (layer, guard) = log_layer(profile, debug);
    // A second call finds a subscriber already installed: ignored (the returned guard still flushes its own file).
    let _ = tracing::subscriber::set_global_default(tracing_subscriber::registry().with(layer));
    guard
}

/// Last line of defence: `(?-u:\b)sk_[A-Za-z0-9]{16,}(?-u:\b)` → `sk_***` and `(?i)(xi-api-key["':\s=]+)[^\s"',}]+`
/// → `$1***`. The word boundaries are **ASCII** (`(?-u:\b)`) like JS `\b` (Rust's `\b` is Unicode-aware, so
/// `clésk_…` would not be masked). Tests: `é` + `sk_` + 20 alphanumerics → masked; `xsk_…` → not masked;
/// `sk_` + 20 alphanumerics + `_x` → not masked.
pub fn redact(line: &str) -> Cow<'_, str> {
    match KEY_RE.replace_all(line, "sk_***") {
        Cow::Borrowed(_) => HEADER_RE.replace_all(line, "${1}***"),
        Cow::Owned(masked) => {
            if HEADER_RE.is_match(&masked) {
                Cow::Owned(HEADER_RE.replace_all(&masked, "${1}***").into_owned())
            } else {
                Cow::Owned(masked)
            }
        }
    }
}

/// One formatted line (without the trailing newline): `2026-10-09T00:20:30.735Z INFO  [app] text` (timestamp from
/// `vox_core::time::iso_utc_ms`). The text is masked with [`redact`] (the timestamp, level and scope are not).
pub fn format_line(timestamp_ms: i64, level: tracing::Level, scope: &str, text: &str) -> String {
    format!("{} {:<5} [{}] {}", vox_core::time::iso_utc_ms(timestamp_ms as f64), level.as_str(), scope, redact(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::path::Path;

    fn read_log(path: &Path) -> String {
        fs::read_to_string(path).unwrap_or_default()
    }

    const KEY20: &str = "abcdefghij0123456789";

    #[test]
    fn redact_vectors() {
        assert_eq!(redact(&format!("key sk_{KEY20} end")), "key sk_*** end");
        assert_eq!(redact(&format!("clé sk_{KEY20}")), "clé sk_***");
        assert_eq!(redact(&format!("é sk_{KEY20}")), "é sk_***");
        assert_eq!(redact(&format!("ésk_{KEY20}")), "ésk_***", "é is not an ASCII word character");
        assert_eq!(redact(&format!("xsk_{KEY20}")), format!("xsk_{KEY20}"));
        assert_eq!(redact(&format!("sk_{KEY20}_x")), format!("sk_{KEY20}_x"));
        assert_eq!(redact("sk_short123"), "sk_short123", "fewer than 16 characters");
        assert_eq!(redact(&format!("a=sk_{KEY20},b=sk_{KEY20}0000")), "a=sk_***,b=sk_***");
        assert_eq!(redact("xi-api-key: abc123 next"), "xi-api-key: *** next");
        assert_eq!(redact(r#"{"XI-API-KEY":"abc","x":1}"#), r#"{"XI-API-KEY":"***","x":1}"#);
        assert_eq!(redact("Xi-Api-Key=secret"), "Xi-Api-Key=***");
        assert_eq!(redact(&format!("xi-api-key: sk_{KEY20}")), "xi-api-key: ***");
        assert!(matches!(redact("nothing to hide"), Cow::Borrowed(_)));
    }

    #[test]
    fn line_format() {
        let ts = chrono::Utc.with_ymd_and_hms(2026, 10, 9, 0, 20, 30).unwrap().timestamp_millis() + 735;
        assert_eq!(
            format_line(ts, Level::INFO, "app", "Vox 0.1.0 starting (rust)"),
            "2026-10-09T00:20:30.735Z INFO  [app] Vox 0.1.0 starting (rust)"
        );
        assert_eq!(format_line(ts, Level::WARN, "data", "x"), "2026-10-09T00:20:30.735Z WARN  [data] x");
        assert_eq!(format_line(ts, Level::ERROR, "tts", "x"), "2026-10-09T00:20:30.735Z ERROR [tts] x");
        assert_eq!(format_line(ts, Level::DEBUG, "ipc", "x"), "2026-10-09T00:20:30.735Z DEBUG [ipc] x");
        assert_eq!(
            format_line(ts, Level::INFO, "tts", &format!("key sk_{KEY20}")),
            "2026-10-09T00:20:30.735Z INFO  [tts] key sk_***"
        );
    }

    #[test]
    fn rotation_with_a_tiny_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs").join("main.log");
        let mut f = LogFile::open(path.clone(), 50);
        assert!(!f.disabled);
        f.append("first line of twenty-nine b\n");
        f.append("second line of thirty-one bb\n"); // 29 + 31 > 50 → rotate first
        let rotated = dir.path().join("logs").join("main.1.log");
        assert_eq!(read_log(&rotated), "first line of twenty-nine b\n");
        assert_eq!(read_log(&path), "second line of thirty-one bb\n");
        // One generation only: the next rotation replaces main.1.log.
        f.append("third line that also does not fit\n");
        assert_eq!(read_log(&rotated), "second line of thirty-one bb\n");
        assert_eq!(read_log(&path), "third line that also does not fit\n");
        // A single line larger than the limit is still appended whole.
        let big = format!("{}\n", "x".repeat(120));
        f.append(&big);
        assert_eq!(read_log(&path), big);
        // The counter starts from the existing size.
        let again = LogFile::open(path.clone(), 50);
        assert_eq!(again.size, big.len() as u64);
    }

    #[test]
    fn unwritable_logs_folder_disables_file_logging() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("logs"), b"a file").unwrap();
        let mut f = LogFile::open(dir.path().join("logs").join("main.log"), 1000);
        assert!(f.disabled);
        f.append("dropped\n");
    }

    fn capture(debug: bool, f: impl FnOnce()) -> String {
        let dir = tempfile::tempdir().unwrap();
        let profile = Profile::at(dir.path(), false);
        let (mut layer, guard) = log_layer(&profile, debug);
        layer.echo = false;
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), f);
        drop(guard);
        read_log(&profile.log_file())
    }

    #[test]
    fn layer_writes_scoped_masked_lines_and_filters_debug() {
        let text = capture(false, || {
            tracing::info!(target: "app", "Vox {} starting", "0.1.0");
            tracing::debug!(target: "app", "hidden");
            tracing::warn!(target: "data", path = "C:\\x", attempt = 2, "could not save");
            tracing::error!(target: "tts", "key sk_{KEY20} leaked");
        });
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert!(lines[0].ends_with(" INFO  [app] Vox 0.1.0 starting"), "{}", lines[0]);
        assert!(lines[1].ends_with(r#" WARN  [data] could not save path=C:\x attempt=2"#), "{}", lines[1]);
        assert!(lines[2].ends_with(" ERROR [tts] key sk_*** leaked"), "{}", lines[2]);
        assert_eq!(lines[0].as_bytes()[10], b'T');
        assert!(lines[0][..24].ends_with('Z'));

        let text = capture(true, || tracing::debug!(target: "app", "visible"));
        assert!(text.ends_with(" DEBUG [app] visible\n"), "{text}");
    }

    #[test]
    fn long_field_values_are_cut() {
        let long = "y".repeat(3000);
        let text = capture(false, || tracing::info!(target: "app", value = ?long, "big"));
        let line = text.lines().next().unwrap();
        assert!(line.ends_with('…'));
        assert!(line.len() < 2100);
    }

    #[test]
    fn init_logging_twice_is_harmless() {
        let dir = tempfile::tempdir().unwrap();
        let profile = Profile::at(dir.path(), false);
        let g1 = init_logging(&profile, false);
        let g2 = init_logging(&profile, true);
        drop(g2);
        drop(g1);
    }
}
