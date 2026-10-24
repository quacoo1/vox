//! History records (contracts.md §3.6, persistence.md §5): the `ReadRecord` shape stored in `history.json`, its
//! sanitize rules for untrusted JSON and for typed patches, listing/search semantics and the History-page helpers.

use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;
use serde_json::Value;

use crate::constants::{ESTIMATED_CHARS_PER_SECOND, APP_NAME};
use crate::js::{js_round, js_trim};
use crate::json::{js_f64, js_opt_f64, to_js_compact};
use crate::listen_log::{same_listen_log, sanitize_listen_log, sanitize_log, ListenLog};
use crate::text16::{prefix16, utf16_len};

/// How a read was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReadSource {
    Hotkey,
    ContextChip,
    SelectionPopup,
    BrowserExtension,
    Explorer,
    Clipboard,
    Manual,
    Replay,
    Cli,
}

impl ReadSource {
    /// Validation list, in this order (main/ipc.ts:73-83).
    pub const ALL: [ReadSource; 9] = [
        ReadSource::Hotkey,
        ReadSource::ContextChip,
        ReadSource::SelectionPopup,
        ReadSource::BrowserExtension,
        ReadSource::Explorer,
        ReadSource::Clipboard,
        ReadSource::Manual,
        ReadSource::Replay,
        ReadSource::Cli,
    ];
    /// History "Filter by source" order (charts/labels.ts:31-41).
    pub const FILTER_ORDER: [ReadSource; 9] = [
        ReadSource::ContextChip,
        ReadSource::Hotkey,
        ReadSource::SelectionPopup,
        ReadSource::BrowserExtension,
        ReadSource::Clipboard,
        ReadSource::Manual,
        ReadSource::Explorer,
        ReadSource::Replay,
        ReadSource::Cli,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hotkey => "hotkey",
            Self::ContextChip => "context-chip",
            Self::SelectionPopup => "selection-popup",
            Self::BrowserExtension => "browser-extension",
            Self::Explorer => "explorer",
            Self::Clipboard => "clipboard",
            Self::Manual => "manual",
            Self::Replay => "replay",
            Self::Cli => "cli",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|x| x.as_str() == s)
    }
    /// UI label (ui-main.md §6.2).
    pub fn label(self) -> &'static str {
        match self {
            Self::ContextChip => "Right-click chip",
            Self::Hotkey => "Hotkey",
            Self::SelectionPopup => "Selection popup",
            Self::BrowserExtension => "Browser extension",
            Self::Explorer => "File Explorer",
            Self::Clipboard => "Clipboard",
            Self::Manual => "Typed in app",
            Self::Replay => "Replay",
            Self::Cli => "Command line",
        }
    }
    /// Lucide icon name (ui/icons) for this source.
    pub fn icon(self) -> &'static str {
        match self {
            Self::ContextChip => "mouse-pointer-click",
            Self::Hotkey => "keyboard",
            Self::SelectionPopup => "text-select",
            Self::BrowserExtension => "globe",
            Self::Explorer => "folder-open",
            Self::Clipboard => "clipboard",
            Self::Manual => "type",
            Self::Replay => "repeat",
            Self::Cli => "terminal",
        }
    }
    /// Analytics app-name fallback when a record has no `sourceApp` (persistence.md §7.3).
    pub fn fallback_app_name(self) -> &'static str {
        match self {
            Self::Manual | Self::Replay => APP_NAME,
            Self::Clipboard => "Clipboard",
            Self::Explorer => "Explorer",
            Self::Cli => "Command line",
            Self::BrowserExtension => "Browser",
            _ => "Other",
        }
    }
}

