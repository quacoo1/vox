//! The settings schema (contracts.md §3.2, §4; persistence.md §3): every key, default, clamp and sanitize rule, the
//! deep-merge patch semantics and the stored JSON form, plus the pure option builders the Settings page uses
//! (ui-settings-widget.md §2).
//!
//! In memory a [`Settings`] is always fully valid. Untrusted input (a `settings.json` from disk, a JSON patch from
//! the UI) is merged as JSON and rebuilt field by field with [`Settings::sanitize`]: an invalid value falls back to the
//! `base` value (the defaults when loading, the current settings when patching) — it never raises an error.

use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::constants::*;
use crate::js::{fixed_trimmed, js_round, js_trim};
use crate::json::{js_f64, to_js_compact, to_js_pretty};
use crate::text16::prefix16;
use crate::voices::TtsModel;

// ───────────────────────────── enums ─────────────────────────────

/// `app.theme`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    System,
    Paper,
    Night,
}

impl ThemeMode {
    pub const ALL: [ThemeMode; 3] = [ThemeMode::System, ThemeMode::Paper, ThemeMode::Night];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Paper => "paper",
            Self::Night => "night",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == s)
    }
    /// Settings → Appearance label.
    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Paper => "Paper",
            Self::Night => "Night",
        }
    }
}

/// The theme actually shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ResolvedTheme {
    Paper,
    Night,
}

impl ResolvedTheme {
    pub fn is_dark(self) -> bool {
        self == Self::Night
    }
}

/// `system` follows the OS app theme (dark → night).
pub fn resolve_theme(mode: ThemeMode, os_dark: bool) -> ResolvedTheme {
    match mode {
        ThemeMode::Paper => ResolvedTheme::Paper,
        ThemeMode::Night => ResolvedTheme::Night,
        ThemeMode::System => {
            if os_dark {
                ResolvedTheme::Night
            } else {
                ResolvedTheme::Paper
            }
        }
    }
}

/// `widget.position`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum WidgetPosition {
    BottomRight,
    TopRight,
    BottomCenter,
    NearCursor,
    Remember,
}

impl WidgetPosition {
    pub const ALL: [WidgetPosition; 5] = [
        WidgetPosition::BottomRight,
        WidgetPosition::TopRight,
        WidgetPosition::BottomCenter,
        WidgetPosition::NearCursor,
        WidgetPosition::Remember,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BottomRight => "bottom-right",
            Self::TopRight => "top-right",
            Self::BottomCenter => "bottom-center",
            Self::NearCursor => "near-cursor",
            Self::Remember => "remember",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == s)
    }
    /// Segmented-control label (ui-settings-widget.md §2.6).
    pub fn label(self) -> &'static str {
        match self {
            Self::BottomRight => "Bottom right",
            Self::TopRight => "Top right",
            Self::BottomCenter => "Bottom center",
            Self::NearCursor => "Cursor",
            Self::Remember => "Last spot",
        }
    }
    /// Accessible label.
    pub fn accessible_label(self) -> &'static str {
        match self {
            Self::NearCursor => "Near the cursor",
            Self::Remember => "Where I last dragged it",
            other => other.label(),
        }
    }
    /// Row description shown for the current position.
    pub fn hint(self) -> &'static str {
        match self {
            Self::BottomRight => "Above the clock, out of the way of most documents.",
            Self::TopRight => "Top corner, clear of the taskbar and notifications.",
            Self::BottomCenter => "Centered above the taskbar, like a media flyout.",
            Self::NearCursor => "Next to where you selected the text.",
            Self::Remember => "Wherever you last dropped it. Drag the widget to move it.",
        }
    }
}

/// `outputFormat` (ElevenLabs `output_format`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum OutputFormat {
    #[serde(rename = "mp3_44100_128")]
    Mp3_44100_128,
    #[serde(rename = "mp3_44100_64")]
    Mp3_44100_64,
    #[serde(rename = "mp3_22050_32")]
    Mp3_22050_32,
}

impl OutputFormat {
    pub const ALL: [OutputFormat; 3] = [OutputFormat::Mp3_44100_128, OutputFormat::Mp3_44100_64, OutputFormat::Mp3_22050_32];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mp3_44100_128 => "mp3_44100_128",
            Self::Mp3_44100_64 => "mp3_44100_64",
            Self::Mp3_22050_32 => "mp3_22050_32",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|m| m.as_str() == s)
    }
    /// "Audio quality" option label.
    pub fn label(self) -> &'static str {
        match self {
            Self::Mp3_44100_128 => "High \u{00B7} 128 kbps",
            Self::Mp3_44100_64 => "Standard \u{00B7} 64 kbps",
            Self::Mp3_22050_32 => "Data saver \u{00B7} 32 kbps",
        }
    }
    /// Constant MP3 bitrate (used for byte-size durations, tts.md §4.8).
    pub fn kbps(self) -> u32 {
        match self {
            Self::Mp3_44100_128 => 128,
            Self::Mp3_44100_64 => 64,
            Self::Mp3_22050_32 => 32,
        }
    }
    /// MIME of the audio ElevenLabs returns for this format.
    pub fn mime(self) -> &'static str {
        "audio/mpeg"
    }
}

// ───────────────────────────── structs ─────────────────────────────

