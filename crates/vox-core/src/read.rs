//! Read requests, results and error codes (contracts.md §3.1, §3.4, §5.3; reader-playback.md §2).

use serde::Serialize;

use crate::constants::{BRIDGE_TITLE_MAX, HTTP_URL_MAX};
use crate::history::ReadSource;
use crate::js::js_trim;
use crate::text16::{prefix16, utf16_len};

/// Why a read failed. Serialized kebab-case (`"no-api-key"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReadErrorCode {
    NoApiKey,
    Unauthorized,
    Quota,
    RateLimit,
    Network,
    EmptyText,
    NoSelection,
    TooLong,
    NotCached,
    Unknown,
}

impl ReadErrorCode {
    pub const ALL: [ReadErrorCode; 10] = [
        Self::NoApiKey,
        Self::Unauthorized,
        Self::Quota,
        Self::RateLimit,
        Self::Network,
        Self::EmptyText,
        Self::NoSelection,
        Self::TooLong,
        Self::NotCached,
        Self::Unknown,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoApiKey => "no-api-key",
            Self::Unauthorized => "unauthorized",
            Self::Quota => "quota",
            Self::RateLimit => "rate-limit",
            Self::Network => "network",
            Self::EmptyText => "empty-text",
            Self::NoSelection => "no-selection",
            Self::TooLong => "too-long",
            Self::NotCached => "not-cached",
            Self::Unknown => "unknown",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == s)
    }
    /// The generic per-code fallback (reader.ts `ERROR_MESSAGES`), used only when an error has no message.
    pub fn default_message(self) -> Option<&'static str> {
        match self {
            Self::Unauthorized => Some("ElevenLabs rejected your API key"),
            Self::Quota => Some("Your ElevenLabs character quota is used up"),
            Self::RateLimit => Some("ElevenLabs is busy right now. Try again in a moment"),
            Self::Network => Some("Couldn’t reach ElevenLabs. Check your connection"),
            Self::TooLong => Some("That selection is too long for the current model"),
            _ => None,
        }
    }
    /// Codes the user fixes in Settings → Account.
    pub fn is_settings_error(self) -> bool {
        matches!(self, Self::NoApiKey | Self::Unauthorized | Self::Quota)
    }
    /// Codes the widget offers "Try again" for.
    pub fn is_retry_error(self) -> bool {
        matches!(self, Self::Network | Self::RateLimit | Self::Unknown)
    }
}

/// Exact user-facing ElevenLabs messages (tts.md §2.4.2). `—` is U+2014; apostrophes are ASCII. These travel as
/// read errors, session errors, key-validation results and toasts; `BAD_KEY` identity decides whether a toast offers
/// "Open settings".
pub mod eleven_messages {
    pub const NO_KEY: &str = "Add your ElevenLabs API key in Settings to start reading";
    pub const BAD_KEY: &str = "Your ElevenLabs key was rejected \u{2014} update it in Settings";
    pub const QUOTA: &str = "You’re out of ElevenLabs credits \u{2014} top up your plan or wait for the monthly reset";
    pub const RATE_LIMIT: &str = "ElevenLabs is busy right now \u{2014} try again in a moment";
    pub const CONCURRENCY: &str = "Too many requests at once for your ElevenLabs plan \u{2014} try again in a moment";
    pub const NETWORK: &str = "Couldn’t reach ElevenLabs \u{2014} check your internet connection";
    pub const TIMEOUT: &str = "ElevenLabs took too long to respond \u{2014} try again";
    pub const SERVER: &str = "ElevenLabs is having trouble right now \u{2014} try again in a moment";
    pub const VOICE_MISSING: &str = "That voice is no longer available \u{2014} pick another one in Voices";
    pub const VOICE_DENIED: &str = "That voice isn’t available on your ElevenLabs plan \u{2014} pick another one in Voices";
    pub const MODEL: &str = "The selected model isn’t available for your key \u{2014} choose another in Settings";
    pub const TOO_LONG: &str = "This passage is too long for the selected model";
    pub const BAD_RESPONSE: &str = "ElevenLabs sent a response that could not be read";
    pub const NO_AUDIO: &str = "ElevenLabs returned no audio";
    pub const CANCELLED: &str = "Cancelled";
    /// Key validation copy (tts.md §2.11).
    pub const PASTE_KEY_FIRST: &str = "Paste your ElevenLabs API key first";
    pub const KEY_HAS_SPACES: &str = "That doesn’t look like an API key \u{2014} it contains spaces";
    pub const KEY_TOO_LONG: &str = "That doesn’t look like an API key";
    pub const KEY_NOT_ACCEPTED: &str = "ElevenLabs didn’t accept this key \u{2014} check that you copied all of it";
    pub const KEY_RESTRICTED_WARNING: &str = "This key can’t read your account usage, so credits won’t be shown. Reading works as long as it has Text to Speech access.";
    /// Engine-level messages.
    pub const NO_TEXT: &str = "There is no text to read";
    pub const CACHED_AUDIO_MISSING: &str = "The saved audio for this read is missing";
    pub const ENGINE_CRASHED: &str = "Something went wrong while preparing the audio";
    pub const PART_FAILED: &str = "Part of the text could not be synthesized";