/// One read, as persisted (field order = serialized order; absent optionals are omitted, never `null`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadRecord {
    /// UUID v4 (≤ 128 chars); also the audio-cache folder name.
    pub id: String,
    /// Epoch ms (any finite number is accepted on load; not rounded).
    #[serde(serialize_with = "js_f64")]
    pub created_at: f64,
    /// Full cleaned text, or the first 160 UTF-16 units when `storeText` was off.
    pub text: String,
    pub text_truncated: bool,
    /// UTF-16 length of the full cleaned text.
    pub char_count: u64,
    pub word_count: u64,
    pub voice_id: String,
    pub voice_name: String,
    pub model_id: String,
    pub source: ReadSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_app: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_process: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    #[serde(serialize_with = "js_f64")]
    pub audio_duration_sec: f64,
    /// Wall-clock seconds listened, all sessions incl. replays.
    #[serde(serialize_with = "js_f64")]
    pub listened_sec: f64,
    /// Furthest audio position reached; `None` = absent / 0.
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "js_opt_f64")]
    pub max_position_sec: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listen_log: Option<ListenLog>,
    pub completed: bool,
    pub cached: bool,
    pub characters_billed: u64,
    pub favorite: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ReadRecord {
    /// A fresh record with zero/false bookkeeping fields (the reader fills creation fields).
    pub fn new(id: String, created_at: f64, source: ReadSource) -> Self {
        ReadRecord {
            id,
            created_at,
            text: String::new(),
            text_truncated: false,
            char_count: 0,
            word_count: 0,
            voice_id: String::new(),
            voice_name: String::new(),
            model_id: String::new(),
            source,
            source_app: None,
            source_process: None,
            source_title: None,
            source_url: None,
            audio_duration_sec: 0.0,
            listened_sec: 0.0,
            max_position_sec: None,
            listen_log: None,
            completed: false,
            cached: false,
            characters_billed: 0,
            favorite: false,
            error: None,
        }
    }
}

// ───────────────────────────── sanitize ─────────────────────────────

fn finite(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::Number(n)) => n.as_f64().filter(|x| x.is_finite()),
        _ => None,
    }
}

fn to_count(x: f64) -> u64 {
    let r = js_round(x.max(0.0));
    if r >= u64::MAX as f64 {
        u64::MAX
    } else {
        r as u64
    }
}

/// `optString`: absent / non-string → base; string → trimmed, `""` → absent.
fn opt_string(v: Option<&Value>, base: Option<&String>) -> Option<String> {
    match v {
        Some(Value::String(s)) => {
            let t = js_trim(s);
            (!t.is_empty()).then(|| t.to_string())
        }
        _ => base.cloned(),
    }
}

fn opt_trimmed(s: Option<String>) -> Option<String> {
    s.and_then(|x| {
        let t = js_trim(&x);
        (!t.is_empty()).then(|| t.to_string())
    })
}

/// Whitespace word count: `\s+`-separated tokens of the JS-trimmed text (persistence.md §8.4 `countWords`). Used by
/// exactly one producer: `vox_store::demo::generate_demo_records` (ARCHITECTURE.md §8 D-P-14), so the persistence.md
/// §12 demo vectors (totals.words, avgWordsPerRead, avgListenWpm, readingTimeEquivalentSec) hold. Everything else
/// counts with [`count_words`] (D-C-1).
pub fn count_whitespace_words(text: &str) -> u64 {
    let t = js_trim(text);
    if t.is_empty() {
        0
    } else {
        t.split(crate::js::is_js_whitespace).filter(|s| !s.is_empty()).count() as u64
    }
}