/// ElevenLabs server-side voice settings.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceSettings {
    #[serde(serialize_with = "js_f64")]
    pub stability: f64,
    #[serde(serialize_with = "js_f64")]
    pub similarity_boost: f64,
    #[serde(serialize_with = "js_f64")]
    pub style: f64,
    pub use_speaker_boost: bool,
    /// Server-side speed 0.7..1.2 (also divides duration estimates).
    #[serde(serialize_with = "js_f64")]
    pub speed: f64,
}

impl Default for VoiceSettings {
    fn default() -> Self {
        Self { stability: 0.5, similarity_boost: 0.75, style: 0.0, use_speaker_boost: true, speed: 1.0 }
    }
}

impl VoiceSettings {
    /// All five values equal the defaults (the "Reset" button is disabled).
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// `triggers.hotkeys`: Electron accelerator strings, `""` = disabled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HotkeySettings {
    pub read_selection: String,
    pub read_clipboard: String,
    pub play_pause: String,
    pub stop: String,
}

impl Default for HotkeySettings {
    fn default() -> Self {
        Self {
            read_selection: "Ctrl+Alt+R".into(),
            read_clipboard: "Ctrl+Alt+V".into(),
            play_pause: "Ctrl+Alt+P".into(),
            stop: "Ctrl+Alt+S".into(),
        }
    }
}