    /// `Your ElevenLabs key isn't allowed to use {what} — enable it for the key on elevenlabs.io, or use another key`.
    pub fn permission(what: &str) -> String {
        format!("Your ElevenLabs key isn’t allowed to use {what} \u{2014} enable it for the key on elevenlabs.io, or use another key")
    }
}

/// `errorMessage(code, fallback)`: the error's own message trimmed if non-empty, else the per-code default, else
/// `Something went wrong while reading`.
pub fn error_message(code: ReadErrorCode, fallback: Option<&str>) -> String {
    if let Some(m) = fallback.map(js_trim).filter(|m| !m.is_empty()) {
        return m.to_string();
    }
    code.default_message().unwrap_or("Something went wrong while reading").to_string()
}

/// `errorNeedsSettings`: `no-api-key` and `quota`; `unauthorized` only when not a permission problem and the message
/// is exactly the bad-key message.
pub fn error_needs_settings(code: ReadErrorCode, message: &str, missing_permission: bool) -> bool {
    match code {
        ReadErrorCode::NoApiKey | ReadErrorCode::Quota => true,
        ReadErrorCode::Unauthorized => !missing_permission && message == eleven_messages::BAD_KEY,
        _ => false,
    }
}

/// Result of starting a read. `ok` means the read **started**; synthesis errors arrive later.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<ReadErrorCode>,
}

impl ReadResult {
    pub fn started(read_id: impl Into<String>) -> Self {
        Self { ok: true, read_id: Some(read_id.into()), error: None, code: None }
    }
    pub fn failed(code: ReadErrorCode, error: impl Into<String>) -> Self {
        Self { ok: false, read_id: None, error: Some(error.into()), code: Some(code) }
    }
    pub fn failed_with_id(read_id: impl Into<String>, code: ReadErrorCode, error: impl Into<String>) -> Self {
        Self { ok: false, read_id: Some(read_id.into()), error: Some(error.into()), code: Some(code) }
    }
    /// The canonical failure results (contracts.md §5.3).
    pub fn no_api_key() -> Self {
        Self::failed(ReadErrorCode::NoApiKey, "No ElevenLabs API key")
    }
    pub fn nothing_to_read() -> Self {
        Self::failed(ReadErrorCode::EmptyText, "Nothing to read")
    }
    pub fn clipboard_empty() -> Self {
        Self::failed(ReadErrorCode::EmptyText, "The clipboard has no text")
    }
    pub fn not_in_history() -> Self {
        Self::failed(ReadErrorCode::Unknown, "That read is no longer in your history")
    }
    pub fn not_cached(read_id: impl Into<String>) -> Self {
        Self::failed_with_id(read_id, ReadErrorCode::NotCached, "The audio is no longer cached and the full text was not saved")
    }
    pub fn still_starting() -> Self {
        Self::failed(ReadErrorCode::Unknown, "Vox is still starting")
    }
    /// The answer to a read request whose core handler panicked (ARCHITECTURE.md §8 D-ARCH-3): `unknown`,
    /// `Something went wrong`. The UI maps it to its transport-failure path (Home's `read-failed` toast).
    pub fn internal_error() -> Self {
        Self::failed(ReadErrorCode::Unknown, crate::copy::SOMETHING_WENT_WRONG)
    }
}

/// Overrides the configured voice (replays that re-synthesize).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceOverride {
    pub id: String,
    pub name: String,
}

/// What the reader is asked to read.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadRequest {
    pub text: String,
    pub source: ReadSource,
    pub source_app: Option<String>,
    pub source_process: Option<String>,
    pub source_title: Option<String>,
    pub source_url: Option<String>,
    pub voice: Option<VoiceOverride>,
}

impl ReadRequest {
    pub fn new(text: impl Into<String>, source: ReadSource) -> Self {
        Self { text: text.into(), source, source_app: None, source_process: None, source_title: None, source_url: None, voice: None }
    }
}

/// Options for a read typed or pasted in the app (`reader:read-text`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReadTextOptions {
    pub source: Option<ReadSource>,
    pub source_app: Option<String>,
    pub source_title: Option<String>,
    pub source_url: Option<String>,
}

impl ReadTextOptions {
    /// Builds the reader request with the IPC-edge rules: text cut to 2 000 000 units; default source `manual`;
    /// `sourceApp` cut to 120 (not trimmed); `sourceTitle` cut to 300; `sourceUrl` kept only if [`is_http_url`].
    pub fn into_request(self, text: &str) -> ReadRequest {
        let non_empty = |s: Option<String>, max: usize| s.filter(|x| !x.is_empty()).map(|x| prefix16(&x, max).to_string());
        ReadRequest {
            text: prefix16(text, crate::constants::MAX_READ_TEXT).to_string(),
            source: self.source.unwrap_or(ReadSource::Manual),
            source_app: non_empty(self.source_app, 120),
            source_process: None,
            source_title: non_empty(self.source_title, 300),
            source_url: self.source_url.filter(|u| is_http_url(u)),
            voice: None,
        }
    }
}