static WORD_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\p{L}\p{N}][\p{L}\p{N}'’-]*").expect("valid regex"));

/// The reader's word count: matches of `/[\p{L}\p{N}][\p{L}\p{N}'’-]*/gu`. Decision (contracts.md §8 #1): used
/// everywhere, including the sanitize fallback.
pub fn count_words(text: &str) -> u64 {
    WORD_RE.find_iter(text).count() as u64
}

/// `\S+` runs (the UI's live word counter in Quick read).
pub fn count_tokens(text: &str) -> u64 {
    text.split(crate::js::is_js_whitespace).filter(|s| !s.is_empty()).count() as u64
}

impl ReadRecord {
    /// Normalises an untrusted record (history-store.ts:48-98). Without `base`, a record lacking a usable `id` or
    /// `createdAt` is rejected (`None`). Unknown keys are dropped.
    pub fn from_value(v: &Value, base: Option<&ReadRecord>) -> Option<ReadRecord> {
        let o = v.as_object()?;
        let g = |k: &str| o.get(k);
        let id = match g("id") {
            Some(Value::String(s)) if !js_trim(s).is_empty() => prefix16(js_trim(s), 128).to_string(),
            _ => base?.id.clone(),
        };
        let created_at = finite(g("createdAt")).or(base.map(|b| b.created_at))?;
        let text = match g("text") {
            Some(Value::String(s)) => s.clone(),
            _ => base.map(|b| b.text.clone()).unwrap_or_default(),
        };
        let bool_or = |k: &str, fb: fn(&ReadRecord) -> bool| -> bool {
            match g(k) {
                Some(Value::Bool(b)) => *b,
                _ => base.map(fb).unwrap_or(false),
            }
        };
        let string_or = |k: &str, fb: fn(&ReadRecord) -> &String| -> String {
            match g(k) {
                Some(Value::String(s)) => s.clone(),
                _ => base.map(|b| fb(b).clone()).unwrap_or_default(),
            }
        };
        let source = match g("source") {
            Some(Value::String(s)) => ReadSource::parse(s),
            _ => None,
        }
        .or(base.map(|b| b.source))
        .unwrap_or(ReadSource::Manual);
        let char_count = finite(g("charCount"))
            .map(to_count)
            .or(base.map(|b| b.char_count))
            .unwrap_or_else(|| utf16_len(&text) as u64);
        let word_count = finite(g("wordCount")).map(to_count).or(base.map(|b| b.word_count)).unwrap_or_else(|| count_words(&text));
        let non_neg = |k: &str, fb: fn(&ReadRecord) -> f64| -> f64 {
            finite(g(k)).map(|x| x.max(0.0)).or(base.map(fb)).unwrap_or(0.0)
        };
        let max_position = finite(g("maxPositionSec"))
            .map(|x| x.max(0.0))
            .or(base.and_then(|b| b.max_position_sec))
            .filter(|x| *x != 0.0);
        let listen_log = match g("listenLog") {
            None => base.and_then(|b| b.listen_log.clone()),
            Some(l) => sanitize_listen_log(l),
        };
        Some(ReadRecord {
            id,
            created_at,
            text_truncated: bool_or("textTruncated", |b| b.text_truncated),
            char_count,
            word_count,
            voice_id: string_or("voiceId", |b| &b.voice_id),
            voice_name: string_or("voiceName", |b| &b.voice_name),
            model_id: string_or("modelId", |b| &b.model_id),
            source,
            source_app: opt_string(g("sourceApp"), base.and_then(|b| b.source_app.as_ref())),
            source_process: opt_string(g("sourceProcess"), base.and_then(|b| b.source_process.as_ref())).map(|s| s.to_lowercase()),
            source_title: opt_string(g("sourceTitle"), base.and_then(|b| b.source_title.as_ref())),
            source_url: opt_string(g("sourceUrl"), base.and_then(|b| b.source_url.as_ref())),
            audio_duration_sec: non_neg("audioDurationSec", |b| b.audio_duration_sec),
            listened_sec: non_neg("listenedSec", |b| b.listened_sec),
            max_position_sec: max_position,
            listen_log,
            completed: bool_or("completed", |b| b.completed),
            cached: bool_or("cached", |b| b.cached),
            characters_billed: finite(g("charactersBilled")).map(to_count).or(base.map(|b| b.characters_billed)).unwrap_or(0),
            favorite: bool_or("favorite", |b| b.favorite),
            error: opt_string(g("error"), base.and_then(|b| b.error.as_ref())),
            text,
        })
    }

    /// Applies the store's normalisation to a typed record (trims optional strings, lower-cases the process,
    /// clamps numbers, drops a zero `maxPositionSec`, sanitizes the ledger, cuts the id to 128 units).
    pub fn normalized(mut self) -> ReadRecord {
        self.id = prefix16(js_trim(&self.id), 128).to_string();
        self.source_app = opt_trimmed(self.source_app);
        self.source_process = opt_trimmed(self.source_process).map(|s| s.to_lowercase());
        self.source_title = opt_trimmed(self.source_title);
        self.source_url = opt_trimmed(self.source_url);
        self.error = opt_trimmed(self.error);
        let nn = |x: f64, fb: f64| if x.is_finite() { x.max(0.0) } else { fb };
        self.audio_duration_sec = nn(self.audio_duration_sec, 0.0);
        self.listened_sec = nn(self.listened_sec, 0.0);
        self.max_position_sec = self.max_position_sec.filter(|x| x.is_finite()).map(|x| x.max(0.0)).filter(|x| *x != 0.0);
        self.listen_log = self.listen_log.as_ref().and_then(sanitize_log);
        self
    }

    /// `update(id, patch)` merge: patch fields over `self`, re-normalised with `self` as the base. The id never
    /// changes. Compare the result with `self` (`==`) to detect a no-op.
    pub fn apply_patch(&self, p: &RecordPatch) -> ReadRecord {
        let mut r = self.clone();
        let nn = |v: Option<f64>, cur: f64| v.filter(|x| x.is_finite()).map(|x| x.max(0.0)).unwrap_or(cur);
        let cnt = |v: Option<f64>, cur: u64| v.filter(|x| x.is_finite()).map(to_count).unwrap_or(cur);
        if let Some(t) = p.created_at.filter(|x| x.is_finite()) {
            r.created_at = t;
        }
        if let Some(t) = &p.text {
            r.text = t.clone();
        }
        if let Some(b) = p.text_truncated {
            r.text_truncated = b;
        }
        r.char_count = cnt(p.char_count, r.char_count);
        r.word_count = cnt(p.word_count, r.word_count);
        if let Some(s) = &p.voice_id {
            r.voice_id = s.clone();
        }
        if let Some(s) = &p.voice_name {
            r.voice_name = s.clone();
        }
        if let Some(s) = &p.model_id {
            r.model_id = s.clone();
        }
        if let Some(s) = p.source {
            r.source = s;
        }
        let set_opt = |field: &mut Option<String>, v: &Option<Option<String>>| {
            if let Some(v) = v {
                *field = v.clone();
            }
        };
        set_opt(&mut r.source_app, &p.source_app);
        set_opt(&mut r.source_process, &p.source_process);
        set_opt(&mut r.source_title, &p.source_title);
        set_opt(&mut r.source_url, &p.source_url);
        set_opt(&mut r.error, &p.error);
        r.audio_duration_sec = nn(p.audio_duration_sec, r.audio_duration_sec);
        r.listened_sec = nn(p.listened_sec, r.listened_sec);
        if let Some(m) = p.max_position_sec.filter(|x| x.is_finite()) {
            r.max_position_sec = Some(m.max(0.0));
        }
        if let Some(l) = &p.listen_log {
            r.listen_log = Some(l.clone());
        }
        if let Some(b) = p.completed {
            r.completed = b;
        }
        if let Some(b) = p.cached {
            r.cached = b;
        }
        r.characters_billed = cnt(p.characters_billed, r.characters_billed);
        if let Some(b) = p.favorite {
            r.favorite = b;
        }
        r.normalized()
    }

    /// Field-by-field equality with ledgers compared by value (absent ≡ empty).
    pub fn same_as(&self, other: &ReadRecord) -> bool {
        let mut a = self.clone();
        let mut b = other.clone();
        let logs_equal = same_listen_log(a.listen_log.as_ref(), b.listen_log.as_ref());
        a.listen_log = None;
        b.listen_log = None;
        logs_equal && a == b
    }

    /// Compact JSON of this record.
    pub fn to_json(&self) -> String {
        to_js_compact(self)
    }
}

/// A typed history patch. `None` = keep; for clearable strings `Some(None)` clears and `Some(Some(s))` sets
/// (trimmed; `""` clears). Non-finite numbers are ignored.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecordPatch {
    pub created_at: Option<f64>,
    pub text: Option<String>,
    pub text_truncated: Option<bool>,
    pub char_count: Option<f64>,
    pub word_count: Option<f64>,
    pub voice_id: Option<String>,
    pub voice_name: Option<String>,
    pub model_id: Option<String>,
    pub source: Option<ReadSource>,
    pub source_app: Option<Option<String>>,
    pub source_process: Option<Option<String>>,
    pub source_title: Option<Option<String>>,
    pub source_url: Option<Option<String>>,
    pub audio_duration_sec: Option<f64>,
    pub listened_sec: Option<f64>,
    pub max_position_sec: Option<f64>,
    pub listen_log: Option<ListenLog>,
    pub completed: Option<bool>,
    pub cached: Option<bool>,
    pub characters_billed: Option<f64>,
    pub favorite: Option<bool>,
    pub error: Option<Option<String>>,
}