impl HotkeySettings {
    /// The accelerator for an action (raw, as stored).
    pub fn get(&self, action: crate::hotkey::HotkeyAction) -> &str {
        use crate::hotkey::HotkeyAction::*;
        match action {
            ReadSelection => &self.read_selection,
            ReadClipboard => &self.read_clipboard,
            PlayPause => &self.play_pause,
            Stop => &self.stop,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TriggerSettings {
    pub right_click_chip: bool,
    pub selection_popup: bool,
    pub hotkeys_enabled: bool,
    pub hotkeys: HotkeySettings,
    pub media_keys: bool,
    pub browser_extension: bool,
    pub explorer_menu: bool,
    /// Lower-case exe names where chips never appear.
    pub excluded_apps: Vec<String>,
    pub restore_clipboard: bool,
}

impl Default for TriggerSettings {
    fn default() -> Self {
        Self {
            right_click_chip: true,
            selection_popup: false,
            hotkeys_enabled: true,
            hotkeys: HotkeySettings::default(),
            media_keys: true,
            browser_extension: true,
            explorer_menu: false,
            excluded_apps: Vec::new(),
            restore_clipboard: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WidgetSettings {
    pub enabled: bool,
    pub position: WidgetPosition,
    /// Last dragged top-left (DIP), used when `position == remember`. Serialized as `null` when unset.
    pub custom_x: Option<i32>,
    pub custom_y: Option<i32>,
    pub auto_hide: bool,
    pub highlight_words: bool,
    pub compact: bool,
}

impl Default for WidgetSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            position: WidgetPosition::BottomRight,
            custom_x: None,
            custom_y: None,
            auto_hide: true,
            highlight_words: true,
            compact: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    pub launch_at_login: bool,
    pub start_minimized: bool,
    pub close_to_tray: bool,
    pub theme: ThemeMode,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self { launch_at_login: false, start_minimized: false, close_to_tray: true, theme: ThemeMode::System }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LibrarySettings {
    pub save_history: bool,
    pub store_text: bool,
    pub cache_audio: bool,
    /// Decimal MB, 50..10 000.
    #[serde(rename = "cacheLimitMB")]
    pub cache_limit_mb: u32,
    /// 0 = keep forever; 0..3650.
    pub retention_days: u32,
}

impl Default for LibrarySettings {
    fn default() -> Self {
        Self { save_history: true, store_text: true, cache_audio: true, cache_limit_mb: 500, retention_days: 0 }
    }
}

impl LibrarySettings {
    /// Cache limit in bytes (`cacheLimitMB × 1 000 000`).
    pub fn cache_limit_bytes(&self) -> u64 {
        self.cache_limit_mb as u64 * CACHE_MB
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextSettings {
    pub skip_urls: bool,
    pub skip_emails: bool,
    /// 100..100 000 UTF-16 units.
    pub max_chars: u32,
}

impl Default for TextSettings {
    fn default() -> Self {
        Self { skip_urls: true, skip_emails: false, max_chars: 20_000 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerSettings {
    /// 1024..65 535.
    pub port: u16,
}

impl Default for ServerSettings {
    fn default() -> Self {
        Self { port: LOCAL_SERVER_DEFAULT_PORT }
    }
}

/// The whole settings object. Serializes (field order = this struct) to the stored `settings.json` form: every field
/// except the derived `has_api_key`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// Always 1.
    pub version: u32,
    /// Derived: an API key is stored (or came from `ELEVENLABS_API_KEY`). Never persisted.
    #[serde(skip)]
    pub has_api_key: bool,
    pub onboarded: bool,
    pub voice_id: String,
    pub voice_name: String,
    pub model_id: String,
    pub output_format: OutputFormat,
    pub voice: VoiceSettings,
    /// ISO 639-1 passed to ElevenLabs; `""` = auto-detect.
    pub language_code: String,
    /// Client-side rate 0.5..3 (pitch preserved).
    #[serde(serialize_with = "js_f64")]
    pub playback_rate: f64,
    /// 0..1.
    #[serde(serialize_with = "js_f64")]
    pub volume: f64,
    pub triggers: TriggerSettings,
    pub widget: WidgetSettings,
    pub app: AppSettings,
    pub library: LibrarySettings,
    pub text: TextSettings,
    pub server: ServerSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: 1,
            has_api_key: false,
            onboarded: false,
            voice_id: ELEVENLABS_DEFAULT_VOICE_ID.into(),
            voice_name: ELEVENLABS_DEFAULT_VOICE_NAME.into(),
            model_id: ELEVENLABS_DEFAULT_MODEL_ID.into(),
            output_format: OutputFormat::Mp3_44100_128,
            voice: VoiceSettings::default(),
            language_code: String::new(),
            playback_rate: 1.0,
            volume: 1.0,
            triggers: TriggerSettings::default(),
            widget: WidgetSettings::default(),
            app: AppSettings::default(),
            library: LibrarySettings::default(),
            text: TextSettings::default(),
            server: ServerSettings::default(),
        }
    }
}

// ───────────────────────────── sanitize ─────────────────────────────

fn at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut cur = v;
    for k in path {
        cur = cur.get(*k)?;
    }
    Some(cur)
}

fn bool_or(v: Option<&Value>, fb: bool) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        _ => fb,
    }
}

/// `num(v, fallback, min, max, integer)`: a JSON number else fallback; integers are `Math.round`ed before clamping.
fn num_or(v: Option<&Value>, fb: f64, min: f64, max: f64, integer: bool) -> f64 {
    match v {
        Some(Value::Number(n)) => match n.as_f64().filter(|x| x.is_finite()) {
            Some(x) => {
                let x = if integer { js_round(x) } else { x };
                x.max(min).min(max)
            }
            None => fb,
        },
        _ => fb,
    }
}

/// `nonEmpty`: string → JS-trim → first `max` UTF-16 units; empty → fallback.
fn non_empty_or(v: Option<&Value>, fb: &str, max: usize) -> String {
    match v {
        Some(Value::String(s)) => {
            let t = prefix16(js_trim(s), max);
            if t.is_empty() {
                fb.to_string()
            } else {
                t.to_string()
            }
        }
        _ => fb.to_string(),
    }
}

static LANGUAGE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z]{2,3}([-_][a-z0-9]{2,8})?$").expect("valid regex"));

/// `languageCode`: trimmed + lower-cased; `""` allowed; else must match `^[a-z]{2,3}([-_][a-z0-9]{2,8})?$`.
pub fn sanitize_language_code(v: Option<&Value>, fb: &str) -> String {
    match v {
        Some(Value::String(s)) => {
            let t = js_trim(s).to_lowercase();
            if t.is_empty() || LANGUAGE_RE.is_match(&t) {
                t
            } else {
                fb.to_string()
            }
        }
        _ => fb.to_string(),
    }
}

/// A hotkey string: trimmed, first 64 units (`""` = disabled); non-strings fall back.
fn hotkey_or(v: Option<&Value>, fb: &str) -> String {
    match v {
        Some(Value::String(s)) => prefix16(js_trim(s), 64).to_string(),
        _ => fb.to_string(),
    }
}

/// `customX/customY`: `null` → unset; a number → clamped ±100 000 then `Math.round`; else fallback.
fn coordinate_or(v: Option<&Value>, fb: Option<i32>) -> Option<i32> {
    match v {
        Some(Value::Null) => None,
        Some(Value::Number(n)) => match n.as_f64().filter(|x| x.is_finite()) {
            Some(x) => Some(js_round(x.clamp(-CUSTOM_COORD_LIMIT, CUSTOM_COORD_LIMIT)) as i32),
            None => fb,
        },
        _ => fb,
    }
}

fn one_of<T: Copy>(v: Option<&Value>, parse: fn(&str) -> Option<T>, fb: T) -> T {
    match v {
        Some(Value::String(s)) => parse(s).unwrap_or(fb),
        _ => fb,
    }
}

/// `normalizeExcludedApps`: not an array → fallback. Per string item: JS-trim, lower-case, last path segment
/// (split on `\` or `/`), trim, skip empty, append `.exe` if missing, de-duplicate (first kept), stop at 200.
pub fn normalize_excluded_apps(v: Option<&Value>, fb: &[String]) -> Vec<String> {
    let Some(Value::Array(items)) = v else { return fb.to_vec() };
    let mut out: Vec<String> = Vec::new();
    for item in items {
        let Value::String(s) = item else { continue };
        let lower = js_trim(s).to_lowercase();
        let last = lower.rsplit(['\\', '/']).next().unwrap_or("");
        let mut name = js_trim(last).to_string();
        if name.is_empty() {
            continue;
        }
        if !name.ends_with(".exe") {
            name.push_str(".exe");
        }
        if !out.contains(&name) {
            out.push(name);
        }
        if out.len() >= EXCLUDED_APPS_MAX {
            break;
        }
    }
    out
}

impl Settings {
    /// Rebuilds a complete, valid `Settings` from untrusted JSON. Missing/invalid fields take the value from `base`;
    /// unknown keys are dropped; `version` is forced to 1; `has_api_key` is the given derived value.
    pub fn sanitize(input: &Value, base: &Settings, has_api_key: bool) -> Settings {
        let b = base;
        let g = |path: &[&str]| at(input, path);
        let bh = &b.triggers.hotkeys;
        Settings {
            version: 1,
            has_api_key,
            onboarded: bool_or(g(&["onboarded"]), b.onboarded),
            voice_id: non_empty_or(g(&["voiceId"]), &b.voice_id, 200),
            voice_name: non_empty_or(g(&["voiceName"]), &b.voice_name, 200),
            model_id: non_empty_or(g(&["modelId"]), &b.model_id, 200),
            output_format: one_of(g(&["outputFormat"]), OutputFormat::parse, b.output_format),
            voice: VoiceSettings {
                stability: num_or(g(&["voice", "stability"]), b.voice.stability, 0.0, 1.0, false),
                similarity_boost: num_or(g(&["voice", "similarityBoost"]), b.voice.similarity_boost, 0.0, 1.0, false),
                style: num_or(g(&["voice", "style"]), b.voice.style, 0.0, 1.0, false),
                use_speaker_boost: bool_or(g(&["voice", "useSpeakerBoost"]), b.voice.use_speaker_boost),
                speed: num_or(g(&["voice", "speed"]), b.voice.speed, 0.7, 1.2, false),
            },
            language_code: sanitize_language_code(g(&["languageCode"]), &b.language_code),
            playback_rate: num_or(g(&["playbackRate"]), b.playback_rate, RATE_MIN, RATE_MAX, false),
            volume: num_or(g(&["volume"]), b.volume, 0.0, 1.0, false),
            triggers: TriggerSettings {
                right_click_chip: bool_or(g(&["triggers", "rightClickChip"]), b.triggers.right_click_chip),
                selection_popup: bool_or(g(&["triggers", "selectionPopup"]), b.triggers.selection_popup),
                hotkeys_enabled: bool_or(g(&["triggers", "hotkeysEnabled"]), b.triggers.hotkeys_enabled),
                hotkeys: HotkeySettings {
                    read_selection: hotkey_or(g(&["triggers", "hotkeys", "readSelection"]), &bh.read_selection),
                    read_clipboard: hotkey_or(g(&["triggers", "hotkeys", "readClipboard"]), &bh.read_clipboard),
                    play_pause: hotkey_or(g(&["triggers", "hotkeys", "playPause"]), &bh.play_pause),
                    stop: hotkey_or(g(&["triggers", "hotkeys", "stop"]), &bh.stop),
                },
                media_keys: bool_or(g(&["triggers", "mediaKeys"]), b.triggers.media_keys),
                browser_extension: bool_or(g(&["triggers", "browserExtension"]), b.triggers.browser_extension),
                explorer_menu: bool_or(g(&["triggers", "explorerMenu"]), b.triggers.explorer_menu),
                excluded_apps: normalize_excluded_apps(g(&["triggers", "excludedApps"]), &b.triggers.excluded_apps),
                restore_clipboard: bool_or(g(&["triggers", "restoreClipboard"]), b.triggers.restore_clipboard),
            },
            widget: WidgetSettings {
                enabled: bool_or(g(&["widget", "enabled"]), b.widget.enabled),
                position: one_of(g(&["widget", "position"]), WidgetPosition::parse, b.widget.position),
                custom_x: coordinate_or(g(&["widget", "customX"]), b.widget.custom_x),
                custom_y: coordinate_or(g(&["widget", "customY"]), b.widget.custom_y),
                auto_hide: bool_or(g(&["widget", "autoHide"]), b.widget.auto_hide),
                highlight_words: bool_or(g(&["widget", "highlightWords"]), b.widget.highlight_words),
                compact: bool_or(g(&["widget", "compact"]), b.widget.compact),
            },
            app: AppSettings {
                launch_at_login: bool_or(g(&["app", "launchAtLogin"]), b.app.launch_at_login),
                start_minimized: bool_or(g(&["app", "startMinimized"]), b.app.start_minimized),
                close_to_tray: bool_or(g(&["app", "closeToTray"]), b.app.close_to_tray),
                theme: one_of(g(&["app", "theme"]), ThemeMode::parse, b.app.theme),
            },
            library: LibrarySettings {
                save_history: bool_or(g(&["library", "saveHistory"]), b.library.save_history),
                store_text: bool_or(g(&["library", "storeText"]), b.library.store_text),
                cache_audio: bool_or(g(&["library", "cacheAudio"]), b.library.cache_audio),
                cache_limit_mb: num_or(g(&["library", "cacheLimitMB"]), b.library.cache_limit_mb as f64, 50.0, 10_000.0, true)
                    as u32,
                retention_days: num_or(g(&["library", "retentionDays"]), b.library.retention_days as f64, 0.0, 3650.0, true)
                    as u32,
            },
            text: TextSettings {
                skip_urls: bool_or(g(&["text", "skipUrls"]), b.text.skip_urls),
                skip_emails: bool_or(g(&["text", "skipEmails"]), b.text.skip_emails),
                max_chars: num_or(g(&["text", "maxChars"]), b.text.max_chars as f64, 100.0, 100_000.0, true) as u32,
            },
            server: ServerSettings {
                port: num_or(g(&["server", "port"]), b.server.port as f64, 1024.0, 65_535.0, true) as u16,
            },
        }
    }

    /// Settings loaded from disk: sanitized against the defaults.
    pub fn from_stored(raw: &Value, has_api_key: bool) -> Settings {
        Self::sanitize(raw, &Settings::default(), has_api_key)
    }

    /// The stored form as a JSON value (no `hasApiKey`).
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("settings always serialize")
    }

    /// `settings.json` text: `JSON.stringify(stored, null, 2)`.
    pub fn stored_json(&self) -> String {
        to_js_pretty(self)
    }

    /// True if the file a value was loaded from differs textually (compact JSON) from what we would write, so a
    /// rewrite should be scheduled (persistence.md §3.4: migration = normalise + rewrite once).
    pub fn needs_rewrite(&self, raw: &Value) -> bool {
        to_js_compact(raw) != to_js_compact(self)
    }

    /// `update(patch)`: `sanitize(deepMerge(current, patch), base = current)`. Invalid patch values keep the current
    /// value. The caller compares the result with `self` to detect a no-op.
    pub fn apply_patch(&self, patch: &Value) -> Settings {
        Self::sanitize(&deep_merge(&self.to_value(), patch), self, self.has_api_key)
    }
}

/// `deepMerge(base, patch)`: plain objects merge recursively; arrays, `null` and primitives replace; keys
/// `__proto__`, `constructor`, `prototype` are ignored. A non-object patch replaces the whole value.
pub fn deep_merge(base: &Value, patch: &Value) -> Value {
    let Value::Object(p) = patch else { return patch.clone() };
    let mut out = match base {
        Value::Object(b) => b.clone(),
        _ => Map::new(),
    };
    for (k, v) in p {
        if k == "__proto__" || k == "constructor" || k == "prototype" {
            continue;
        }
        let merged = match (v, out.get(k)) {
            (Value::Object(_), Some(existing @ Value::Object(_))) => deep_merge(existing, v),
            _ => v.clone(),
        };
        out.insert(k.clone(), merged);
    }
    Value::Object(out)
}

/// Builds a nested JSON patch from a dotted key path: `patch_at("triggers.hotkeys.stop", json!(""))` →
/// `{"triggers":{"hotkeys":{"stop":""}}}`. Used by the UI's generic setting callbacks.
pub fn patch_at(path: &str, value: Value) -> Value {
    let mut v = value;
    for key in path.rsplit('.') {
        let mut m = Map::new();
        m.insert(key.to_string(), v);
        v = Value::Object(m);
    }
    v
}

/// Snaps a Slint slider value (`float` = `f32`) to its step before it becomes a settings patch or a player command
/// (contracts.md §3.2/§4.1 slider steps; ARCHITECTURE.md §7.1): `js_round(value / step) × step`, then rounded to the
/// step's number of decimals, so `0.8f32` (0.800000011920929…) becomes exactly `0.8` and the no-op check / the
/// optimistic overlay compare equal values. Steps: `volume` and the 0..1 voice sliders 0.01, `voice.speed` its slider
/// step (0.01), `library.cacheLimitMB` 50 (an integer). A non-finite value or a step ≤ 0 returns the value unchanged
/// (the settings sanitizer then rejects a non-finite one).
pub fn quantize(value: f32, step: f64) -> f64 {
    let v = value as f64;
    if !v.is_finite() || step.is_nan() || step <= 0.0 || !step.is_finite() {
        return v;
    }
    let snapped = js_round(v / step) * step;
    let decimals = {
        let s = format!("{step}");
        s.split_once('.').map(|(_, f)| f.len().min(12)).unwrap_or(0)
    };
    let p = 10f64.powi(decimals as i32);
    let r = (snapped * p).round() / p;
    if r == 0.0 {
        0.0
    } else {
        r
    }
}

/// Whether hotkey registration must re-run: `hotkeysEnabled` or any of the four **raw** strings changed.
pub fn hotkeys_changed(a: &Settings, b: &Settings) -> bool {
    a.triggers.hotkeys_enabled != b.triggers.hotkeys_enabled || a.triggers.hotkeys != b.triggers.hotkeys
}

// ───────────────────────────── Settings page helpers ─────────────────────────────

/// Settings section ids, in nav order, with their labels (ui-settings-widget.md §2.1).
pub const SETTINGS_SECTIONS: [(&str, &str); 9] = [
    ("account", "Account"),
    ("voice", "Voice & model"),
    ("playback", "Playback"),
    ("triggers", "Triggers"),
    ("widget", "Widget"),
    ("appearance", "Appearance"),
    ("library", "Library & privacy"),
    ("integrations", "Integrations"),
    ("about", "About"),
];

/// True for a known settings section id.
pub fn is_settings_section(id: &str) -> bool {
    SETTINGS_SECTIONS.iter().any(|(s, _)| *s == id)
}

/// Playback-speed options: the presets plus the current rate when it is finite and not within 0.001 of a preset;
/// ascending.
pub fn playback_rate_options(current: f64) -> Vec<f64> {
    let mut v: Vec<f64> = PLAYBACK_RATES.to_vec();
    if current.is_finite() && !v.iter().any(|r| (r - current).abs() < 0.001) {
        v.push(current);
    }
    v.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    v
}

/// Index of the option within 0.001 of `current`, if any.
pub fn rate_option_index(options: &[f64], current: f64) -> Option<usize> {
    options.iter().position(|r| (r - current).abs() < 0.001)
}

/// Accessible label `"1.25 times speed"`.
pub fn rate_accessible_label(rate: f64) -> String {
    format!("{} times speed", fixed_trimmed(rate, 2))
}

pub const MAX_CHAR_PRESETS: [u32; 6] = [2_000, 5_000, 10_000, 20_000, 50_000, 100_000];

/// "Longest selection" options: presets plus the current value if different, ascending.
pub fn max_chars_options(current: u32) -> Vec<u32> {
    let mut v = MAX_CHAR_PRESETS.to_vec();
    if !v.contains(&current) {
        v.push(current);
    }
    v.sort_unstable();
    v
}

/// `20,000 characters`.
pub fn max_chars_label(n: u32) -> String {
    format!("{} characters", crate::format::format_number(n as f64))
}

/// Retention presets: (days, label).
pub const RETENTION_PRESETS: [(u32, &str); 5] = [(0, "Keep forever"), (7, "7 days"), (30, "30 days"), (90, "90 days"), (365, "1 year")];

/// Retention options plus the current custom value as `"{n} days"`.
pub fn retention_options(current: u32) -> Vec<(u32, String)> {
    let mut v: Vec<(u32, String)> = RETENTION_PRESETS.iter().map(|(d, l)| (*d, l.to_string())).collect();
    if !v.iter().any(|(d, _)| *d == current) {
        v.push((current, format!("{current} days")));
    }
    v
}

/// `'1\u{a0}year'`, `'7\u{a0}days'`, `'1\u{a0}day'` (no-break space).
pub fn retention_label(days: u32) -> String {
    if days == 365 {
        "1\u{00A0}year".into()
    } else {
        format!("{days}\u{00A0}day{}", if days == 1 { "" } else { "s" })
    }
}

/// Switching retention from `current` to `next` can delete reads right away.
pub fn is_stricter_retention(next: u32, current: u32) -> bool {
    next > 0 && (current == 0 || next < current)
}

/// The Settings page's strict exe-name check (ui-settings-widget.md §2.5): `None` = invalid.
pub fn normalize_exe_name(input: &str) -> Option<String> {
    let trimmed = js_trim(input).trim_matches('"');
    let trimmed = js_trim(trimmed);
    let last = trimmed.rsplit(['\\', '/']).next().unwrap_or("");
    let mut name = js_trim(last).to_lowercase();
    if name.is_empty() {
        return None;
    }
    if !name.ends_with(".exe") {
        name.push_str(".exe");
    }
    let bad_char = name.chars().any(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*') || (c as u32) < 0x20);
    let bad_tail = name.ends_with("..exe") || name.ends_with(" .exe");
    if bad_char || name == ".exe" || bad_tail {
        return None;
    }
    (crate::text16::utf16_len(&name) <= 120).then_some(name)
}

/// Adds a program to the excluded list; `Err` carries the UI error text.
pub fn add_excluded_app(list: &[String], input: &str) -> Result<Vec<String>, String> {
    let Some(name) = normalize_exe_name(input) else {
        return Err("Enter a program name like notepad.exe".into());
    };
    if list.contains(&name) {
        return Err(format!("{name} is already excluded"));
    }
    let mut out = list.to_vec();
    out.push(name);
    Ok(out)
}

/// A user-typed port: trimmed `^\d{1,5}$` within 1024..65535.
pub fn parse_port(text: &str) -> Option<u16> {
    let t = text.trim();
    if t.is_empty() || t.len() > 5 || !t.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let n: u32 = t.parse().ok()?;
    (1024..=65_535).contains(&n).then_some(n as u16)
}

/// `{n}× credits` for a cost factor (`None` when unknown).
pub fn cost_label(factor: Option<f64>) -> Option<String> {
    let f = factor.filter(|f| f.is_finite() && *f > 0.0)?;
    Some(format!("{}\u{00D7} credits", fixed_trimmed(f, 2)))
}

/// Model select options `(value, label)`; the current id is prepended (prettified) when the list lacks it.
pub fn model_options(models: &[TtsModel], current: &str) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = models.iter().map(|m| (m.model_id.clone(), crate::format::model_name(&m.model_id, models))).collect();
    if !current.is_empty() && !v.iter().any(|(id, _)| id == current) {
        v.insert(0, (current.to_string(), crate::format::model_name(current, models)));
    }
    v
}

/// Language options: `('', "Auto-detect")`, then the model's languages sorted by name; an unknown current code is
/// appended upper-cased.
pub fn language_options(models: &[TtsModel], model_id: &str, current: &str) -> Vec<(String, String)> {
    let mut langs: Vec<(String, String)> = models
        .iter()
        .find(|m| m.model_id == model_id)
        .map(|m| m.languages.iter().map(|l| (l.id.clone(), l.name.clone())).collect())
        .unwrap_or_default();
    langs.sort_by_key(|l| l.1.to_lowercase());
    let mut v = vec![(String::new(), "Auto-detect".to_string())];
    v.extend(langs);
    if !current.is_empty() && !v.iter().any(|(id, _)| id == current) {
        v.push((current.to_string(), current.to_uppercase()));
    }
    v
}

/// The patch written when the model changes: also resets a pinned language the new (known) model lacks.
pub fn model_change_patch(models: &[TtsModel], new_model: &str, current_language: &str) -> Value {
    let mut m = Map::new();
    m.insert("modelId".into(), Value::String(new_model.into()));
    if !current_language.is_empty() {
        if let Some(model) = models.iter().find(|x| x.model_id == new_model) {
            if !model.languages.is_empty() && !model.languages.iter().any(|l| l.id == current_language) {
                m.insert("languageCode".into(), Value::String(String::new()));
            }
        }
    }
    Value::Object(m)
}

/// Meta badges under the model row: cost (flagged when > 1), max chars per request, language count.
pub fn model_badges(model: &TtsModel) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    if let Some(c) = cost_label(model.cost_factor) {
        out.push((c, model.cost_factor.unwrap_or(0.0) > 1.0));
    }
    if let Some(mc) = model.max_chars {
        out.push((format!("{}K chars / request", js_round(mc / 1000.0)), false));
    }
    if model.languages.len() > 1 {
        out.push((format!("{} languages", model.languages.len()), false));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn slider_values_are_quantized() {
        assert_eq!(quantize(0.8f32, 0.01), 0.8);
        assert_eq!(quantize(0.65f32, 0.05), 0.65);
        assert_eq!(quantize(0.3f32, 0.01), 0.3);
        assert_eq!(quantize(1.15f32, 0.01), 1.15);
        assert_eq!(quantize(0.004f32, 0.01), 0.0);
        assert_eq!(quantize(1037.0f32, 50.0), 1050.0);
        assert_eq!(quantize(1024.9f32, 50.0), 1000.0);
        assert!(quantize(f32::NAN, 0.01).is_nan());
        assert_eq!(quantize(0.5f32, 0.0), 0.5);
        // The quantized value is what lands in the patch (and compares equal to the stored f64).
        assert_eq!(patch_at("volume", json!(quantize(0.8f32, 0.01))), json!({"volume": 0.8}));
    }

    const DEFAULT_FILE: &str = r#"{
  "version": 1,
  "onboarded": false,
  "voiceId": "JBFqnCBsd6RMkjVDRZzb",
  "voiceName": "George",
  "modelId": "eleven_flash_v2_5",
  "outputFormat": "mp3_44100_128",
  "voice": {
    "stability": 0.5,
    "similarityBoost": 0.75,
    "style": 0,
    "useSpeakerBoost": true,
    "speed": 1
  },
  "languageCode": "",
  "playbackRate": 1,
  "volume": 1,
  "triggers": {
    "rightClickChip": true,
    "selectionPopup": false,
    "hotkeysEnabled": true,
    "hotkeys": {
      "readSelection": "Ctrl+Alt+R",
      "readClipboard": "Ctrl+Alt+V",
      "playPause": "Ctrl+Alt+P",
      "stop": "Ctrl+Alt+S"
    },
    "mediaKeys": true,
    "browserExtension": true,
    "explorerMenu": false,
    "excludedApps": [],
    "restoreClipboard": true
  },
  "widget": {
    "enabled": true,
    "position": "bottom-right",
    "customX": null,
    "customY": null,
    "autoHide": true,
    "highlightWords": true,
    "compact": false
  },
  "app": {
    "launchAtLogin": false,
    "startMinimized": false,
    "closeToTray": true,
    "theme": "system"
  },
  "library": {
    "saveHistory": true,
    "storeText": true,
    "cacheAudio": true,
    "cacheLimitMB": 500,
    "retentionDays": 0
  },
  "text": {
    "skipUrls": true,
    "skipEmails": false,
    "maxChars": 20000
  },
  "server": {
    "port": 47823
  }
}"#;

    #[test]
    fn defaults_serialize_byte_identical_to_electron() {
        assert_eq!(Settings::default().stored_json(), DEFAULT_FILE);
        let raw = crate::json::parse_lenient(DEFAULT_FILE).unwrap();
        let s = Settings::from_stored(&raw, false);
        assert_eq!(s, Settings::default());
        assert!(!s.needs_rewrite(&raw));
    }

    #[test]
    fn round_trip_customised_file() {
        let text = DEFAULT_FILE
            .replace("\"playbackRate\": 1", "\"playbackRate\": 1.25")
            .replace("\"customX\": null", "\"customX\": -1200")
            .replace("\"excludedApps\": []", "\"excludedApps\": [\n      \"photoshop.exe\"\n    ]")
            .replace("\"theme\": \"system\"", "\"theme\": \"night\"");
        let raw = crate::json::parse_lenient(&text).unwrap();
        let s = Settings::from_stored(&raw, true);
        assert_eq!(s.playback_rate, 1.25);
        assert_eq!(s.widget.custom_x, Some(-1200));
        assert_eq!(s.app.theme, ThemeMode::Night);
        assert!(s.has_api_key);
        assert_eq!(s.stored_json(), text);
        assert!(!s.needs_rewrite(&raw));
    }

    #[test]
    fn migration_rewrites_when_different() {
        // unknown key, stray hasApiKey, out-of-range values, wrong types, other key order
        let raw = json!({
            "hasApiKey": true, "volume": 7, "playbackRate": "fast", "zzz": 1,
            "library": {"cacheLimitMB": 20000.4, "retentionDays": -3},
            "text": {"maxChars": 99.5}, "server": {"port": 80},
            "widget": {"customX": -100.5, "customY": 1e9, "position": "left"},
            "triggers": null, "voiceId": "  ", "languageCode": " EN-us ",
            "voice": {"speed": 2, "stability": true}
        });
        let s = Settings::from_stored(&raw, false);
        assert_eq!(s.volume, 1.0);
        assert_eq!(s.playback_rate, 1.0);
        assert_eq!(s.library.cache_limit_mb, 10_000);
        assert_eq!(s.library.retention_days, 0);
        assert_eq!(s.text.max_chars, 100);
        assert_eq!(s.server.port, 1024);
        assert_eq!(s.widget.custom_x, Some(-100)); // JS Math.round(-100.5) = -100
        assert_eq!(s.widget.custom_y, Some(100_000));
        assert_eq!(s.widget.position, WidgetPosition::BottomRight);
        assert_eq!(s.triggers, TriggerSettings::default());
        assert_eq!(s.voice_id, ELEVENLABS_DEFAULT_VOICE_ID);
        assert_eq!(s.language_code, "en-us");
        assert_eq!(s.voice.speed, 1.2);
        assert_eq!(s.voice.stability, 0.5);
        assert!(s.needs_rewrite(&raw));
        assert!(!s.stored_json().contains("hasApiKey"));
    }

    #[test]
    fn patch_semantics() {
        let cur = Settings { has_api_key: true, ..Settings::default() };
        // invalid value in a patch keeps the CURRENT value (not the default)
        let cur2 = cur.apply_patch(&json!({"playbackRate": 1.5}));
        assert_eq!(cur2.playback_rate, 1.5);
        let same = cur2.apply_patch(&json!({"playbackRate": "x", "triggers": null}));
        assert_eq!(same, cur2);
        assert!(same.has_api_key);
        // nested merge keeps siblings, arrays replace
        let s = cur2.apply_patch(&json!({"triggers": {"hotkeys": {"stop": " Ctrl+Alt+Q "}, "excludedApps": ["C:\\Tools\\Foo"]}}));
        assert_eq!(s.triggers.hotkeys.stop, "Ctrl+Alt+Q");
        assert_eq!(s.triggers.hotkeys.read_selection, "Ctrl+Alt+R");
        assert_eq!(s.triggers.excluded_apps, vec!["foo.exe".to_string()]);
        // null clears a coordinate
        let w = s.apply_patch(&json!({"widget": {"position": "remember", "customX": 10, "customY": 20}}));
        assert_eq!((w.widget.custom_x, w.widget.custom_y), (Some(10), Some(20)));
        assert_eq!(w.apply_patch(&json!({"widget": {"customX": null}})).widget.custom_x, None);
        assert_eq!(patch_at("widget.compact", json!(true)), json!({"widget": {"compact": true}}));
        assert!(hotkeys_changed(&cur2, &s));
    }

    #[test]
    fn deep_merge_rules() {
        assert_eq!(deep_merge(&json!({"a": {"b": 1, "c": 2}}), &json!({"a": {"b": 3}})), json!({"a": {"b": 3, "c": 2}}));
        assert_eq!(deep_merge(&json!({"a": [1, 2]}), &json!({"a": [3]})), json!({"a": [3]}));
        assert_eq!(deep_merge(&json!({"a": 1}), &json!({"__proto__": {"x": 1}})), json!({"a": 1}));
        assert_eq!(deep_merge(&json!({"a": 1}), &json!(5)), json!(5));
    }

    #[test]
    fn excluded_apps_normalisation() {
        let v = json!(["  Photoshop.EXE ", "C:\\Apps\\slack", "a/b/c", "", 5, "slack.exe", "soffice.bin"]);
        assert_eq!(
            normalize_excluded_apps(Some(&v), &[]),
            vec!["photoshop.exe", "slack.exe", "c.exe", "soffice.bin.exe"]
        );
        assert_eq!(normalize_exe_name("\"C:\\Program Files\\Foo\\Bar.EXE\""), Some("bar.exe".into()));
        assert_eq!(normalize_exe_name("con .exe"), None);
        assert_eq!(normalize_exe_name("a?b"), None);
        assert_eq!(normalize_exe_name("slack"), Some("slack.exe".into()));
        assert_eq!(add_excluded_app(&["slack.exe".into()], "Slack").unwrap_err(), "slack.exe is already excluded");
    }

    #[test]
    fn option_builders() {
        assert_eq!(playback_rate_options(2.5), vec![0.75, 1.0, 1.25, 1.5, 1.75, 2.0, 2.5]);
        assert_eq!(playback_rate_options(1.0005).len(), 6);
        assert_eq!(max_chars_options(12_345)[3], 12_345);
        assert_eq!(max_chars_label(20_000), "20,000 characters");
        assert_eq!(retention_options(14).last().unwrap().1, "14 days");
        assert_eq!(retention_label(365), "1\u{a0}year");
        assert!(is_stricter_retention(7, 0));
        assert!(!is_stricter_retention(0, 7));
        assert!(!is_stricter_retention(30, 7));
        assert_eq!(parse_port("00080"), None);
        assert_eq!(parse_port(" 47823 "), Some(47823));
        assert_eq!(parse_port("123456"), None);
        assert_eq!(cost_label(Some(0.5)).unwrap(), "0.5\u{d7} credits");
        assert_eq!(rate_accessible_label(1.25), "1.25 times speed");
    }
}