/// The app a selection came from.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ForegroundInfo {
    /// Lower-case exe basename, `""` when the process can't be queried.
    pub process_name: String,
    /// Friendly name, e.g. `Chrome`.
    pub app_name: String,
    /// Window title, whitespace-collapsed, trimmed, ≤ 300 units.
    pub title: String,
    pub pid: u32,
}

/// Processes whose foreground window labels a bridge read with the browser's own name.
pub const KNOWN_BROWSER_PROCESSES: [&str; 9] =
    ["chrome.exe", "msedge.exe", "brave.exe", "opera.exe", "vivaldi.exe", "arc.exe", "chromium.exe", "yandex.exe", "firefox.exe"];

/// A validated `POST /read` body (integrations.md §1.6.2): `url` already matched `^(https?|file)://` and ≤ 2048,
/// `title` trimmed, ≤ 300, `None` if blank.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeReadRequest {
    pub text: String,
    pub url: Option<String>,
    pub title: Option<String>,
}

impl BridgeReadRequest {
    /// The reader request (index.ts:226-245): source `browser-extension`; `sourceApp` = the foreground app only if it
    /// is a known browser, else `"Browser"`; `sourceUrl` only for http(s) URLs ≤ 4096.
    pub fn into_request(self, foreground: Option<&ForegroundInfo>) -> ReadRequest {
        let fg = foreground.filter(|f| KNOWN_BROWSER_PROCESSES.contains(&f.process_name.as_str()));
        ReadRequest {
            text: self.text,
            source: ReadSource::BrowserExtension,
            source_app: Some(fg.map(|f| f.app_name.clone()).filter(|a| !a.is_empty()).unwrap_or_else(|| "Browser".into())),
            source_process: fg.map(|f| f.process_name.clone()),
            source_title: self.title.map(|t| prefix16(&t, BRIDGE_TITLE_MAX).to_string()),
            source_url: self.url.filter(|u| is_http_url(u)),
            voice: None,
        }
    }
}

/// `isHttpUrl`: a string ≤ 4096 units that parses as an absolute URL with scheme `http` or `https`.
pub fn is_http_url(s: &str) -> bool {
    if utf16_len(s) > HTTP_URL_MAX {
        return false;
    }
    match url::Url::parse(s) {
        Ok(u) => u.scheme() == "http" || u.scheme() == "https",
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages() {
        assert_eq!(error_message(ReadErrorCode::Quota, Some("  ")), "Your ElevenLabs character quota is used up");
        assert_eq!(error_message(ReadErrorCode::Quota, Some(" Own ")), "Own");
        assert_eq!(error_message(ReadErrorCode::EmptyText, None), "Something went wrong while reading");
        assert!(error_needs_settings(ReadErrorCode::Unauthorized, eleven_messages::BAD_KEY, false));
        assert!(!error_needs_settings(ReadErrorCode::Unauthorized, eleven_messages::BAD_KEY, true));
        assert!(!error_needs_settings(ReadErrorCode::Unauthorized, "ElevenLabs: Unusual activity", false));
        assert!(error_needs_settings(ReadErrorCode::Quota, "", false));
    }

    #[test]
    fn result_json() {
        assert_eq!(crate::json::to_js_compact(&ReadResult::started("r1")), r#"{"ok":true,"readId":"r1"}"#);
        assert_eq!(
            crate::json::to_js_compact(&ReadResult::still_starting()),
            r#"{"ok":false,"error":"Vox is still starting","code":"unknown"}"#
        );
        assert_eq!(
            crate::json::to_js_compact(&ReadResult::internal_error()),
            r#"{"ok":false,"error":"Something went wrong","code":"unknown"}"#
        );
    }

    #[test]
    fn bridge_mapping() {
        let fg = ForegroundInfo { process_name: "msedge.exe".into(), app_name: "Edge".into(), title: "x".into(), pid: 1 };
        let req = BridgeReadRequest { text: "t".into(), url: Some("file:///c:/x.txt".into()), title: Some("Page".into()) };
        let r = req.clone().into_request(Some(&fg));
        assert_eq!(r.source_app.as_deref(), Some("Edge"));
        assert_eq!(r.source_process.as_deref(), Some("msedge.exe"));
        assert_eq!(r.source_url, None); // file:// accepted by the server, dropped here
        let term = ForegroundInfo { process_name: "cmd.exe".into(), app_name: "Command Prompt".into(), ..Default::default() };
        let r = req.into_request(Some(&term));
        assert_eq!(r.source_app.as_deref(), Some("Browser"));
        assert_eq!(r.source_process, None);
        assert!(is_http_url("https://example.com/a"));
        assert!(!is_http_url("example.com"));
        assert!(!is_http_url("javascript:alert(1)"));
    }
}