// ───────────────────────────── files ─────────────────────────────

/// `history.json` file wrapper: `{"version":1,"records":[…]}`.
#[derive(Debug, Serialize)]
pub struct HistoryFile<'a> {
    pub version: u32,
    pub records: &'a [ReadRecord],
}

/// History export (pretty JSON): `{"app":"Vox","version":1,"exportedAt":ISO,"records":[…]}`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryExport<'a> {
    pub app: &'a str,
    pub version: u32,
    pub exported_at: String,
    pub records: &'a [ReadRecord],
}

/// Parses a loaded `history.json` value (`{records:[…]}` or a bare array): sanitizes every record, counts rejects,
/// sorts by `createdAt` descending (stable) and drops later duplicates of an id. Returns `(records, rejected)`.
pub fn parse_history_file(v: &Value) -> (Vec<ReadRecord>, usize) {
    let items: &[Value] = match v {
        Value::Array(a) => a,
        Value::Object(o) => match o.get("records") {
            Some(Value::Array(a)) => a,
            _ => &[],
        },
        _ => &[],
    };
    let mut rejected = 0;
    let mut recs: Vec<ReadRecord> = Vec::with_capacity(items.len());
    for it in items {
        match ReadRecord::from_value(it, None) {
            Some(r) => recs.push(r),
            None => rejected += 1,
        }
    }
    recs.sort_by(|a, b| b.created_at.partial_cmp(&a.created_at).unwrap_or(std::cmp::Ordering::Equal));
    let mut seen = std::collections::HashSet::new();
    recs.retain(|r| seen.insert(r.id.clone()));
    (recs, rejected)
}

/// Index where a record with `created_at` belongs in a newest-first list: after every strictly newer record and
/// **before** existing records with an equal timestamp.
pub fn insertion_index(records: &[ReadRecord], created_at: f64) -> usize {
    records.partition_point(|r| r.created_at > created_at)
}

// ───────────────────────────── query ─────────────────────────────

/// `history:list` query.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HistoryQuery {
    /// Raw search text (only the first 500 units are used).
    pub search: Option<String>,
    /// Default 50, floor + clamp 1..500.
    pub limit: Option<f64>,
    /// Default 0, floor, ≥ 0.
    pub offset: Option<f64>,
    pub favorites_only: bool,
    pub source: Option<ReadSource>,
}

impl HistoryQuery {
    /// Effective `limit`.
    pub fn effective_limit(&self) -> usize {
        match self.limit.filter(|x| x.is_finite()) {
            Some(l) => l.floor().clamp(1.0, 500.0) as usize,
            None => 50,
        }
    }
    /// Effective `offset`.
    pub fn effective_offset(&self) -> usize {
        match self.offset.filter(|x| x.is_finite()) {
            Some(o) => o.floor().clamp(0.0, 9_007_199_254_740_991.0) as usize,
            None => 0,
        }
    }
    /// Lower-cased literal search terms: the first 500 units, trimmed, split on whitespace, at most 16.
    pub fn terms(&self) -> Vec<String> {
        let Some(s) = &self.search else { return Vec::new() };
        let s = prefix16(s, 500);
        js_trim(s)
            .split(crate::js::is_js_whitespace)
            .filter(|t| !t.is_empty())
            .take(16)
            .map(|t| t.to_lowercase())
            .collect()
    }
    /// True if `r` matches the filters and every term occurs (case-insensitively) in one of text, sourceApp,
    /// sourceTitle, voiceName, sourceUrl.
    pub fn matches(&self, r: &ReadRecord, terms: &[String]) -> bool {
        if self.favorites_only && !r.favorite {
            return false;
        }
        if let Some(s) = self.source {
            if r.source != s {
                return false;
            }
        }
        if terms.is_empty() {
            return true;
        }
        let fields: Vec<String> = [Some(&r.text), r.source_app.as_ref(), r.source_title.as_ref(), Some(&r.voice_name), r.source_url.as_ref()]
            .into_iter()
            .flatten()
            .map(|s| s.to_lowercase())
            .collect();
        terms.iter().all(|t| fields.iter().any(|f| f.contains(t.as_str())))
    }
}

/// One page of matches.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HistoryPage {
    pub items: Vec<ReadRecord>,
    /// Matches before paging.
    pub total: usize,
}

/// Runs a query over a newest-first record list.
pub fn list_records(records: &[ReadRecord], q: &HistoryQuery) -> HistoryPage {
    let terms = q.terms();
    let matches: Vec<&ReadRecord> = records.iter().filter(|r| q.matches(r, &terms)).collect();
    let total = matches.len();
    let items = matches.into_iter().skip(q.effective_offset()).take(q.effective_limit()).cloned().collect();
    HistoryPage { items, total }
}

/// Number of non-favorite records `applyRetention(days)` would delete now (`createdAt < now − days × 86 400 000`).
pub fn count_expiring(records: &[ReadRecord], days: u32, now_ms: f64) -> usize {
    if days == 0 {
        return 0;
    }
    let cutoff = now_ms - days as f64 * crate::constants::DAY_MS;
    records.iter().filter(|r| !r.favorite && r.created_at < cutoff).count()
}

// ───────────────────────────── History-page helpers ─────────────────────────────

/// "Heard" share for the History row: completed → 1; unknown audio length → 0; else reached / audio, capped at 0.99.
pub fn listened_fraction(r: &ReadRecord) -> f64 {
    if r.completed {
        return 1.0;
    }
    if r.audio_duration_sec.is_nan() || r.audio_duration_sec <= 0.0 {
        return 0.0;
    }
    let reached = match r.max_position_sec {
        Some(m) if m > 0.0 => m,
        _ => r.listened_sec,
    };
    let share = reached / r.audio_duration_sec;
    if share.is_nan() {
        0.0
    } else {
        share.clamp(0.0, 0.99)
    }
}

/// Audio length `(seconds, is_estimate)`: the synthesized length, else `round(charCount / 15)` estimated.
pub fn audio_length(r: &ReadRecord) -> (f64, bool) {
    if r.audio_duration_sec > 0.0 {
        (r.audio_duration_sec, false)
    } else {
        (js_round(r.char_count as f64 / ESTIMATED_CHARS_PER_SECOND), true)
    }
}

/// Replay is possible: cached audio, or the full text to re-synthesize.
pub fn can_replay(r: &ReadRecord) -> bool {
    r.cached || !r.text_truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SAMPLE: &str = r#"{"version":1,"records":[{"id":"7c134be0-e9c6-4e9e-a122-0a916f3827ff","createdAt":1792229746902,"text":"Hello there.","textTruncated":false,"charCount":12,"wordCount":2,"voiceId":"nPczCjzI2devNBz1zQrb","voiceName":"Brian","modelId":"eleven_multilingual_v2","source":"context-chip","sourceApp":"Chrome","sourceProcess":"chrome.exe","sourceTitle":"Harlow Bay - Google Chrome","audioDurationSec":69.3,"listenedSec":69.3,"maxPositionSec":69.3,"listenLog":{"2026-10-17T09":69.3},"completed":true,"cached":false,"charactersBilled":969,"favorite":false},{"id":"b","createdAt":1792220998996,"text":"x","textTruncated":true,"charCount":400,"wordCount":60,"voiceId":"","voiceName":"","modelId":"","source":"manual","audioDurationSec":0,"listenedSec":0,"completed":false,"cached":false,"charactersBilled":0,"favorite":true,"error":"Only part of the text could be synthesized"}]}"#;

    #[test]
    fn history_file_round_trips_byte_identical() {
        let v = crate::json::parse_lenient(SAMPLE).unwrap();
        let (recs, rejected) = parse_history_file(&v);
        assert_eq!(rejected, 0);
        assert_eq!(recs.len(), 2);
        let out = to_js_compact(&HistoryFile { version: 1, records: &recs });
        assert_eq!(out, SAMPLE);
    }

    #[test]
    fn sanitize_rules() {
        let r = ReadRecord::from_value(
            &json!({"id": "  x  ", "createdAt": 5.5, "text": " a b ", "charCount": -3, "wordCount": "9",
                    "source": "nope", "sourceApp": "   ", "sourceProcess": " Chrome.EXE ", "maxPositionSec": 0,
                    "listenLog": {}, "completed": 1, "audioDurationSec": -2, "error": 7, "extra": true}),
            None,
        )
        .unwrap();
        assert_eq!(r.id, "x");
        assert_eq!(r.created_at, 5.5);
        assert_eq!(r.text, " a b ");
        assert_eq!(r.char_count, 0);
        assert_eq!(r.word_count, 2); // fallback: Unicode word rule over the text
        assert_eq!(r.source, ReadSource::Manual);
        assert_eq!(r.source_app, None);
        assert_eq!(r.source_process.as_deref(), Some("chrome.exe"));
        assert_eq!(r.max_position_sec, None);
        assert_eq!(r.listen_log, None);
        assert!(!r.completed);
        assert_eq!(r.audio_duration_sec, 0.0);
        assert_eq!(r.error, None);
        assert!(ReadRecord::from_value(&json!({"createdAt": 1}), None).is_none());
        assert!(ReadRecord::from_value(&json!({"id": "a"}), None).is_none());
        assert!(!r.to_json().contains("extra"));
    }

    #[test]
    fn patch_semantics() {
        let base = ReadRecord::from_value(&json!({"id": "a", "createdAt": 1, "sourceApp": "Chrome", "error": "x"}), None).unwrap();
        let same = base.apply_patch(&RecordPatch { listened_sec: Some(f64::NAN), ..Default::default() });
        assert_eq!(same, base);
        let cleared = base.apply_patch(&RecordPatch { source_app: Some(None), error: Some(Some("  ".into())), ..Default::default() });
        assert_eq!(cleared.source_app, None);
        assert_eq!(cleared.error, None);
        let p = base.apply_patch(&RecordPatch { listened_sec: Some(12.34), characters_billed: Some(10.5), max_position_sec: Some(0.0), ..Default::default() });
        assert_eq!(p.listened_sec, 12.34);
        assert_eq!(p.characters_billed, 11);
        assert_eq!(p.max_position_sec, None);
    }

    #[test]
    fn ordering_and_dedupe() {
        let v = json!([{"id": "a", "createdAt": 1}, {"id": "b", "createdAt": 3}, {"id": "a", "createdAt": 2}, {"id": "c", "createdAt": 3}]);
        let (recs, _) = parse_history_file(&v);
        let ids: Vec<&str> = recs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["b", "c", "a"]);
        assert_eq!(recs[2].created_at, 2.0);
        assert_eq!(insertion_index(&recs, 3.0), 0); // ties go before existing
        assert_eq!(insertion_index(&recs, 2.5), 2);
    }

    #[test]
    fn listing() {
        let v = crate::json::parse_lenient(SAMPLE).unwrap();
        let (recs, _) = parse_history_file(&v);
        let q = HistoryQuery { search: Some("  HELLO   chrome ".into()), ..Default::default() };
        assert_eq!(list_records(&recs, &q).total, 1);
        let q = HistoryQuery { favorites_only: true, ..Default::default() };
        assert_eq!(list_records(&recs, &q).items[0].id, "b");
        let q = HistoryQuery { limit: Some(0.0), offset: Some(1.7), ..Default::default() };
        let page = list_records(&recs, &q);
        assert_eq!((page.items.len(), page.total), (1, 2));
        assert_eq!(count_expiring(&recs, 1, 1792229746902.0 + 2.0 * 86_400_000.0), 1);
    }

    #[test]
    fn words_and_fractions() {
        assert_eq!(count_words("Don’t stop-me now, 42 times!"), 5);
        assert_eq!(count_words("中文"), 1);
        assert_eq!(count_tokens("  a  b\nc "), 3);
        // persistence.md §8 demo snippets: the demo generator’s whitespace rule differs from the reader’s rule
        // (D-P-14), and the §12 vectors depend on the whitespace one.
        const COUNCIL: &str = r#"The city council voted on Tuesday to make its overnight bus network permanent after a two-year trial that carried more than 1.4 million passengers. Five routes will run every thirty minutes between midnight and five in the morning, linking the hospital district, the airport and the three largest residential neighbourhoods. Night-shift workers made up almost half of all trips during the trial, according to the transport office. Councillors who opposed the plan questioned the running costs, but the motion passed comfortably."#;
        const TENANCY: &str = r#"7.1 The landlord shall keep in repair the structure and exterior of the property, including drains, gutters and external pipes, and shall keep in proper working order the installations for the supply of water, gas, electricity, sanitation and heating. 7.2 The tenant shall notify the landlord in writing of any defect or disrepair within a reasonable time of becoming aware of it. 7.3 The landlord or their agent may enter the property to inspect its condition or carry out repairs, provided that at least twenty-four hours' written notice has been given, except in an emergency. 7.4 The tenant shall not be responsible for fair wear and tear, nor for damage caused by an insured risk unless that damage results from the tenant's negligence."#;
        assert_eq!(count_whitespace_words(COUNCIL), 81);
        assert_eq!(count_words(COUNCIL), 82);
        assert_eq!(count_whitespace_words(TENANCY), 123);
        assert_eq!(count_words(TENANCY), 127);
        assert_eq!(count_whitespace_words("   "), 0);
        let mut r = ReadRecord::new("a".into(), 0.0, ReadSource::Manual);
        r.audio_duration_sec = 100.0;
        r.max_position_sec = Some(100.0);
        assert_eq!(listened_fraction(&r), 0.99);
        r.audio_duration_sec = 0.0;
        r.char_count = 400;
        assert_eq!(audio_length(&r), (27.0, true));
        // ui-main.md §13: {audio 0, listened 50} → 0; {audio 100, listened 40} → 0.4; audioLength({0, 300}) → (20, est).
        let mut q = ReadRecord::new("b".into(), 0.0, ReadSource::Manual);
        q.listened_sec = 50.0;
        assert_eq!(listened_fraction(&q), 0.0);
        q.audio_duration_sec = 100.0;
        q.listened_sec = 40.0;
        assert_eq!(listened_fraction(&q), 0.4);
        q.audio_duration_sec = 0.0;
        q.char_count = 300;
        assert_eq!(audio_length(&q), (20.0, true));
    }
}
