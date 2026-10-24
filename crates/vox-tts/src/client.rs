//! The ElevenLabs REST client (tts.md §2; BRIEF.md Product rulings "Network"; ARCHITECTURE.md §2.4).
//!
//! Transport: one shared blocking `ureq::Agent` (rustls with **`RootCerts::PlatformVerifier`**, i.e. the Windows
//! certificate store; gzip; redirects followed), headers `accept: application/json`, `user-agent:
//! Vox/<version> (Windows)` ([`user_agent`]), `xi-api-key` only when a key exists (marked sensitive, never logged),
//! `content-type: application/json` only with a body. Proxy: [`ClientConfig::proxy`] (the Windows system proxy that
//! vox-app reads with `vox_win::registry::system_proxy()`; `None` = direct), replaceable at run time with
//! [`ElevenLabsClient::set_proxy`]. The per-request timeout bounds the whole exchange including the body; the body is
//! read before the status is checked (tts.md §2.3).
//!
//! Bodies (tts.md §2.3 steps 5–7, §2.4.3, §8; D-NET-2 makes a 2xx bad body non-retryable, so the client must be no
//! stricter than Node): the agent is built with `http_status_as_error(false)` (4xx/5xx come back as responses with
//! their bodies, so the §2.4.4 classification and the §2.6.1 parameter fallback can read them) and keeps ureq's
//! 15 s `max_idle_age`. Every body — 2xx and error bodies alike — is read as **bytes** under the request's global
//! timeout (`read_to_vec` with a 32 MiB limit), decoded with `String::from_utf8_lossy` (Node's `res.text()` is
//! lossy), and JSON bodies are parsed with `vox_core::json::parse_lenient` (lone surrogate escapes → U+FFFD; an
//! out-of-range number becomes a non-number for that field only, so `map_alignment` returns `None` exactly as with JS
//! `Infinity` and the audio is still returned). Test: a 200 synthesis body with `"\ud800"` in
//! `normalized_alignment.characters` and a `1e400` time returns the audio without alignment.
//!
//! Proxy at run time (D-NET-1): `set_proxy` swaps the agent atomically (in-flight requests keep the old one; the memos
//! and [`RejectedParams`] are untouched). vox-app re-reads `system_proxy()` on `SystemEvent::Resumed` and before a
//! read / catalog fetch when the last check is ≥ `PROXY_RECHECK_MS` (30 s) old, and calls `set_proxy` only when the
//! value changed. When a proxied request fails **before anything was sent** (any connect-phase failure, below: the
//! proxy refuses, resets, times out, answers `CONNECT` with an error status or with something that is not HTTP — a
//! SOCKS listener, say — or the tunnelled TLS handshake fails), the client retries that request once with a direct
//! connection (safe: nothing was sent) and logs `proxy <host:port> unreachable; using a direct connection` once per
//! proxy value. Only `http://` / `https://` (CONNECT) proxies are supported; any other scheme (e.g. `socks5://` from
//! `ALL_PROXY`) is ignored with a warning and requests go direct, like a proxy URL that does not parse.
//!
//! Which failures are retryable (decision D-NET-2, "Do not retry timeouts … retry only what is safe"): see
//! [`crate::error`]. The client itself never retries except the parameter fallback below.
//!
//! Transport failure classification (implementation notes): the agents are built with `Agent::with_parts` from the
//! same connector chain as ureq's default (CONNECT proxy → TCP → rustls), wrapped in [`ConnectPhase`], plus a
//! resolver wrapped the same way. Every error those raise — DNS, the TCP connect, the whole proxy `CONNECT`
//! exchange, the TLS handshake (completed inside the connector, before any request byte; rustls never sends early
//! data) — is tagged as a [`ConnectPhaseError`], so the phase is known at the source instead of being guessed from
//! an I/O error kind (a `NetworkDown` / `HostUnreachable` / reset can also hit an established connection after the
//! request was written). A failure counts as **unsent** only when it is tagged, or is a resolve/connect timeout
//! (10 s, undici's default connect timeout; it covers the proxy `CONNECT` and the TLS handshake): unsent failures are
//! [`TtsError::network_unsent`] (retryable) and trigger the direct retry, except a TLS handshake the server or a
//! middlebox rejected (certificate, protocol alert: `InvalidData` / rustls errors) — still unsent, so the direct
//! retry applies, but [`TtsError::network`] because retrying cannot help. Every untagged timeout is
//! [`TtsError::timeout`] and every untagged transport failure (anything on an established connection, a body read
//! error) is [`TtsError::network`] (not retryable): ambiguous cases are treated as possibly billed. POSTs do not
//! follow redirects (a 3xx comes back as an error response), so a connect-phase failure of a POST can only belong to
//! its one and only request; GETs follow redirects as Node does (re-sending a GET bills nothing).
//!
//! Parameter fallback (tts.md §2.6.1, changed by the Product rulings): a 400/422 that names `language_code` or the
//! context fields makes the client resend once without it (≤ 2 reductions, ≤ 3 requests) **and remember the rejection
//! for the rest of the process**: [`RejectedParams`] keyed by model id (`language_code` also by the code), so later
//! chunks and later reads never send that parameter to that model again.
//!
//! Caches (tts.md §2.10): voices 10 min, models 1 h, account 60 s, keyed by `hex(sha256(key))[0..16]` (`"anon"`
//! without a key); a non-refresh call joins an in-flight load for the same fingerprint; failures are not cached; an
//! older load never overwrites a newer one.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::{Condvar, Mutex, RwLock};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use ureq::http::{HeaderMap, HeaderValue};
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{
    ConnectProxyConnector, ConnectionDetails, Connector, NextTimeout, RustlsConnector, TcpConnector,
};
use vox_core::constants::{
    ACCOUNT_TTL_MS, LIST_TIMEOUT_MS, MAX_API_KEY_LEN, MAX_VOICE_PAGES, MODELS_TTL_MS, SYNTH_MS_PER_CHAR,
    SYNTH_TIMEOUT_MS, VOICES_TTL_MS, VOICE_PAGE_SIZE,
};
use vox_core::js::{has_js_whitespace, js_trim};
use vox_core::read::{eleven_messages as msg, ReadErrorCode};
use vox_core::settings::VoiceSettings;
use vox_core::text16::utf16_len;
use vox_core::tts::{Alignment, AudioBytes};
use vox_core::voices::{AccountInfo, ApiKeyResult, TtsModel, Voice};

use crate::cancel::CancelToken;
use crate::error::{error_from_response, int_header, TtsError, INVALID_KEY_CODES};
use crate::jsval::{js_num, str_of};
use crate::mapping::{
    decode_base64_lenient, map_account, map_alignment, map_model, map_voice, mime_for_format, sort_voices,
};

/// Reads the current API key on every call (`SettingsStore::api_key`).
pub type KeySource = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// A forward proxy for every request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyConfig {
    /// `http://host:port` (the scheme is always `http`; HTTPS goes through `CONNECT`).
    pub url: String,
    /// Host patterns that bypass the proxy (`*.corp.example`, `.example`, exact hosts; `<local>` already expanded by
    /// the caller to "hosts without a dot").
    pub bypass: Vec<String>,
    /// Bypass hosts without a dot (`<local>` in `ProxyOverride`).
    pub bypass_local: bool,
}

/// Client construction options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientConfig {
    /// `https://api.elevenlabs.io` in production; a local mock server in tests.
    pub base_url: String,
    /// [`user_agent`] in production.
    pub user_agent: String,
    /// The Windows system proxy at construction time (later changes: `ElevenLabsClient::set_proxy`), `None` = direct.
    pub proxy: Option<ProxyConfig>,
    /// Use the Windows certificate store (production) instead of the bundled WebPKI roots (tests against a local
    /// plain-HTTP server ignore this).
    pub platform_roots: bool,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self { base_url: vox_core::constants::ELEVENLABS_BASE_URL.into(), user_agent: user_agent(), proxy: None, platform_roots: true }
    }
}

/// `Vox/<version> (Windows)`.
pub fn user_agent() -> String {
    format!("{}/{} (Windows)", vox_core::constants::APP_NAME, vox_core::constants::APP_VERSION)
}

/// One synthesis request (tts.md §2.6).
#[derive(Debug, Clone, PartialEq)]
pub struct SynthesisRequest {
    pub text: String,
    pub voice_id: String,
    pub model_id: String,
    /// `output_format` query value, e.g. `mp3_44100_128`.
    pub output_format: String,
    pub voice_settings: VoiceSettings,
    pub previous_text: Option<String>,
    pub next_text: Option<String>,
    pub language_code: Option<String>,
}

/// A synthesized chunk.
#[derive(Debug, Clone, PartialEq)]
pub struct SynthesisResult {
    pub audio: AudioBytes,
    pub mime: String,
    pub alignment: Option<Alignment>,
    pub request_id: Option<String>,
    /// `x-character-count` / `character-cost` header.
    pub character_cost: Option<f64>,
}

// ───────────────────────────── rejected parameters (D-NET-3) ─────────────────────────────

/// Optional parameters a model rejected during this process (the Product ruling "remember a rejected optional
/// parameter for the rest of the session"). Thread-safe; shared by all clones of the client.
#[derive(Debug, Default)]
pub struct RejectedParams {
    /// `(model_id, language_code)` pairs.
    languages: Mutex<HashSet<(String, String)>>,
    /// Model ids that rejected `previous_text` / `next_text`.
    contexts: Mutex<HashSet<String>>,
}

/// `NO_CONTEXT_MODELS = /^eleven_v3/`: request stitching is documented as unavailable there.
fn never_sends_context(model_id: &str) -> bool {
    model_id.starts_with("eleven_v3")
}

fn non_empty(v: &Option<String>) -> bool {
    v.as_deref().is_some_and(|s| !s.is_empty())
}

impl RejectedParams {
    /// `language_code = code` was rejected for `model_id`.
    pub fn language_rejected(&self, model_id: &str, code: &str) -> bool {
        self.languages.lock().contains(&(model_id.to_string(), code.to_string()))
    }
    /// `previous_text`/`next_text` were rejected for `model_id`.
    pub fn context_rejected(&self, model_id: &str) -> bool {
        self.contexts.lock().contains(model_id)
    }
    pub fn reject_language(&self, model_id: &str, code: &str) {
        self.languages.lock().insert((model_id.to_string(), code.to_string()));
    }
    pub fn reject_context(&self, model_id: &str) {
        self.contexts.lock().insert(model_id.to_string());
    }
    /// Removes remembered rejections from a request before it is sent (and `eleven_v3*` context, tts.md §2.6.1).
    pub fn apply(&self, req: &SynthesisRequest) -> SynthesisRequest {
        let mut out = req.clone();
        if let Some(code) = out.language_code.as_deref() {
            if code.is_empty() || self.language_rejected(&out.model_id, code) {
                out.language_code = None;
            }
        }
        if never_sends_context(&out.model_id) || self.context_rejected(&out.model_id) {
            out.previous_text = None;
            out.next_text = None;
        }
        out
    }
    fn remember(&self, model_id: &str, which: &RejectedParam) {
        match which {
            RejectedParam::Language(code) => {
                self.reject_language(model_id, code);
                tracing::info!(target: "tts", "model {model_id} rejected language_code {code}; not sending it again");
            }
            RejectedParam::Context => {
                self.reject_context(model_id);
                tracing::info!(target: "tts", "model {model_id} rejected the context fields; not sending them again");
            }
        }
    }
}

// ───────────────────────────── memo caches (tts.md §2.10) ─────────────────────────────

/// One load's outcome, shared with the callers that joined it.
struct Slot<T> {
    result: Mutex<Option<Result<T, TtsError>>>,
    ready: Condvar,
}

impl<T: Clone> Slot<T> {
    fn new() -> Self {
        Self { result: Mutex::new(None), ready: Condvar::new() }
    }
    fn fill(&self, r: Result<T, TtsError>) {
        let mut g = self.result.lock();
        if g.is_none() {
            *g = Some(r);
        }
        self.ready.notify_all();
    }
    fn wait(&self) -> Result<T, TtsError> {
        let mut g = self.result.lock();
        loop {
            if let Some(r) = g.as_ref() {
                return r.clone();
            }
            self.ready.wait(&mut g);
        }
    }
}

struct MemoEntry<T> {
    fp: String,
    at: Instant,
    value: T,
    gen: u64,
}

struct Pending<T> {
    fp: String,
    gen: u64,
    slot: Arc<Slot<T>>,
}

struct MemoState<T> {
    entry: Option<MemoEntry<T>>,
    pending: Option<Pending<T>>,
    gen: u64,
}

/// `Memo<T>`: a TTL cache keyed by key fingerprint whose concurrent callers share one load.
struct Memo<T> {
    ttl: Duration,
    state: Mutex<MemoState<T>>,
}

impl<T: Clone> Memo<T> {
    fn new(ttl: Duration) -> Self {
        Self { ttl, state: Mutex::new(MemoState { entry: None, pending: None, gen: 0 }) }
    }

    fn get(&self, fp: &str, refresh: bool, load: impl FnOnce() -> Result<T, TtsError>) -> Result<T, TtsError> {
        let (slot, gen) = {
            let mut st = self.state.lock();
            if !refresh {
                if let Some(e) = &st.entry {
                    if e.fp == fp && e.at.elapsed() < self.ttl {
                        return Ok(e.value.clone());
                    }
                }
                if let Some(p) = &st.pending {
                    if p.fp == fp {
                        let slot = p.slot.clone();
                        drop(st);
                        return slot.wait();
                    }
                }
            }
            st.gen += 1;
            let gen = st.gen;
            let slot = Arc::new(Slot::new());
            // A forced refresh also becomes what later non-refresh callers join.
            st.pending = Some(Pending { fp: fp.to_string(), gen, slot: slot.clone() });
            (slot, gen)
        };
        let mut settle = Settle { memo: self, slot: &slot, gen, fp, result: None };
        let result = load();
        settle.result = Some(result.clone());
        drop(settle);
        result
    }

    fn clear(&self) {
        let mut st = self.state.lock();
        st.entry = None;
        st.pending = None;
    }
}

/// Stores a finished load and releases its joiners — also when the load panicked (they get an error).
struct Settle<'a, T: Clone> {
    memo: &'a Memo<T>,
    slot: &'a Arc<Slot<T>>,
    gen: u64,
    fp: &'a str,
    result: Option<Result<T, TtsError>>,
}

impl<T: Clone> Drop for Settle<'_, T> {
    fn drop(&mut self) {
        // `None` only when the loader panicked: its joiners get a generic failure instead of waiting forever.
        let result = self
            .result
            .take()
            .unwrap_or_else(|| Err(TtsError::new(ReadErrorCode::Unknown, vox_core::copy::SOMETHING_WENT_WRONG)));
        {
            let mut st = self.memo.state.lock();
            if let Ok(value) = &result {
                // An older load finishing after a newer one never clobbers it (TTL from completion).
                if st.entry.as_ref().is_none_or(|e| e.gen < self.gen) {
                    st.entry = Some(MemoEntry { fp: self.fp.to_string(), at: Instant::now(), value: value.clone(), gen: self.gen });
                }
            }
            if st.pending.as_ref().is_some_and(|p| p.gen == self.gen) {
                st.pending = None;
            }
        }
        self.slot.fill(result);
    }
}

// ───────────────────────────── transport ─────────────────────────────

/// Undici's default connect timeout; the connect phase includes the TLS handshake and a proxy's `CONNECT`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Bodies are read fully under the request timeout, up to this size.
const MAX_BODY_BYTES: u64 = 32 * 1024 * 1024;
/// Voice sample downloads (ui-settings-widget.md §4.6).
const PREVIEW_TIMEOUT_MS: u64 = 15_000;

/// Request timeouts (constants in production; shortened by tests).
#[derive(Debug, Clone, Copy)]
struct Timeouts {
    synth_min: Duration,
    synth_per_char: Duration,
    list: Duration,
    preview: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            synth_min: Duration::from_millis(SYNTH_TIMEOUT_MS),
            synth_per_char: Duration::from_millis(SYNTH_MS_PER_CHAR),
            list: Duration::from_millis(LIST_TIMEOUT_MS),
            preview: Duration::from_millis(PREVIEW_TIMEOUT_MS),
        }
    }
}

/// The agents for one proxy value (swapped as a whole by `set_proxy`).
struct Net {
    proxy: Option<ProxyConfig>,
    proxied: Option<ureq::Agent>,
    direct: ureq::Agent,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Method {
    Get,
    Post,
}

/// One request for [`ElevenLabsClient::exchange`].
struct HttpRequest<'a> {
    method: Method,
    url: &'a url::Url,
    /// `xi-api-key` (sensitive header) when present.
    key: Option<&'a str>,
    /// `accept` header; `None` = ureq's default.
    accept: Option<&'a str>,
    /// JSON body (adds `content-type: application/json`).
    body: Option<Vec<u8>>,
    /// Bounds the whole exchange including the body.
    timeout: Duration,
    cancel: Option<&'a CancelToken>,
}

/// A completed HTTP exchange (any status), body read in full.
struct Exchange {
    status: u16,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Exchange {
    /// `headers.get(name)`: case-insensitive; repeated headers joined with `", "`.
    fn header(&self, name: &str) -> Option<String> {
        let values: Vec<String> =
            self.headers.get_all(name).iter().map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned()).collect();
        if values.is_empty() {
            None
        } else {
            Some(values.join(", "))
        }
    }

    fn ok(&self) -> bool {
        (200..=299).contains(&self.status)
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// Non-2xx → the §2.4.4 classification; 2xx → the lenient JSON body or `bad_response`.
    fn json(&self) -> Result<Value, TtsError> {
        let raw = self.text();
        if !self.ok() {
            return Err(error_from_response(self.status, &raw, &|n| self.header(n)));
        }
        vox_core::json::parse_lenient(&raw).map_err(|_| TtsError::bad_response())
    }
}

/// How a transport failure relates to the request having been sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    /// Certainly nothing was written: safe to retry, and the proxy fallback trigger.
    Unsent,
    /// Certainly nothing was written, but the TLS handshake was rejected (certificate, protocol alert): the proxy
    /// fallback still applies; retrying the same route cannot help.
    Rejected,
    /// The request timeout fired (possibly billed).
    Timeout,
    /// Anything else (possibly sent).
    Sent,
}

impl Failure {
    /// Nothing reached the server: a direct retry of a proxied request is safe.
    fn unsent(self) -> bool {
        matches!(self, Failure::Unsent | Failure::Rejected)
    }
}

/// A failure raised while a connection was being set up (DNS, TCP connect, proxy `CONNECT`, TLS handshake): no byte
/// of the request had been written. Produced only by [`connect_phase`].
#[derive(Debug)]
struct ConnectPhaseError(ureq::Error);

impl std::fmt::Display for ConnectPhaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "while connecting: {}", self.0)
    }
}

impl std::error::Error for ConnectPhaseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

/// Tags a connect-phase error (idempotent: the proxy's own connection runs through the same chain, nested). Timeouts
/// stay as they are: ureq already names their phase (`Resolve`/`Connect` vs. the request's `Global` budget).
fn connect_phase(e: ureq::Error) -> ureq::Error {
    let keep = match &e {
        ureq::Error::Timeout(_) => true,
        ureq::Error::Other(inner) => inner.is::<ConnectPhaseError>(),
        _ => false,
    };
    if keep {
        e
    } else {
        ureq::Error::Other(Box::new(ConnectPhaseError(e)))
    }
}

/// Wraps a connector chain so that everything it raises is tagged with [`connect_phase`].
#[derive(Debug)]
struct ConnectPhase<C>(C);

impl<C: Connector<()>> Connector<()> for ConnectPhase<C> {
    type Out = C::Out;

    fn connect(&self, details: &ConnectionDetails, chained: Option<()>) -> Result<Option<Self::Out>, ureq::Error> {
        self.0.connect(details, chained).map_err(connect_phase)
    }
}

/// ureq's resolver, its failures tagged with [`connect_phase`] (a DNS failure is unsent).
#[derive(Debug, Default)]
struct ConnectPhaseResolver(DefaultResolver);

impl Resolver for ConnectPhaseResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        self.0.resolve(uri, config, timeout).map_err(connect_phase)
    }
}

fn classify_failure(e: &ureq::Error) -> Failure {
    use std::io::ErrorKind;
    match e {
        ureq::Error::Other(inner) => match inner.downcast_ref::<ConnectPhaseError>() {
            Some(ConnectPhaseError(cause)) => match cause {
                // The handshake was answered with an alert or a certificate the verifier refused.
                ureq::Error::Io(io) if io.kind() == ErrorKind::InvalidData => Failure::Rejected,
                ureq::Error::Rustls(_) | ureq::Error::Tls(_) | ureq::Error::TlsRequired => Failure::Rejected,
                _ => Failure::Unsent,
            },
            None => Failure::Sent,
        },
        // Raised only while connecting (normally tagged already).
        ureq::Error::HostNotFound | ureq::Error::ConnectProxyFailed(_) => Failure::Unsent,
        ureq::Error::Timeout(ureq::Timeout::Resolve | ureq::Timeout::Connect) => Failure::Unsent,
        ureq::Error::Timeout(_) => Failure::Timeout,
        ureq::Error::Io(io) if io.kind() == ErrorKind::TimedOut => Failure::Timeout,
        // Untagged: on an established connection, possibly after the request was written (D-NET-2) — including
        // `NetworkDown` / `HostUnreachable` / resets from a live socket.
        _ => Failure::Sent,
    }
}

fn failure_error(f: Failure) -> TtsError {
    match f {
        Failure::Unsent => TtsError::network_unsent(),
        Failure::Timeout => TtsError::timeout(),
        Failure::Rejected | Failure::Sent => TtsError::network(),
    }
}

/// `proxy` must be a CONNECT (`http`/`https`) proxy (`build_net` checks).
fn build_agent(user_agent: &str, platform_roots: bool, proxy: Option<ureq::Proxy>) -> ureq::Agent {
    let roots = if platform_roots { ureq::tls::RootCerts::PlatformVerifier } else { ureq::tls::RootCerts::WebPki };
    let tls = ureq::tls::TlsConfig::builder().root_certs(roots).build();
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .user_agent(user_agent)
        .tls_config(tls)
        // Explicit: never ureq's environment proxy (vox-win already folded the environment into `ClientConfig`).
        .proxy(proxy)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .build();
    // ureq's default chain without its SOCKS and missing-TLS-provider guards (neither applies: SOCKS proxies are
    // refused by `build_net`, rustls is compiled in), tagged so that connect-phase failures are recognisable.
    let connector = ConnectPhase(
        ().chain(ConnectProxyConnector::default()).chain(TcpConnector::default()).chain(RustlsConnector::default()),
    );
    ureq::Agent::with_parts(config, connector, ConnectPhaseResolver::default())
}

/// `host:port` of a proxy URL for logs (no scheme, no credentials).
fn proxy_display(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let rest = rest.rsplit_once('@').map_or(rest, |(_, r)| r);
    rest.trim_end_matches('/').to_string()
}

fn build_net(user_agent: &str, platform_roots: bool, proxy: Option<ProxyConfig>, direct: Option<ureq::Agent>) -> Net {
    let direct = direct.unwrap_or_else(|| build_agent(user_agent, platform_roots, None));
    let proxied = proxy.as_ref().and_then(|p| match ureq::Proxy::new(&p.url) {
        Ok(up) if matches!(up.protocol(), ureq::ProxyProtocol::Http | ureq::ProxyProtocol::Https) => {
            Some(build_agent(user_agent, platform_roots, Some(up)))
        }
        // SOCKS (e.g. `ALL_PROXY=socks5://…`) is not compiled into ureq here.
        Ok(up) => {
            tracing::warn!(
                target: "tts",
                "ignoring the {} proxy {} (not supported); using a direct connection",
                up.protocol(),
                proxy_display(&p.url)
            );
            None
        }
        Err(e) => {
            tracing::warn!(target: "tts", "ignoring the proxy {} ({e}); using a direct connection", proxy_display(&p.url));
            None
        }
    });
    Net { proxy, proxied, direct }
}

/// `*` wildcard match (ASCII case already folded by the caller).
fn glob_match(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some((pi, ti));
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

/// True if `host` bypasses the proxy (WinINET `ProxyOverride` semantics: `*` wildcards, `<local>` = no dot,
/// `.suffix` = subdomains; scheme prefixes and ports in patterns are ignored; case-insensitive).
fn proxy_bypassed(cfg: &ProxyConfig, host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    if cfg.bypass_local && !host.contains('.') {
        return true;
    }
    cfg.bypass.iter().any(|raw| {
        let mut p = raw.trim().to_ascii_lowercase();
        if p.is_empty() {
            return false;
        }
        if p == "<local>" {
            return !host.contains('.');
        }
        if let Some((_, rest)) = p.split_once("://") {
            p = rest.to_string();
        }
        if let Some((h, port)) = p.rsplit_once(':') {
            if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) && !h.contains(':') {
                p = h.to_string();
            }
        }
        if p.starts_with('.') {
            host.ends_with(&p)
        } else {
            glob_match(&p, &host)
        }
    })
}

/// `encodeURIComponent`: percent-encodes everything except `A-Za-z0-9-_.!~*'()`.
fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `clamp(v, lo, hi, fallback)`: finite → clamped, else the fallback.
fn clamp_or(v: f64, lo: f64, hi: f64, fallback: f64) -> f64 {
    if v.is_finite() {
        v.max(lo).min(hi)
    } else {
        fallback
    }
}

// ───────────────────────────── client ─────────────────────────────

struct ClientInner {
    key_source: KeySource,
    /// `None` only if neither the configured nor the built-in base URL parses (every request then fails).
    base: Option<url::Url>,
    user_agent: String,
    platform_roots: bool,
    timeouts: Timeouts,
    net: RwLock<Arc<Net>>,
    rejected: RejectedParams,
    voices: Memo<Vec<Voice>>,
    models: Memo<Vec<TtsModel>>,
    account: Memo<Option<AccountInfo>>,
    /// Proxy URLs whose "unreachable" line was already logged.
    proxy_warned: Mutex<HashSet<String>>,
}

/// Cheap to clone; all clones share the agent, the caches and [`RejectedParams`].
#[derive(Clone)]
pub struct ElevenLabsClient {
    inner: Arc<ClientInner>,
}

impl ElevenLabsClient {
    /// Builds the agent from `config`.
    pub fn new(key_source: KeySource, config: ClientConfig) -> Self {
        Self::with_timeouts(key_source, config, Timeouts::default())
    }

    fn with_timeouts(key_source: KeySource, config: ClientConfig, timeouts: Timeouts) -> Self {
        let base = url::Url::parse(&config.base_url)
            .map_err(|e| tracing::warn!(target: "tts", "invalid ElevenLabs base URL ({e}); using the default"))
            .or_else(|_| url::Url::parse(vox_core::constants::ELEVENLABS_BASE_URL))
            .ok();
        let net = build_net(&config.user_agent, config.platform_roots, config.proxy, None);
        Self {
            inner: Arc::new(ClientInner {
                key_source,
                base,
                user_agent: config.user_agent,
                platform_roots: config.platform_roots,
                timeouts,
                net: RwLock::new(Arc::new(net)),
                rejected: RejectedParams::default(),
                voices: Memo::new(Duration::from_millis(VOICES_TTL_MS)),
                models: Memo::new(Duration::from_millis(MODELS_TTL_MS)),
                account: Memo::new(Duration::from_millis(ACCOUNT_TTL_MS)),
                proxy_warned: Mutex::new(HashSet::new()),
            }),
        }
    }

    /// The parameters this process remembers as rejected (shared by every clone).
    #[cfg(test)]
    fn rejected_params(&self) -> &RejectedParams {
        &self.inner.rejected
    }

    /// `key()`: the stored key JS-trimmed; empty → none. Read on every operation.
    fn key(&self) -> Option<String> {
        (self.inner.key_source)().map(|k| js_trim(&k).to_string()).filter(|k| !k.is_empty())
    }

    /// `new URL(path, base)` + `URLSearchParams.set` for each query pair.
    fn api_url(&self, path: &str, query: &[(&str, &str)]) -> Result<url::Url, TtsError> {
        let base = self.inner.base.as_ref().ok_or_else(TtsError::network)?;
        let mut url = base.join(path).map_err(|_| TtsError::network())?;
        if !query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (k, v) in query {
                pairs.append_pair(k, v);
            }
        }
        Ok(url)
    }

    /// One HTTP exchange (`apiRequest` steps 1–5): cancellation check, agent choice (proxy unless bypassed), one direct
    /// retry when the proxy itself failed before sending, transport errors mapped per §2.4.5 / D-NET-2.
    fn exchange(&self, req: HttpRequest<'_>) -> Result<Exchange, TtsError> {
        let HttpRequest { method, url, key, accept, body, timeout, cancel } = req;
        let cancelled = || cancel.is_some_and(CancelToken::is_cancelled);
        if cancelled() {
            return Err(TtsError::cancelled());
        }
        let mut headers: Vec<(&'static str, HeaderValue)> = Vec::with_capacity(3);
        if let Some(accept) = accept {
            headers.push(("accept", HeaderValue::from_str(accept).map_err(|_| TtsError::network())?));
        }
        if let Some(key) = key {
            let mut v = HeaderValue::from_bytes(key.as_bytes()).map_err(|_| TtsError::network())?;
            v.set_sensitive(true);
            headers.push(("xi-api-key", v));
        }
        if body.is_some() {
            headers.push(("content-type", HeaderValue::from_static("application/json")));
        }

        let net = self.inner.net.read().clone();
        let host = url.host_str().unwrap_or("");
        let proxied = match (&net.proxy, &net.proxied) {
            (Some(cfg), Some(agent)) if !proxy_bypassed(cfg, host) => Some((cfg, agent)),
            _ => None,
        };
        let result = match proxied {
            Some((cfg, agent)) => match run_request(agent, method, url.as_str(), &headers, body.as_deref(), timeout) {
                Err(e) if classify_failure(&e).unsent() && !cancelled() => {
                    tracing::debug!(target: "tts", "proxied request failed before sending: {e}");
                    if self.inner.proxy_warned.lock().insert(cfg.url.clone()) {
                        tracing::warn!(target: "tts", "proxy {} unreachable; using a direct connection", proxy_display(&cfg.url));
                    }
                    run_request(&net.direct, method, url.as_str(), &headers, body.as_deref(), timeout)
                }
                other => other,
            },
            None => run_request(&net.direct, method, url.as_str(), &headers, body.as_deref(), timeout),
        };
        match result {
            Ok(ex) => Ok(ex),
            Err(_) if cancelled() => Err(TtsError::cancelled()),
            Err(e) => {
                let f = classify_failure(&e);
                tracing::debug!(target: "tts", "request failed before a response: {e}");
                Err(failure_error(f))
            }
        }
    }

    /// GET a JSON API endpoint with the list timeout.
    fn get_json(&self, key: Option<&str>, path: &str, query: &[(&str, &str)]) -> Result<Value, TtsError> {
        let url = self.api_url(path, query)?;
        let timeout = self.inner.timeouts.list;
        self.exchange(HttpRequest { method: Method::Get, url: &url, key, accept: Some("application/json"), body: None, timeout, cancel: None })?
            .json()
    }

    fn voice_list(data: &Value) -> Result<Vec<Voice>, TtsError> {
        match data.get("voices") {
            Some(Value::Array(items)) if data.is_object() => Ok(items.iter().filter_map(map_voice).collect()),
            _ => Err(TtsError::bad_response()),
        }
    }

    /// `fetchVoicesV2`: up to 25 pages; an error on any page discards the whole listing.
    fn fetch_voices_v2(&self, key: &str) -> Result<Vec<Voice>, TtsError> {
        let page_size = VOICE_PAGE_SIZE.to_string();
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..MAX_VOICE_PAGES {
            let mut query: Vec<(&str, &str)> = vec![("page_size", page_size.as_str()), ("include_total_count", "false")];
            if let Some(t) = token.as_deref() {
                query.push(("next_page_token", t));
            }
            let data = self.get_json(Some(key), "/v2/voices", &query)?;
            out.extend(Self::voice_list(&data)?);
            token = str_of(data.get("next_page_token")).map(str::to_string);
            if data.get("has_more") != Some(&Value::Bool(true)) || token.is_none() {
                break;
            }
        }
        Ok(out)
    }

    fn fetch_voices_v1(&self, key: Option<&str>) -> Result<Vec<Voice>, TtsError> {
        let data = self.get_json(key, "/v1/voices", &[])?;
        Self::voice_list(&data)
    }

    /// All voices for the key (v2 paging, v1 fallback, anonymous v1 without a key), sorted (tts.md §2.7).
    pub fn list_voices(&self, refresh: bool) -> Result<Vec<Voice>, TtsError> {
        let key = self.key();
        self.inner.voices.get(&key_fingerprint(key.as_deref()), refresh, || match key.as_deref() {
            None => match self.fetch_voices_v1(None) {
                Ok(v) => Ok(sort_voices(v)),
                Err(e) if e.code == ReadErrorCode::Network => Err(e),
                Err(_) => Err(TtsError::no_key()),
            },
            Some(key) => match self.fetch_voices_v2(key) {
                Ok(v) => Ok(sort_voices(v)),
                Err(e) => {
                    let v2_unavailable =
                        e.status.is_some_and(|s| (400..500).contains(&s) && ![401, 402, 403, 429].contains(&s));
                    if !v2_unavailable || e.missing_permission {
                        return Err(e);
                    }
                    self.fetch_voices_v1(Some(key)).map(sort_voices)
                }
            },
        })
    }

    /// TTS-capable models; no key → `no_key` (tts.md §2.8).
    pub fn list_models(&self, refresh: bool) -> Result<Vec<TtsModel>, TtsError> {
        let Some(key) = self.key() else {
            return Err(TtsError::no_key());
        };
        self.inner.models.get(&key_fingerprint(Some(&key)), refresh, || {
            match self.get_json(Some(&key), "/v1/models", &[])? {
                Value::Array(items) => Ok(items.iter().filter_map(map_model).collect()),
                _ => Err(TtsError::bad_response()),
            }
        })
    }

    /// Subscription; `Ok(None)` without a key or when the key lacks permission (cached as `None`) (tts.md §2.9).
    pub fn get_account(&self, refresh: bool) -> Result<Option<AccountInfo>, TtsError> {
        let Some(key) = self.key() else {
            return Ok(None);
        };
        self.inner.account.get(&key_fingerprint(Some(&key)), refresh, || {
            match self.get_json(Some(&key), "/v1/user/subscription", &[]).and_then(|data| map_account(&data)) {
                Ok(a) => Ok(Some(a)),
                Err(e) if e.missing_permission => Ok(None),
                Err(e) => Err(e),
            }
        })
    }

    /// `synthesizeOnce`: one `with-timestamps` request.
    fn synthesize_once(&self, key: &str, req: &SynthesisRequest, cancel: &CancelToken) -> Result<SynthesisResult, TtsError> {
        let path = format!("/v1/text-to-speech/{}/with-timestamps", encode_uri_component(&req.voice_id));
        let url = self.api_url(&path, &[("output_format", req.output_format.as_str())])?;
        let body = synthesis_body(req).to_string().into_bytes();
        let t = self.inner.timeouts;
        let per_char = t.synth_per_char.saturating_mul(u32::try_from(utf16_len(&req.text)).unwrap_or(u32::MAX));
        let timeout = t.synth_min.max(per_char);
        let ex = self.exchange(HttpRequest {
            method: Method::Post,
            url: &url,
            key: Some(key),
            accept: Some("application/json"),
            body: Some(body),
            timeout,
            cancel: Some(cancel),
        })?;
        let data = ex.json()?;
        let Some(obj) = data.as_object() else {
            return Err(TtsError::bad_response());
        };
        let audio = str_of(obj.get("audio_base64")).map(decode_base64_lenient).unwrap_or_default();
        if audio.is_empty() {
            return Err(TtsError::no_audio());
        }
        let alignment = obj
            .get("alignment")
            .and_then(map_alignment)
            .or_else(|| obj.get("normalized_alignment").and_then(map_alignment));
        Ok(SynthesisResult {
            audio: Arc::from(audio),
            mime: mime_for_format(&req.output_format).to_string(),
            alignment,
            request_id: ex.header("request-id"),
            character_cost: int_header(ex.header("x-character-count").as_deref())
                .or_else(|| int_header(ex.header("character-cost").as_deref())),
        })
    }

    /// One chunk with timestamps: [`RejectedParams::apply`], then up to 2 parameter-fallback resends (each remembered),
    /// no other retries (the engine owns those). A cancelled token is checked before sending; an in-flight request is
    /// **not** aborted by `cancel` (blocking ureq) — it runs to completion or timeout and the caller decides what to do
    /// with the result (the engine bills it, tts.md §4.7 as changed by D-NET-4).
    pub fn synthesize(&self, req: &SynthesisRequest, cancel: &CancelToken) -> Result<SynthesisResult, TtsError> {
        let Some(key) = self.key() else {
            return Err(TtsError::no_key());
        };
        let mut current = self.inner.rejected.apply(req);
        let mut attempt = 0;
        loop {
            match self.synthesize_once(&key, &current, cancel) {
                Ok(r) => return Ok(r),
                Err(e) => {
                    let reduced = if attempt < 2 { without_rejected_param(&current, &e) } else { None };
                    let Some((next, which)) = reduced else {
                        return Err(e);
                    };
                    self.inner.rejected.remember(&current.model_id, &which);
                    current = next;
                    attempt += 1;
                }
            }
        }
    }

    /// Replaces the proxy for every later request (module docs "Proxy at run time"): builds a new agent and swaps it
    /// atomically; requests in flight keep the agent they started with; memos and rejected parameters are kept.
    pub fn set_proxy(&self, proxy: Option<ProxyConfig>) {
        let direct = self.inner.net.read().direct.clone();
        let net = build_net(&self.inner.user_agent, self.inner.platform_roots, proxy, Some(direct));
        *self.inner.net.write() = Arc::new(net);
    }

    /// The proxy currently in use (`None` = direct).
    pub fn proxy(&self) -> Option<ProxyConfig> {
        self.inner.net.read().proxy.clone()
    }

    /// Drops every memo (never called by the app; key changes rely on the fingerprint).
    pub fn clear_cache(&self) {
        self.inner.voices.clear();
        self.inner.models.clear();
        self.inner.account.clear();
    }

    /// `validateApiKey(key)` (tts.md §2.11): uncached, uses the candidate key. Also applies the 256-unit length guard
    /// (`That doesn’t look like an API key`) so callers need not. `settings` in the result is filled by the caller.
    pub fn validate_api_key(&self, key: &str) -> ApiKeyResult {
        let failed = |error: String| ApiKeyResult { ok: false, error: Some(error), warning: None, account: None };
        let k = js_trim(key);
        if utf16_len(k) > MAX_API_KEY_LEN {
            return failed(msg::KEY_TOO_LONG.into());
        }
        if k.is_empty() {
            return failed(msg::PASTE_KEY_FIRST.into());
        }
        if has_js_whitespace(k) {
            return failed(msg::KEY_HAS_SPACES.into());
        }
        match self.get_json(Some(k), "/v1/user/subscription", &[]).and_then(|data| map_account(&data)) {
            Ok(account) => ApiKeyResult { ok: true, error: None, warning: None, account: Some(account) },
            Err(e) if e.missing_permission => {
                ApiKeyResult { ok: true, error: None, warning: Some(msg::KEY_RESTRICTED_WARNING.into()), account: None }
            }
            Err(e) if e.code == ReadErrorCode::Quota => {
                ApiKeyResult { ok: true, error: None, warning: Some(msg::QUOTA.into()), account: None }
            }
            Err(e) if e.code == ReadErrorCode::Unauthorized => {
                let code = e.api_code.as_deref().unwrap_or("").to_lowercase();
                let generic = e.api_message.as_deref().is_none_or(str::is_empty) || INVALID_KEY_CODES.contains(&code.as_str());
                failed(if generic { msg::KEY_NOT_ACCEPTED.into() } else { e.message })
            }
            Err(e) => failed(e.message),
        }
    }

    /// Downloads a voice's public sample (`Voice::preview_url`, no API key header) for the Voices/onboarding preview
    /// (ui-settings-widget.md §4.6). 15 s timeout; checks `cancel` before sending and after the body (a cancelled
    /// download returns `TtsError::cancelled`).
    pub fn fetch_preview(&self, url: &str, cancel: &CancelToken) -> Result<AudioBytes, TtsError> {
        if cancel.is_cancelled() {
            return Err(TtsError::cancelled());
        }
        let parsed = url::Url::parse(url)
            .ok()
            .filter(|u| matches!(u.scheme(), "http" | "https"))
            .ok_or_else(|| TtsError::new(ReadErrorCode::Unknown, "This voice sample has an invalid address"))?;
        let timeout = self.inner.timeouts.preview;
        let ex = self.exchange(HttpRequest { method: Method::Get, url: &parsed, key: None, accept: None, body: None, timeout, cancel: Some(cancel) })?;
        if cancel.is_cancelled() {
            return Err(TtsError::cancelled());
        }
        if !ex.ok() {
            return Err(error_from_response(ex.status, &ex.text(), &|n| ex.header(n)));
        }
        if ex.body.is_empty() {
            return Err(TtsError::no_audio());
        }
        Ok(Arc::from(ex.body))
    }
}

fn run_request(
    agent: &ureq::Agent,
    method: Method,
    url: &str,
    headers: &[(&'static str, HeaderValue)],
    body: Option<&[u8]>,
    timeout: Duration,
) -> Result<Exchange, ureq::Error> {
    let mut res = match (method, body) {
        (Method::Post, body) => {
            let mut rb = agent.post(url);
            for (k, v) in headers {
                rb = rb.header(*k, v.clone());
            }
            // No redirects for a POST (module docs): a connect failure can then only belong to this one request.
            rb.config().timeout_global(Some(timeout)).max_redirects(0).build().send(body.unwrap_or(&[]))?
        }
        (Method::Get, _) => {
            let mut rb = agent.get(url);
            for (k, v) in headers {
                rb = rb.header(*k, v.clone());
            }
            rb.config().timeout_global(Some(timeout)).build().call()?
        }
    };
    let status = res.status().as_u16();
    let headers = res.headers().clone();
    let body = res.body_mut().with_config().limit(MAX_BODY_BYTES).read_to_vec()?;
    Ok(Exchange { status, headers, body })
}

/// `hex(sha256(utf8(key)))[0..16]`, or `"anon"`.
pub fn key_fingerprint(key: Option<&str>) -> String {
    match key {
        Some(k) if !k.is_empty() => {
            let digest = Sha256::digest(k.as_bytes());
            hex::encode(digest)[..16].to_string()
        }
        _ => "anon".into(),
    }
}

/// `withoutRejectedParam(req, err)` (tts.md §2.6.1): `(reduced request, which parameter)`; the caller remembers it.
pub fn without_rejected_param(req: &SynthesisRequest, err: &TtsError) -> Option<(SynthesisRequest, RejectedParam)> {
    if !matches!(err.status, Some(400) | Some(422)) {
        return None;
    }
    let hint = format!("{} {}", err.param.as_deref().unwrap_or(""), err.api_message.as_deref().unwrap_or("")).to_lowercase();
    if let Some(code) = req.language_code.as_deref().filter(|c| !c.is_empty()) {
        if hint.contains("language") {
            return Some((SynthesisRequest { language_code: None, ..req.clone() }, RejectedParam::Language(code.to_string())));
        }
    }
    let has_context = non_empty(&req.previous_text) || non_empty(&req.next_text);
    let names_context = ["previous_text", "next_text", "previous text", "next text", "stitch"].iter().any(|n| hint.contains(n));
    if has_context && names_context {
        return Some((SynthesisRequest { previous_text: None, next_text: None, ..req.clone() }, RejectedParam::Context));
    }
    None
}

/// Which optional parameter a fallback dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectedParam {
    Language(String),
    Context,
}

/// The JSON body of a synthesis request (clamped voice settings; optional fields omitted when empty).
pub fn synthesis_body(req: &SynthesisRequest) -> serde_json::Value {
    let vs = &req.voice_settings;
    let mut voice = Map::new();
    voice.insert("stability".into(), js_num(clamp_or(vs.stability, 0.0, 1.0, 0.5)));
    voice.insert("similarity_boost".into(), js_num(clamp_or(vs.similarity_boost, 0.0, 1.0, 0.75)));
    voice.insert("style".into(), js_num(clamp_or(vs.style, 0.0, 1.0, 0.0)));
    voice.insert("use_speaker_boost".into(), Value::Bool(vs.use_speaker_boost));
    voice.insert("speed".into(), js_num(clamp_or(vs.speed, 0.7, 1.2, 1.0)));
    let mut body = Map::new();
    body.insert("text".into(), Value::String(req.text.clone()));
    body.insert("model_id".into(), Value::String(req.model_id.clone()));
    body.insert("voice_settings".into(), Value::Object(voice));
    let optional = [("previous_text", &req.previous_text), ("next_text", &req.next_text), ("language_code", &req.language_code)];
    for (name, value) in optional {
        if let Some(v) = value.as_deref().filter(|v| !v.is_empty()) {
            body.insert(name.into(), Value::String(v.to_string()));
        }
    }
    Value::Object(body)
}

#[cfg(test)]
pub(crate) mod test_support {
    //! A local `tiny_http` mock of the ElevenLabs API for the client and engine tests.

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use parking_lot::Mutex;

    use super::*;

    /// A request as the mock server saw it.
    #[derive(Debug, Clone)]
    pub struct Seen {
        pub method: String,
        pub url: String,
        pub headers: Vec<(String, String)>,
        pub body: String,
    }

    impl Seen {
        pub fn header(&self, name: &str) -> Option<&str> {
            self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
        }
        pub fn path(&self) -> &str {
            self.url.split('?').next().unwrap_or("")
        }
        pub fn json(&self) -> Value {
            serde_json::from_str(&self.body).unwrap_or(Value::Null)
        }
    }

    /// A canned response.
    #[derive(Debug, Clone)]
    pub struct Reply {
        pub status: u16,
        pub body: String,
        pub headers: Vec<(String, String)>,
        pub delay: Duration,
    }

    impl Reply {
        pub fn json(status: u16, body: Value) -> Self {
            Self::raw(status, &body.to_string())
        }
        pub fn raw(status: u16, body: &str) -> Self {
            Self { status, body: body.to_string(), headers: vec![], delay: Duration::ZERO }
        }
        pub fn header(mut self, k: &str, v: &str) -> Self {
            self.headers.push((k.into(), v.into()));
            self
        }
        pub fn delay(mut self, d: Duration) -> Self {
            self.delay = d;
            self
        }
    }

    type Handler = dyn Fn(&Seen, usize) -> Reply + Send + Sync;

    /// The mock server; dropped → stops accepting.
    pub struct MockServer {
        pub base_url: String,
        pub seen: Arc<Mutex<Vec<Seen>>>,
        stop: Arc<AtomicBool>,
        server: Arc<tiny_http::Server>,
    }

    impl MockServer {
        /// `handler(request, n)`: `n` = how many requests arrived before this one (0-based).
        pub fn start(handler: impl Fn(&Seen, usize) -> Reply + Send + Sync + 'static) -> Self {
            let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").expect("bind the mock server"));
            let port = server.server_addr().to_ip().expect("ip listener").port();
            let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
            let stop = Arc::new(AtomicBool::new(false));
            let handler: Arc<Handler> = Arc::new(handler);
            {
                let (server, seen, stop) = (server.clone(), seen.clone(), stop.clone());
                std::thread::spawn(move || {
                    while !stop.load(Ordering::SeqCst) {
                        let Ok(Some(mut request)) = server.recv_timeout(Duration::from_millis(50)) else {
                            continue;
                        };
                        let mut body = String::new();
                        let _ = std::io::Read::read_to_string(request.as_reader(), &mut body);
                        let s = Seen {
                            method: request.method().to_string(),
                            url: request.url().to_string(),
                            headers: request
                                .headers()
                                .iter()
                                .map(|h| (h.field.as_str().to_string(), h.value.as_str().to_string()))
                                .collect(),
                            body,
                        };
                        let n = {
                            let mut seen = seen.lock();
                            seen.push(s.clone());
                            seen.len() - 1
                        };
                        let handler = handler.clone();
                        std::thread::spawn(move || {
                            let reply = handler(&s, n);
                            if !reply.delay.is_zero() {
                                std::thread::sleep(reply.delay);
                            }
                            let mut response = tiny_http::Response::from_string(reply.body).with_status_code(reply.status);
                            for (k, v) in &reply.headers {
                                if let Ok(h) = tiny_http::Header::from_bytes(k.as_bytes(), v.as_bytes()) {
                                    response = response.with_header(h);
                                }
                            }
                            let _ = request.respond(response);
                        });
                    }
                });
            }
            Self { base_url: format!("http://127.0.0.1:{port}"), seen, stop, server }
        }

        pub fn seen(&self) -> Vec<Seen> {
            self.seen.lock().clone()
        }

        pub fn count(&self) -> usize {
            self.seen.lock().len()
        }
    }

    impl Drop for MockServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            self.server.unblock();
        }
    }

    /// A minimal HTTP `CONNECT` proxy (what ureq speaks to any HTTP proxy); records each tunnel's target.
    pub struct ConnectProxy {
        pub url: String,
        pub targets: Arc<Mutex<Vec<String>>>,
    }

    impl ConnectProxy {
        pub fn start() -> Self {
            use std::io::{Read, Write};
            use std::net::{Shutdown, TcpListener, TcpStream};
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind the proxy");
            let port = listener.local_addr().expect("proxy address").port();
            let targets: Arc<Mutex<Vec<String>>> = Arc::default();
            let recorded = targets.clone();
            // Detached: blocks in accept until the test process exits.
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut client) = stream else { continue };
                    let recorded = recorded.clone();
                    std::thread::spawn(move || {
                        let mut head = Vec::new();
                        let mut byte = [0u8; 1];
                        while !head.ends_with(b"\r\n\r\n") {
                            if client.read(&mut byte).unwrap_or(0) == 0 {
                                return;
                            }
                            head.push(byte[0]);
                        }
                        let line = String::from_utf8_lossy(&head).into_owned();
                        let mut parts = line.split_whitespace();
                        let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or("").to_string());
                        recorded.lock().push(format!("{method} {target}"));
                        let Ok(upstream) = TcpStream::connect(&target) else {
                            let _ = client.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n");
                            return;
                        };
                        let _ = client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n");
                        let (Ok(mut client_read), Ok(mut upstream_write)) = (client.try_clone(), upstream.try_clone()) else {
                            return;
                        };
                        let forward = std::thread::spawn(move || {
                            let _ = std::io::copy(&mut client_read, &mut upstream_write);
                            let _ = upstream_write.shutdown(Shutdown::Write);
                        });
                        let mut upstream_read = upstream;
                        let _ = std::io::copy(&mut upstream_read, &mut client);
                        let _ = client.shutdown(Shutdown::Write);
                        let _ = forward.join();
                    });
                }
            });
            Self { url: format!("http://127.0.0.1:{port}"), targets }
        }
    }

    /// Reads from `s` until the end of an HTTP head (or EOF).
    pub fn read_head(s: &mut std::net::TcpStream) {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if std::io::Read::read(s, &mut byte).unwrap_or(0) == 0 {
                return;
            }
            head.push(byte[0]);
        }
    }

    /// A server that accepts, reads the request head (the request was written), then drops the connection without
    /// answering — what a sent request sees when the network goes away. Returns its URL and the accepted count.
    pub fn hangup_server() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind the hang-up server");
        let url = format!("http://{}", listener.local_addr().expect("address"));
        let accepted: Arc<std::sync::atomic::AtomicUsize> = Arc::default();
        let count = accepted.clone();
        // Detached: blocks in accept until the test process exits.
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                count.fetch_add(1, Ordering::SeqCst);
                read_head(&mut s);
                drop(s);
            }
        });
        (url, accepted)
    }

    pub fn key_source(key: &str) -> KeySource {
        let key = key.to_string();
        Arc::new(move || Some(key.clone()))
    }

    pub fn config(base_url: &str) -> ClientConfig {
        ClientConfig { base_url: base_url.into(), user_agent: user_agent(), proxy: None, platform_roots: false }
    }

    /// A client with short timeouts (synthesis `min`, lists `list`).
    pub fn client_with(base_url: &str, key: KeySource, synth_min: Duration, list: Duration) -> ElevenLabsClient {
        ElevenLabsClient::with_timeouts(
            key,
            config(base_url),
            Timeouts { synth_min, synth_per_char: Duration::ZERO, list, preview: list },
        )
    }

    pub fn client(base_url: &str, key: &str) -> ElevenLabsClient {
        ElevenLabsClient::new(key_source(key), config(base_url))
    }

    /// A 200 synthesis body with `audio` encoded and a 1:1 alignment for `text`.
    pub fn synthesis_reply(audio: &[u8], text: &str) -> Reply {
        use base64::Engine as _;
        let chars: Vec<String> = text.chars().map(|c| c.to_string()).collect();
        let starts: Vec<f64> = (0..chars.len()).map(|i| i as f64 * 0.05).collect();
        let ends: Vec<f64> = (0..chars.len()).map(|i| (i + 1) as f64 * 0.05).collect();
        let alignment = if chars.is_empty() {
            Value::Null
        } else {
            serde_json::json!({"characters": chars, "character_start_times_seconds": starts, "character_end_times_seconds": ends})
        };
        Reply::json(
            200,
            serde_json::json!({
                "audio_base64": base64::engine::general_purpose::STANDARD.encode(audio),
                "alignment": alignment,
                "normalized_alignment": Value::Null
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Barrier;

    use serde_json::json;

    use super::test_support::*;
    use super::*;

    fn request(text: &str) -> SynthesisRequest {
        SynthesisRequest {
            text: text.into(),
            voice_id: "voice".into(),
            model_id: "eleven_flash_v2_5".into(),
            output_format: "mp3_44100_128".into(),
            voice_settings: VoiceSettings::default(),
            previous_text: None,
            next_text: None,
            language_code: None,
        }
    }

    fn err(status: u16, param: Option<&str>, api_message: Option<&str>) -> TtsError {
        TtsError {
            status: Some(status),
            param: param.map(str::to_string),
            api_message: api_message.map(str::to_string),
            ..TtsError::new(ReadErrorCode::Unknown, "x")
        }
    }

    // ───────────── pure helpers ─────────────

    #[test]
    fn synthesis_body_verified_request() {
        let req = SynthesisRequest {
            text: "Hello".into(),
            voice_id: "a b/c".into(),
            model_id: "eleven_flash_v2_5".into(),
            output_format: "mp3_44100_128".into(),
            voice_settings: VoiceSettings { stability: f64::NAN, similarity_boost: 2.0, style: -1.0, use_speaker_boost: true, speed: 5.0 },
            previous_text: Some("prev".into()),
            next_text: Some("next".into()),
            language_code: Some("en".into()),
        };
        assert_eq!(
            synthesis_body(&req).to_string(),
            r#"{"text":"Hello","model_id":"eleven_flash_v2_5","voice_settings":{"stability":0.5,"similarity_boost":1,"style":0,"use_speaker_boost":true,"speed":1.2},"previous_text":"prev","next_text":"next","language_code":"en"}"#
        );
        // Empty optional fields are omitted.
        let req = SynthesisRequest { previous_text: Some(String::new()), language_code: Some(String::new()), ..request("x") };
        assert_eq!(
            synthesis_body(&req).to_string(),
            r#"{"text":"x","model_id":"eleven_flash_v2_5","voice_settings":{"stability":0.5,"similarity_boost":0.75,"style":0,"use_speaker_boost":true,"speed":1}}"#
        );
        let mut low = request("x");
        low.voice_settings.speed = 0.1;
        assert_eq!(synthesis_body(&low)["voice_settings"]["speed"], json!(0.7));
    }

    #[test]
    fn uri_component_and_query_encoding() {
        assert_eq!(encode_uri_component("a b/c"), "a%20b%2Fc");
        assert_eq!(encode_uri_component("A-z_0.!~*'()"), "A-z_0.!~*'()");
        assert_eq!(encode_uri_component("é?&=+"), "%C3%A9%3F%26%3D%2B");
        let c = client("http://127.0.0.1:9", "k");
        let url = c.api_url("/v2/voices", &[("page_size", "100"), ("next_page_token", "a+b/c= d")]).unwrap();
        assert_eq!(url.as_str(), "http://127.0.0.1:9/v2/voices?page_size=100&next_page_token=a%2Bb%2Fc%3D+d");
        // `new URL(path, base)`: an absolute path replaces the base path.
        let c = client("http://127.0.0.1:9/some/base", "k");
        assert_eq!(c.api_url("/v1/models", &[]).unwrap().as_str(), "http://127.0.0.1:9/v1/models");
    }

    #[test]
    fn key_fingerprints() {
        assert_eq!(key_fingerprint(None), "anon");
        assert_eq!(key_fingerprint(Some("")), "anon");
        // sha256("abc") = ba7816bf8f01cfea414140de5dae2223…
        assert_eq!(key_fingerprint(Some("abc")), "ba7816bf8f01cfea");
        assert_eq!(key_fingerprint(Some("abc")).len(), 16);
        assert_ne!(key_fingerprint(Some("abd")), key_fingerprint(Some("abc")));
    }

    #[test]
    fn without_rejected_param_rules() {
        let full = SynthesisRequest {
            previous_text: Some("p".into()),
            next_text: Some("n".into()),
            language_code: Some("en".into()),
            ..request("x")
        };
        // Only 400 / 422.
        assert!(without_rejected_param(&full, &err(500, Some("body.language_code"), None)).is_none());
        assert!(without_rejected_param(&full, &TtsError::network()).is_none());
        // Language first (hint from param or message, case-insensitive).
        let (r, which) = without_rejected_param(&full, &err(422, Some("body.language_code"), Some("unsupported"))).unwrap();
        assert_eq!(which, RejectedParam::Language("en".into()));
        assert_eq!((r.language_code, r.previous_text.as_deref()), (None, Some("p")));
        let (_, which) = without_rejected_param(&full, &err(400, None, Some("Language not supported"))).unwrap();
        assert_eq!(which, RejectedParam::Language("en".into()));
        // Context: every hint form.
        for hint in ["previous_text", "next_text", "Previous Text bad", "next text", "Request stitching is not supported"] {
            let (r, which) = without_rejected_param(&full, &err(400, None, Some(hint))).unwrap();
            assert_eq!(which, RejectedParam::Context, "{hint}");
            assert_eq!((r.previous_text, r.next_text, r.language_code.as_deref()), (None, None, Some("en")));
        }
        // Without the parameter in the request, nothing to drop.
        assert!(without_rejected_param(&request("x"), &err(422, Some("body.language_code"), None)).is_none());
        assert!(without_rejected_param(&request("x"), &err(400, None, Some("stitch"))).is_none());
        assert!(without_rejected_param(&full, &err(400, None, Some("something else"))).is_none());
        // `language` mentioned but no language_code sent, context sent and named → context.
        let ctx_only = SynthesisRequest { language_code: None, ..full.clone() };
        let (_, which) = without_rejected_param(&ctx_only, &err(400, None, Some("language next_text"))).unwrap();
        assert_eq!(which, RejectedParam::Context);
    }

    #[test]
    fn rejected_params_are_remembered_and_applied() {
        let p = RejectedParams::default();
        let req = SynthesisRequest {
            previous_text: Some("p".into()),
            next_text: Some("n".into()),
            language_code: Some("en".into()),
            ..request("x")
        };
        assert_eq!(p.apply(&req), req);
        p.reject_language("eleven_flash_v2_5", "en");
        assert!(p.language_rejected("eleven_flash_v2_5", "en"));
        assert!(!p.language_rejected("eleven_flash_v2_5", "fr"));
        assert!(!p.language_rejected("eleven_turbo_v2_5", "en"));
        let applied = p.apply(&req);
        assert_eq!((applied.language_code, applied.previous_text.as_deref()), (None, Some("p")));
        // Another code for the same model is still sent.
        let fr = SynthesisRequest { language_code: Some("fr".into()), ..req.clone() };
        assert_eq!(p.apply(&fr).language_code.as_deref(), Some("fr"));
        p.reject_context("eleven_flash_v2_5");
        assert!(p.context_rejected("eleven_flash_v2_5"));
        let applied = p.apply(&fr);
        assert_eq!((applied.previous_text, applied.next_text), (None, None));
        // eleven_v3* never sends context.
        let v3 = SynthesisRequest { model_id: "eleven_v3_alpha".into(), ..req.clone() };
        let applied = RejectedParams::default().apply(&v3);
        assert_eq!((applied.previous_text, applied.next_text, applied.language_code.as_deref()), (None, None, Some("en")));
    }

    #[test]
    fn proxy_bypass_rules() {
        let cfg = |bypass: &[&str], local: bool| ProxyConfig {
            url: "http://proxy:8080".into(),
            bypass: bypass.iter().map(|s| s.to_string()).collect(),
            bypass_local: local,
        };
        assert!(!proxy_bypassed(&cfg(&[], false), "api.elevenlabs.io"));
        assert!(proxy_bypassed(&cfg(&[], true), "intranet"));
        assert!(!proxy_bypassed(&cfg(&[], true), "api.elevenlabs.io"));
        assert!(proxy_bypassed(&cfg(&["*.elevenlabs.io"], false), "API.ElevenLabs.io"));
        assert!(!proxy_bypassed(&cfg(&["*.elevenlabs.io"], false), "elevenlabs.org"));
        assert!(proxy_bypassed(&cfg(&[".elevenlabs.io"], false), "api.elevenlabs.io"));
        assert!(proxy_bypassed(&cfg(&["api.elevenlabs.io"], false), "api.elevenlabs.io"));
        assert!(!proxy_bypassed(&cfg(&["elevenlabs.io"], false), "api.elevenlabs.io"));
        assert!(proxy_bypassed(&cfg(&["10.*", "192.168.*"], false), "192.168.1.5"));
        assert!(proxy_bypassed(&cfg(&["<local>"], false), "printer"));
        assert!(proxy_bypassed(&cfg(&["https://api.elevenlabs.io:443"], false), "api.elevenlabs.io"));
        assert!(proxy_bypassed(&cfg(&["*"], false), "anything.example"));
        assert!(!proxy_bypassed(&cfg(&["", "  "], false), "a.b"));
        assert!(glob_match("a*b*c", "axxbyyc"));
        assert!(!glob_match("a*b*c", "axxbyy"));
        assert!(glob_match("**", ""));
        assert_eq!(proxy_display("http://user:pw@proxy.corp:8080/"), "proxy.corp:8080");
    }

    #[test]
    fn failure_classes() {
        use std::io::{Error as IoError, ErrorKind};
        let io = |kind: ErrorKind| ureq::Error::Io(IoError::from(kind));
        let tagged = |e: ureq::Error| classify_failure(&connect_phase(e));
        assert_eq!(classify_failure(&ureq::Error::HostNotFound), Failure::Unsent);
        assert_eq!(classify_failure(&ureq::Error::ConnectProxyFailed("x".into())), Failure::Unsent);
        assert_eq!(classify_failure(&ureq::Error::Timeout(ureq::Timeout::Connect)), Failure::Unsent);
        assert_eq!(classify_failure(&ureq::Error::Timeout(ureq::Timeout::Resolve)), Failure::Unsent);
        assert_eq!(classify_failure(&ureq::Error::Timeout(ureq::Timeout::Global)), Failure::Timeout);
        assert_eq!(classify_failure(&ureq::Error::Timeout(ureq::Timeout::RecvBody)), Failure::Timeout);
        assert_eq!(classify_failure(&io(ErrorKind::TimedOut)), Failure::Timeout);
        assert_eq!(classify_failure(&ureq::Error::ConnectionFailed), Failure::Sent);
        // Untagged I/O errors come from an established connection: possibly sent, whatever the kind (a Winsock
        // WSAENETDOWN / WSAEHOSTUNREACH on a live socket included).
        for kind in [
            ErrorKind::NetworkDown,
            ErrorKind::NetworkUnreachable,
            ErrorKind::HostUnreachable,
            ErrorKind::ConnectionRefused,
            ErrorKind::AddrNotAvailable,
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::UnexpectedEof,
        ] {
            assert_eq!(classify_failure(&io(kind)), Failure::Sent, "{kind:?}");
            let e = failure_error(classify_failure(&io(kind)));
            assert_eq!((e.clone(), e.retryable), (TtsError::network(), false), "{kind:?}");
        }
        // The same kinds raised while connecting (DNS, TCP, proxy CONNECT, TLS handshake) are unsent.
        for kind in [ErrorKind::NetworkDown, ErrorKind::ConnectionRefused, ErrorKind::ConnectionReset, ErrorKind::UnexpectedEof] {
            assert_eq!(tagged(io(kind)), Failure::Unsent, "{kind:?}");
        }
        assert_eq!(tagged(ureq::Error::HostNotFound), Failure::Unsent);
        assert_eq!(tagged(ureq::Error::ConnectionFailed), Failure::Unsent);
        // A rejected TLS handshake: unsent, but final.
        assert_eq!(tagged(io(ErrorKind::InvalidData)), Failure::Rejected);
        assert_eq!(tagged(ureq::Error::Tls("bad name")), Failure::Rejected);
        assert!(Failure::Rejected.unsent() && Failure::Unsent.unsent() && !Failure::Sent.unsent() && !Failure::Timeout.unsent());
        // Timeouts keep their own phase; tagging is idempotent and keeps the cause in the message.
        assert_eq!(tagged(ureq::Error::Timeout(ureq::Timeout::Global)), Failure::Timeout);
        assert_eq!(tagged(ureq::Error::Timeout(ureq::Timeout::Connect)), Failure::Unsent);
        let twice = connect_phase(connect_phase(io(ErrorKind::ConnectionReset)));
        let ureq::Error::Other(inner) = &twice else { panic!("{twice:?}") };
        let ConnectPhaseError(cause) = inner.downcast_ref::<ConnectPhaseError>().unwrap();
        assert!(matches!(cause, ureq::Error::Io(e) if e.kind() == ErrorKind::ConnectionReset));
        assert!(twice.to_string().contains("while connecting"));
        // An unrelated `Other` error is not a connect-phase one.
        assert_eq!(classify_failure(&ureq::Error::Other(Box::new(IoError::from(ErrorKind::NetworkDown)))), Failure::Sent);

        assert!(failure_error(Failure::Unsent).retryable);
        assert!(!failure_error(Failure::Rejected).retryable);
        assert!(!failure_error(Failure::Timeout).retryable);
        assert!(!failure_error(Failure::Sent).retryable);
    }

    #[test]
    fn user_agent_format() {
        assert_eq!(user_agent(), format!("Vox/{} (Windows)", vox_core::constants::APP_VERSION));
        assert_eq!(ClientConfig::default().base_url, "https://api.elevenlabs.io");
        assert!(ClientConfig::default().platform_roots);
    }

    // ───────────── memo ─────────────

    #[test]
    fn memo_shares_concurrent_loads_and_does_not_cache_failures() {
        let memo = Arc::new(Memo::<u32>::new(Duration::from_secs(60)));
        let loads = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(4));
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let (memo, loads, barrier) = (memo.clone(), loads.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    memo.get("fp", false, || {
                        loads.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(150));
                        Ok(7)
                    })
                })
            })
            .collect();
        for h in handles {
            assert_eq!(h.join().unwrap(), Ok(7));
        }
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        // Cached now.
        assert_eq!(memo.get("fp", false, || Ok(8)), Ok(7));
        // A refresh reloads; a different fingerprint misses.
        assert_eq!(memo.get("fp", true, || Ok(9)), Ok(9));
        assert_eq!(memo.get("other", false, || Ok(10)), Ok(10));
        // Failures are not cached (and the last success for "other" replaced "fp").
        assert_eq!(memo.get("x", false, || Err(TtsError::network())), Err(TtsError::network()));
        assert_eq!(memo.get("x", false, || Ok(11)), Ok(11));
        memo.clear();
        assert_eq!(memo.get("x", false, || Ok(12)), Ok(12));
    }

    #[test]
    fn memo_joiners_share_a_failure() {
        let memo = Arc::new(Memo::<u32>::new(Duration::from_secs(60)));
        let started = Arc::new(Barrier::new(2));
        let m = memo.clone();
        let s = started.clone();
        let first = std::thread::spawn(move || {
            m.get("fp", false, || {
                s.wait();
                std::thread::sleep(Duration::from_millis(100));
                Err(TtsError::timeout())
            })
        });
        started.wait();
        // Joins the in-flight load: gets its error, does not run its own loader.
        assert_eq!(memo.get("fp", false, || Ok(1)), Err(TtsError::timeout()));
        assert_eq!(first.join().unwrap(), Err(TtsError::timeout()));
        assert_eq!(memo.get("fp", false, || Ok(2)), Ok(2));
    }

    #[test]
    fn memo_refresh_replaces_pending_and_older_loads_never_overwrite() {
        let memo = Arc::new(Memo::<u32>::new(Duration::from_secs(60)));
        let started = Arc::new(Barrier::new(2));
        let (m, s) = (memo.clone(), started.clone());
        // A slow first load (gen 1)…
        let slow = std::thread::spawn(move || {
            m.get("fp", false, || {
                s.wait();
                std::thread::sleep(Duration::from_millis(200));
                Ok(1)
            })
        });
        started.wait();
        // …is overtaken by a forced refresh (gen 2) that later non-refresh callers join.
        let (m, s2) = (memo.clone(), Arc::new(Barrier::new(2)));
        let s2c = s2.clone();
        let refresh = std::thread::spawn(move || {
            m.get("fp", true, || {
                s2c.wait();
                std::thread::sleep(Duration::from_millis(50));
                Ok(2)
            })
        });
        s2.wait();
        assert_eq!(memo.get("fp", false, || Ok(99)), Ok(2)); // joined the refresh
        assert_eq!(refresh.join().unwrap(), Ok(2));
        assert_eq!(slow.join().unwrap(), Ok(1)); // its caller still gets its own value…
        assert_eq!(memo.get("fp", false, || Ok(99)), Ok(2)); // …but it never replaced the newer entry
    }

    #[test]
    fn memo_ttl_counts_from_completion() {
        let memo = Memo::<u32>::new(Duration::from_millis(150));
        assert_eq!(
            memo.get("fp", false, || {
                std::thread::sleep(Duration::from_millis(200)); // longer than the TTL
                Ok(1)
            }),
            Ok(1)
        );
        assert_eq!(memo.get("fp", false, || Ok(2)), Ok(1)); // still fresh: measured from completion
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(memo.get("fp", false, || Ok(3)), Ok(3));
    }

    #[test]
    fn memo_releases_joiners_when_a_loader_panics() {
        let memo = Arc::new(Memo::<u32>::new(Duration::from_secs(60)));
        let started = Arc::new(Barrier::new(2));
        let (m, s) = (memo.clone(), started.clone());
        let panicking = std::thread::spawn(move || {
            m.get("fp", false, || -> Result<u32, TtsError> {
                s.wait();
                std::thread::sleep(Duration::from_millis(50));
                panic!("loader bug")
            })
        });
        started.wait();
        assert!(memo.get("fp", false, || Ok(1)).is_err());
        assert!(panicking.join().is_err());
        assert_eq!(memo.get("fp", false, || Ok(5)), Ok(5));
    }

    // ───────────── against the mock server ─────────────

    /// (1) The §2.6 request on the wire.
    #[test]
    fn synthesis_request_on_the_wire() {
        let server = MockServer::start(|_, _| synthesis_reply(b"MP3DATA", "Hello").header("x-character-count", "42").header("request-id", "rq"));
        let c = ElevenLabsClient::new(key_source("  KEY  "), config(&server.base_url));
        let req = SynthesisRequest {
            text: "Hello".into(),
            voice_id: "a b/c".into(),
            model_id: "eleven_flash_v2_5".into(),
            output_format: "mp3_44100_128".into(),
            voice_settings: VoiceSettings { stability: f64::NAN, similarity_boost: 2.0, style: -1.0, use_speaker_boost: true, speed: 5.0 },
            previous_text: Some("prev".into()),
            next_text: Some("next".into()),
            language_code: Some("en".into()),
        };
        let r = c.synthesize(&req, &CancelToken::new()).unwrap();
        assert_eq!(&r.audio[..], b"MP3DATA");
        assert_eq!(r.mime, "audio/mpeg");
        assert_eq!(r.character_cost, Some(42.0));
        assert_eq!(r.request_id.as_deref(), Some("rq"));
        assert_eq!(r.alignment.unwrap().chars.len(), 5);
        let seen = server.seen();
        assert_eq!(seen.len(), 1);
        let s = &seen[0];
        assert_eq!(s.method, "POST");
        assert_eq!(s.url, "/v1/text-to-speech/a%20b%2Fc/with-timestamps?output_format=mp3_44100_128");
        assert_eq!(s.header("xi-api-key"), Some("KEY"));
        assert_eq!(s.header("accept"), Some("application/json"));
        assert_eq!(s.header("content-type"), Some("application/json"));
        assert_eq!(s.header("user-agent").map(str::to_string), Some(user_agent()));
        assert_eq!(
            s.body,
            r#"{"text":"Hello","model_id":"eleven_flash_v2_5","voice_settings":{"stability":0.5,"similarity_boost":1,"style":0,"use_speaker_boost":true,"speed":1.2},"previous_text":"prev","next_text":"next","language_code":"en"}"#
        );
    }

    /// (2) 422 language → 400 stitching → 200 (3 POSTs), then the next chunk in 1 POST without either (D-NET-3).
    #[test]
    fn parameter_fallback_is_remembered() {
        let server = MockServer::start(|s, _| {
            let body = s.json();
            if body.get("language_code").is_some() {
                Reply::json(422, json!({"detail":[{"loc":["body","language_code"],"msg":"unsupported"}]}))
            } else if body.get("previous_text").is_some() || body.get("next_text").is_some() {
                Reply::json(400, json!({"detail":{"status":"invalid_request","message":"Request stitching is not supported"}}))
            } else {
                synthesis_reply(b"AUDIO", "x")
            }
        });
        let c = client(&server.base_url, "k");
        let req = SynthesisRequest {
            previous_text: Some("prev".into()),
            next_text: Some("next".into()),
            language_code: Some("de".into()),
            ..request("x")
        };
        assert!(c.synthesize(&req, &CancelToken::new()).is_ok());
        assert_eq!(server.count(), 3);
        let seen = server.seen();
        assert!(seen[0].json().get("language_code").is_some() && seen[0].json().get("previous_text").is_some());
        assert!(seen[1].json().get("language_code").is_none() && seen[1].json().get("previous_text").is_some());
        assert!(seen[2].json().get("language_code").is_none() && seen[2].json().get("previous_text").is_none());
        // The next chunk (same model) goes out reduced up front: one POST.
        assert!(c.synthesize(&req, &CancelToken::new()).is_ok());
        assert_eq!(server.count(), 4);
        let last = &server.seen()[3];
        assert!(last.json().get("language_code").is_none() && last.json().get("next_text").is_none());
        assert!(c.rejected_params().language_rejected("eleven_flash_v2_5", "de"));
        assert!(c.rejected_params().context_rejected("eleven_flash_v2_5"));
        // Clones share the memory.
        assert!(c.clone().rejected_params().context_rejected("eleven_flash_v2_5"));
    }

    #[test]
    fn fallback_is_bounded_and_unrelated_errors_are_returned() {
        // Always 422 naming the language: original + 1 reduced request, then the error stands (nothing left to drop).
        let server = MockServer::start(|_, _| Reply::json(422, json!({"detail":[{"loc":["body","language_code"],"msg":"bad"}]})));
        let c = client(&server.base_url, "k");
        let req = SynthesisRequest { language_code: Some("en".into()), ..request("x") };
        let e = c.synthesize(&req, &CancelToken::new()).unwrap_err();
        assert_eq!(e.status, Some(422));
        assert_eq!(server.count(), 2);
        // A 401 is not a parameter problem: one request.
        let server = MockServer::start(|_, _| Reply::json(401, json!({"detail":{"status":"invalid_api_key"}})));
        let c = client(&server.base_url, "k");
        let e = c.synthesize(&req, &CancelToken::new()).unwrap_err();
        assert_eq!((e.code, server.count()), (ReadErrorCode::Unauthorized, 1));
    }

    #[test]
    fn synthesis_response_rules() {
        // (8) `"\ud800"` and `1e400` in a 200 body: the audio still comes back (alignment dropped).
        let body = r#"{"audio_base64":"QUJD","alignment":null,"normalized_alignment":{"characters":["\ud800"],"character_start_times_seconds":[0],"character_end_times_seconds":[1e400]}}"#;
        let server = MockServer::start(move |_, _| Reply::raw(200, body));
        let c = client(&server.base_url, "k");
        let r = c.synthesize(&request("x"), &CancelToken::new()).unwrap();
        assert_eq!(&r.audio[..], b"ABC");
        assert!(r.alignment.is_none());
        assert_eq!(r.character_cost, None);

        // Normalized alignment is the fallback; character-cost header is accepted; lenient base64.
        let server = MockServer::start(|_, _| {
            Reply::json(200, json!({"audio_base64":"QU JD\n","alignment":{"characters":[]},"normalized_alignment":{"characters":["a"],"character_start_times_seconds":[0],"character_end_times_seconds":[0.5]}}))
                .header("character-cost", "1.5")
        });
        let r = client(&server.base_url, "k").synthesize(&request("x"), &CancelToken::new()).unwrap();
        assert_eq!(&r.audio[..], b"ABC");
        assert_eq!(r.alignment.unwrap().chars, ["a"]);
        assert_eq!(r.character_cost, Some(1.5));

        // No audio / unreadable bodies are final (not retryable, D-NET-2).
        for body in [r#"{"audio_base64":""}"#, r#"{"audio_base64":"!!!!"}"#, r#"{}"#] {
            let server = MockServer::start(move |_, _| Reply::raw(200, body));
            let e = client(&server.base_url, "k").synthesize(&request("x"), &CancelToken::new()).unwrap_err();
            assert_eq!(e, TtsError::no_audio(), "{body}");
        }
        for body in ["", "not json", "[1]", "\"s\""] {
            let server = MockServer::start(move |_, _| Reply::raw(200, body));
            let e = client(&server.base_url, "k").synthesize(&request("x"), &CancelToken::new()).unwrap_err();
            assert_eq!(e, TtsError::bad_response(), "{body}");
            assert!(!e.retryable);
        }
        // Errors carry the HTTP classification.
        let server = MockServer::start(|_, _| Reply::json(429, json!({})).header("retry-after", "2"));
        let e = client(&server.base_url, "k").synthesize(&request("x"), &CancelToken::new()).unwrap_err();
        assert_eq!((e.code, e.retryable, e.retry_after_sec), (ReadErrorCode::RateLimit, true, Some(2.0)));
    }

    #[test]
    fn synthesis_needs_a_key_and_honours_cancellation() {
        let server = MockServer::start(|_, _| synthesis_reply(b"A", "x"));
        let c = ElevenLabsClient::new(Arc::new(|| Some("   ".to_string())), config(&server.base_url));
        assert_eq!(c.synthesize(&request("x"), &CancelToken::new()).unwrap_err(), TtsError::no_key());
        let c = ElevenLabsClient::new(Arc::new(|| None), config(&server.base_url));
        assert_eq!(c.synthesize(&request("x"), &CancelToken::new()).unwrap_err().code, ReadErrorCode::NoApiKey);
        let token = CancelToken::new();
        token.cancel();
        let e = client(&server.base_url, "k").synthesize(&request("x"), &token).unwrap_err();
        assert!(e.aborted);
        assert_eq!(server.count(), 0);
    }

    #[test]
    fn timeouts_and_transport_errors_are_not_retryable() {
        let server = MockServer::start(|_, _| synthesis_reply(b"A", "x").delay(Duration::from_millis(600)));
        let c = client_with(&server.base_url, key_source("k"), Duration::from_millis(150), Duration::from_millis(150));
        let e = c.synthesize(&request("x"), &CancelToken::new()).unwrap_err();
        assert_eq!(e, TtsError::timeout());
        assert!(!e.retryable);
        // A refused connection is certainly unsent → retryable network error.
        let dead = dead_port_url();
        let e = client(&dead, "k").synthesize(&request("x"), &CancelToken::new()).unwrap_err();
        assert_eq!(e, TtsError::network_unsent());
        assert!(e.retryable);
    }

    fn dead_port_url() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        format!("http://127.0.0.1:{port}")
    }

    fn voices_page(ids: &[&str], token: Option<&str>, has_more: Value) -> Reply {
        let voices: Vec<Value> = ids.iter().map(|id| json!({"voice_id": id, "name": id})).collect();
        let mut body = json!({"voices": voices, "has_more": has_more});
        if let Some(t) = token {
            body["next_page_token"] = json!(t);
        }
        Reply::json(200, body)
    }

    /// (3) `/v2/voices` paging.
    #[test]
    fn voices_v2_paging() {
        let server = MockServer::start(|_, n| match n % 2 {
            0 => voices_page(&["b"], Some("a+b/c= d"), json!(true)),
            _ => voices_page(&["a"], Some("t2"), json!("true")), // not strictly `true` → stop
        });
        let c = client(&server.base_url, "k");
        let voices = c.list_voices(false).unwrap();
        assert_eq!(voices.iter().map(|v| v.voice_id.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        let seen = server.seen();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].url, "/v2/voices?page_size=100&include_total_count=false");
        assert_eq!(seen[1].url, "/v2/voices?page_size=100&include_total_count=false&next_page_token=a%2Bb%2Fc%3D+d");
        assert_eq!(server.seen()[0].header("xi-api-key"), Some("k"));
        // Cached (10 min): no new request.
        c.list_voices(false).unwrap();
        assert_eq!(server.count(), 2);
        // A refresh reloads.
        c.list_voices(true).unwrap();
        assert_eq!(server.count(), 4);
    }

    #[test]
    fn voices_v2_page_cap_and_errors() {
        // 25 pages all saying has_more: the 25 pages are returned silently.
        let server = MockServer::start(|_, n| voices_page(&[&format!("v{n}")], Some("t"), json!(true)));
        let voices = client(&server.base_url, "k").list_voices(false).unwrap();
        assert_eq!((voices.len(), server.count()), (25, 25));

        // An error on page 2 discards page 1 (no v1 fallback for a 5xx).
        let server = MockServer::start(|_, n| if n == 0 { voices_page(&["a"], Some("t"), json!(true)) } else { Reply::json(500, json!({})) });
        let e = client(&server.base_url, "k").list_voices(false).unwrap_err();
        assert_eq!((e.status, server.count()), (Some(500), 2));

        // A page without a voices array is a bad response.
        let server = MockServer::start(|_, _| Reply::json(200, json!({"voices": "x"})));
        assert_eq!(client(&server.base_url, "k").list_voices(false).unwrap_err(), TtsError::bad_response());
    }

    #[test]
    fn voices_v1_fallback_rules() {
        let fallback_for = |status: u16, body: Value| {
            let server = MockServer::start(move |s, _| {
                if s.path() == "/v2/voices" {
                    Reply::json(status, body.clone())
                } else {
                    voices_page(&["legacy"], None, json!(false))
                }
            });
            let r = client(&server.base_url, "k").list_voices(false);
            (r, server.seen().iter().map(|s| s.path().to_string()).collect::<Vec<_>>())
        };
        // 404 / 400 / 422 → v1 with the key.
        for status in [400, 404, 422] {
            let (r, paths) = fallback_for(status, json!({}));
            assert_eq!(r.unwrap()[0].voice_id, "legacy", "{status}");
            assert_eq!(paths, ["/v2/voices", "/v1/voices"]);
        }
        // 401/402/403/429 and 5xx: no fallback.
        for status in [401, 402, 403, 429, 500] {
            let (r, paths) = fallback_for(status, json!({}));
            assert!(r.is_err(), "{status}");
            assert_eq!(paths, ["/v2/voices"], "{status}");
        }
        // Never with a missing permission (even on a 4xx that would fall back).
        let (r, paths) = fallback_for(400, json!({"detail":{"status":"missing_permissions","message":"missing the permission voices_read"}}));
        assert!(r.unwrap_err().missing_permission);
        assert_eq!(paths, ["/v2/voices"]);
    }

    #[test]
    fn anonymous_voices() {
        // Without a key: v1 without an auth header, cached under "anon".
        let server = MockServer::start(|_, _| voices_page(&["premade"], None, json!(false)));
        let c = ElevenLabsClient::new(Arc::new(|| None), config(&server.base_url));
        assert_eq!(c.list_voices(false).unwrap()[0].voice_id, "premade");
        let s = &server.seen()[0];
        assert_eq!(s.path(), "/v1/voices");
        assert_eq!(s.header("xi-api-key"), None);
        c.list_voices(false).unwrap();
        assert_eq!(server.count(), 1);

        // An anonymous 401 needs_authorization → no-api-key.
        let server = MockServer::start(|_, _| Reply::json(401, json!({"detail":{"status":"needs_authorization"}})));
        let c = ElevenLabsClient::new(Arc::new(|| None), config(&server.base_url));
        assert_eq!(c.list_voices(false).unwrap_err(), TtsError::no_key());

        // An anonymous timeout is rethrown as is.
        let server = MockServer::start(|_, _| voices_page(&["x"], None, json!(false)).delay(Duration::from_millis(600)));
        let c = client_with(&server.base_url, Arc::new(|| None), Duration::from_secs(5), Duration::from_millis(150));
        assert_eq!(c.list_voices(false).unwrap_err(), TtsError::timeout());
    }

    /// (4) Models.
    #[test]
    fn models() {
        let server = MockServer::start(|_, _| {
            Reply::json(200, json!([{"model_id":"m1","can_do_text_to_speech":true},{"model_id":"m2","can_do_text_to_speech":false},"x"]))
        });
        let c = client(&server.base_url, "k");
        let models = c.list_models(false).unwrap();
        assert_eq!(models.iter().map(|m| m.model_id.as_str()).collect::<Vec<_>>(), ["m1"]);
        assert_eq!(server.seen()[0].path(), "/v1/models");
        c.list_models(false).unwrap();
        assert_eq!(server.count(), 1);

        let server = MockServer::start(|_, _| Reply::json(200, json!({"models": []})));
        assert_eq!(client(&server.base_url, "k").list_models(false).unwrap_err(), TtsError::bad_response());

        let c = ElevenLabsClient::new(Arc::new(|| None), config(&server.base_url));
        assert_eq!(c.list_models(false).unwrap_err(), TtsError::no_key());
    }

    /// (5) Account: missing permission → `Ok(None)` cached 60 s; no key → `Ok(None)` without a request.
    #[test]
    fn account() {
        let server = MockServer::start(|_, _| {
            Reply::json(401, json!({"detail":{"status":"missing_permissions","message":"missing the permission user_read"}}))
        });
        let c = client(&server.base_url, "k");
        assert_eq!(c.get_account(false), Ok(None));
        assert_eq!(c.get_account(false), Ok(None));
        assert_eq!(server.count(), 1);
        assert_eq!(server.seen()[0].path(), "/v1/user/subscription");

        let server = MockServer::start(|_, _| Reply::json(200, json!({"tier":"creator","character_count":10,"character_limit":100})));
        let a = client(&server.base_url, "k").get_account(false).unwrap().unwrap();
        assert_eq!((a.tier.as_str(), a.character_count, a.character_limit), ("creator", 10.0, 100.0));

        let server = MockServer::start(|_, _| Reply::json(500, json!({})));
        assert!(client(&server.base_url, "k").get_account(false).is_err());

        let c = ElevenLabsClient::new(Arc::new(|| None), config(&server.base_url));
        let before = server.count();
        assert_eq!(c.get_account(false), Ok(None));
        assert_eq!(server.count(), before);
    }

    /// (6) Memo through the client: concurrent callers share one request; a key change misses the cache.
    #[test]
    fn client_memo_shares_requests_and_follows_the_key() {
        let server = MockServer::start(|_, _| {
            Reply::json(200, json!([{"model_id":"m","can_do_text_to_speech":true}])).delay(Duration::from_millis(150))
        });
        let key = Arc::new(Mutex::new("k1".to_string()));
        let k = key.clone();
        let c = ElevenLabsClient::new(Arc::new(move || Some(k.lock().clone())), config(&server.base_url));
        let barrier = Arc::new(Barrier::new(3));
        let handles: Vec<_> = (0..3)
            .map(|_| {
                let (c, b) = (c.clone(), barrier.clone());
                std::thread::spawn(move || {
                    b.wait();
                    c.list_models(false)
                })
            })
            .collect();
        for h in handles {
            assert!(h.join().unwrap().is_ok());
        }
        assert_eq!(server.count(), 1);
        *key.lock() = "k2".into();
        c.list_models(false).unwrap();
        assert_eq!(server.count(), 2);
        assert_eq!(server.seen()[1].header("xi-api-key"), Some("k2"));
        c.clear_cache();
        c.list_models(false).unwrap();
        assert_eq!(server.count(), 3);
    }

    /// (7) Every §2.11 validation row.
    #[test]
    fn key_validation_rows() {
        let server = MockServer::start(|s, _| match s.header("xi-api-key").unwrap_or("") {
            "good" => Reply::json(200, json!({"tier":"free","character_count":1,"character_limit":10})),
            "array" => Reply::json(200, json!([1])),
            "restricted" => Reply::json(401, json!({"detail":{"status":"missing_permissions","message":"The API key you used is missing the permission user_read"}})),
            "broke" => Reply::json(401, json!({"detail":{"status":"quota_exceeded"}})),
            "invalid" => Reply::json(401, json!({"detail":{"status":"invalid_api_key","message":"Invalid API key"}})),
            "nomessage" => Reply::json(401, json!({"detail":{"status":"detected_unusual_activity"}})),
            "unusual" => Reply::json(401, json!({"detail":{"status":"detected_unusual_activity","message":"Unusual activity detected."}})),
            "upper" => Reply::json(401, json!({"detail":{"status":"INVALID_API_KEY","message":"bad"}})),
            "server" => Reply::json(503, json!({})),
            _ => Reply::json(418, json!({})),
        });
        let c = client(&server.base_url, "stored");
        let v = |k: &str| c.validate_api_key(k);

        let r = v("  good \n");
        assert!(r.ok && r.error.is_none() && r.warning.is_none());
        assert_eq!(r.account.unwrap().tier, "free");
        assert_eq!(server.seen().last().unwrap().header("xi-api-key"), Some("good")); // the candidate, trimmed

        assert_eq!(v("   ").error.as_deref(), Some(msg::PASTE_KEY_FIRST));
        assert_eq!(v("").error.as_deref(), Some(msg::PASTE_KEY_FIRST));
        for spaced in ["ab cd", "ab\u{00A0}cd", "ab\u{FEFF}cd", "ab\u{2028}cd", "ab\tcd"] {
            let r = v(spaced);
            assert_eq!((r.ok, r.error.as_deref()), (false, Some(msg::KEY_HAS_SPACES)), "{spaced:?}");
        }
        let long = "k".repeat(257);
        assert_eq!(v(&long).error.as_deref(), Some(msg::KEY_TOO_LONG));
        assert_ne!(v(&format!("  {}  ", "k".repeat(256))).error.as_deref(), Some(msg::KEY_TOO_LONG));
        let requests_before = server.count();
        v("ab cd");
        assert_eq!(server.count(), requests_before); // nothing sent for local rejections

        let r = v("array");
        assert_eq!((r.ok, r.error.as_deref()), (false, Some(msg::BAD_RESPONSE)));

        let r = v("restricted");
        assert!(r.ok && r.account.is_none());
        assert_eq!(r.warning.as_deref(), Some(msg::KEY_RESTRICTED_WARNING));

        let r = v("broke");
        assert!(r.ok && r.account.is_none());
        assert_eq!(r.warning.as_deref(), Some(msg::QUOTA));

        assert_eq!(v("invalid").error.as_deref(), Some(msg::KEY_NOT_ACCEPTED));
        assert_eq!(v("nomessage").error.as_deref(), Some(msg::KEY_NOT_ACCEPTED));
        assert_eq!(v("upper").error.as_deref(), Some(msg::KEY_NOT_ACCEPTED));
        assert_eq!(v("unusual").error.as_deref(), Some("ElevenLabs: Unusual activity detected."));
        assert_eq!(v("server").error.as_deref(), Some(msg::SERVER));
        assert_eq!(v("other").error.as_deref(), Some("ElevenLabs returned an error (HTTP 418)"));

        let r = client(&dead_port_url(), "k").validate_api_key("good");
        assert_eq!((r.ok, r.error.as_deref()), (false, Some(msg::NETWORK)));
    }

    /// (9) A dead proxy → one direct retry per request, logged once.
    #[test]
    fn dead_proxy_falls_back_to_direct() {
        let server = MockServer::start(|_, _| Reply::json(200, json!([{"model_id":"m","can_do_text_to_speech":true}])));
        let proxy = ProxyConfig { url: dead_port_url(), bypass: vec![], bypass_local: false };
        let cfg = ClientConfig { proxy: Some(proxy.clone()), ..config(&server.base_url) };
        let c = ElevenLabsClient::new(key_source("k"), cfg);
        assert_eq!(c.proxy(), Some(proxy.clone()));
        assert!(c.list_models(false).is_ok());
        assert!(c.list_models(true).is_ok());
        assert_eq!(server.count(), 2);
        assert_eq!(c.inner.proxy_warned.lock().len(), 1);
        // A different dead proxy is logged once more.
        let other = ProxyConfig { url: dead_port_url().replace("http://", "http://user:pw@"), ..proxy };
        c.set_proxy(Some(other.clone()));
        assert!(c.list_models(true).is_ok());
        assert_eq!(c.inner.proxy_warned.lock().len(), 2);
        // Back to direct.
        c.set_proxy(None);
        assert_eq!(c.proxy(), None);
        assert!(c.list_models(true).is_ok());
        assert_eq!(server.count(), 4);
    }

    /// A working proxy is used (one `CONNECT` tunnel to the API host), and a bypass entry skips it.
    #[test]
    fn live_proxy_is_used_unless_bypassed() {
        let target = MockServer::start(|_, _| Reply::json(200, json!([])));
        let proxy_server = ConnectProxy::start();
        let proxy = ProxyConfig { url: proxy_server.url.clone(), bypass: vec![], bypass_local: false };
        let c = ElevenLabsClient::new(key_source("k"), ClientConfig { proxy: Some(proxy.clone()), ..config(&target.base_url) });
        c.list_models(false).unwrap();
        let host = target.base_url.trim_start_matches("http://").to_string();
        assert_eq!(*proxy_server.targets.lock(), [format!("CONNECT {host}")]);
        assert_eq!(target.count(), 1);
        assert_eq!(target.seen()[0].header("xi-api-key"), Some("k"));
        assert!(c.inner.proxy_warned.lock().is_empty());
        // Bypassed hosts go direct.
        c.set_proxy(Some(ProxyConfig { bypass: vec!["127.0.0.*".into()], ..proxy.clone() }));
        c.list_models(true).unwrap();
        assert_eq!(proxy_server.targets.lock().len(), 1);
        assert_eq!(target.count(), 2);
        // `<local>`-style bypass does not apply to a dotted host.
        c.set_proxy(Some(ProxyConfig { bypass_local: true, ..proxy }));
        c.list_models(true).unwrap();
        assert_eq!(target.count(), 3);
    }

    /// Ways a proxy can fail its own `CONNECT` exchange (nothing reached the API yet).
    #[derive(Debug, Clone, Copy)]
    enum BadProxy {
        /// Accepts, reads part of the `CONNECT`, then drops the socket with unread input (a reset).
        Reset,
        /// Answers like a SOCKS5 listener (`05 ff`), i.e. not HTTP.
        NotHttp,
        /// Reads the `CONNECT`, then closes without answering.
        Silent,
        /// Refuses with `407`.
        AuthRequired,
    }

    fn bad_proxy(kind: BadProxy) -> (String, Arc<AtomicUsize>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let accepted: Arc<AtomicUsize> = Arc::default();
        let count = accepted.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                count.fetch_add(1, Ordering::SeqCst);
                match kind {
                    BadProxy::Reset => {
                        let _ = s.read(&mut [0u8; 1]);
                    }
                    BadProxy::NotHttp => {
                        read_head(&mut s);
                        let _ = s.write_all(&[0x05, 0xff]);
                    }
                    BadProxy::Silent => read_head(&mut s),
                    BadProxy::AuthRequired => {
                        read_head(&mut s);
                        let _ = s.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n");
                    }
                }
                drop(s);
            }
        });
        (url, accepted)
    }

    /// D-NET-1: a proxy that resets, speaks something else than HTTP, hangs up or refuses → each request goes out
    /// directly exactly once (lists and synthesis alike).
    #[test]
    fn broken_proxies_fall_back_to_direct() {
        for kind in [BadProxy::Reset, BadProxy::NotHttp, BadProxy::Silent, BadProxy::AuthRequired] {
            let target = MockServer::start(|s, _| {
                if s.method == "POST" {
                    synthesis_reply(b"AUDIO", "x")
                } else {
                    Reply::json(200, json!([{"model_id":"m","can_do_text_to_speech":true}]))
                }
            });
            let (url, accepted) = bad_proxy(kind);
            let proxy = ProxyConfig { url, bypass: vec![], bypass_local: false };
            let c = ElevenLabsClient::new(key_source("k"), ClientConfig { proxy: Some(proxy), ..config(&target.base_url) });
            assert_eq!(c.list_models(false).map(|m| m.len()), Ok(1), "{kind:?}");
            assert_eq!(target.count(), 1, "{kind:?}");
            let r = c.synthesize(&request("x"), &CancelToken::new());
            assert_eq!(r.map(|r| r.audio.to_vec()), Ok(b"AUDIO".to_vec()), "{kind:?}");
            assert_eq!(target.count(), 2, "{kind:?}: the POST reached the API exactly once");
            assert_eq!(accepted.load(Ordering::SeqCst), 2, "{kind:?}: one proxy attempt per request");
            assert_eq!(c.inner.proxy_warned.lock().len(), 1, "{kind:?}");
        }
    }

    /// D-NET-2: a request that was written and then lost its connection is possibly billed — not retryable, and
    /// never re-sent (not even directly when it went through a working proxy).
    #[test]
    fn failure_after_sending_is_final() {
        let (url, accepted) = hangup_server();
        let e = client(&url, "k").synthesize(&request("x"), &CancelToken::new()).unwrap_err();
        assert_eq!((e.clone(), e.retryable), (TtsError::network(), false));
        assert_eq!(accepted.load(Ordering::SeqCst), 1);

        let proxy_server = ConnectProxy::start();
        let proxy = ProxyConfig { url: proxy_server.url.clone(), bypass: vec![], bypass_local: false };
        let c = ElevenLabsClient::new(key_source("k"), ClientConfig { proxy: Some(proxy), ..config(&url) });
        let e = c.synthesize(&request("x"), &CancelToken::new()).unwrap_err();
        assert_eq!((e.clone(), e.retryable), (TtsError::network(), false));
        assert_eq!(proxy_server.targets.lock().len(), 1);
        assert_eq!(accepted.load(Ordering::SeqCst), 2, "no direct re-send after the tunnel delivered the request");
        assert!(c.inner.proxy_warned.lock().is_empty());
    }

    /// A POST is never redirected (its connect failures stay unambiguous); a GET follows redirects like Node.
    #[test]
    fn redirects_only_for_gets() {
        let server = MockServer::start(|s, _| match (s.method.as_str(), s.path()) {
            ("POST", _) => Reply::raw(307, "").header("location", "/elsewhere"),
            ("GET", "/v1/models") => Reply::raw(302, "").header("location", "/v1/models-moved"),
            ("GET", "/v1/models-moved") => Reply::json(200, json!([{"model_id":"m","can_do_text_to_speech":true}])),
            _ => Reply::raw(404, ""),
        });
        let c = client(&server.base_url, "k");
        let e = c.synthesize(&request("x"), &CancelToken::new()).unwrap_err();
        assert_eq!((e.status, e.retryable), (Some(307), false));
        assert_eq!(server.count(), 1);
        assert_eq!(c.list_models(false).map(|m| m.len()), Ok(1));
        let paths: Vec<String> = server.seen().iter().map(|s| s.path().to_string()).collect();
        assert_eq!(paths, ["/v1/text-to-speech/voice/with-timestamps", "/v1/models", "/v1/models-moved"]);
    }

    /// A SOCKS proxy URL (e.g. from `ALL_PROXY`) is ignored, not a crash: requests go direct.
    #[test]
    fn socks_proxies_are_ignored() {
        let server = MockServer::start(|_, _| Reply::json(200, json!([])));
        let proxy = ProxyConfig { url: "socks5://127.0.0.1:9".into(), bypass: vec![], bypass_local: false };
        let c = ElevenLabsClient::new(key_source("k"), ClientConfig { proxy: Some(proxy.clone()), ..config(&server.base_url) });
        assert_eq!(c.proxy(), Some(proxy));
        assert!(c.list_models(false).is_ok());
        assert_eq!(server.count(), 1);
        assert!(c.inner.proxy_warned.lock().is_empty());
    }

    #[test]
    fn preview_download() {
        let server = MockServer::start(|s, _| match s.path() {
            "/sample.mp3" => Reply::raw(200, "MP3BYTES"),
            "/empty.mp3" => Reply::raw(200, ""),
            _ => Reply::raw(404, "missing"),
        });
        let c = client(&server.base_url, "secret");
        let audio = c.fetch_preview(&format!("{}/sample.mp3", server.base_url), &CancelToken::new()).unwrap();
        assert_eq!(&audio[..], b"MP3BYTES");
        assert_eq!(server.seen()[0].header("xi-api-key"), None); // never the key
        assert_eq!(c.fetch_preview(&format!("{}/empty.mp3", server.base_url), &CancelToken::new()).unwrap_err(), TtsError::no_audio());
        let e = c.fetch_preview(&format!("{}/nope.mp3", server.base_url), &CancelToken::new()).unwrap_err();
        assert_eq!(e.status, Some(404));
        assert_eq!(c.fetch_preview("file:///c:/x.mp3", &CancelToken::new()).unwrap_err().code, ReadErrorCode::Unknown);
        assert_eq!(c.fetch_preview(&format!("{}/sample.mp3", dead_port_url()), &CancelToken::new()).unwrap_err().code, ReadErrorCode::Network);
        let token = CancelToken::new();
        token.cancel();
        let before = server.count();
        assert!(c.fetch_preview(&format!("{}/sample.mp3", server.base_url), &token).unwrap_err().aborted);
        assert_eq!(server.count(), before);
        // Cancelled while downloading: the result is discarded.
        let slow = MockServer::start(|_, _| Reply::raw(200, "MP3").delay(Duration::from_millis(200)));
        let token = CancelToken::new();
        let t2 = token.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            t2.cancel();
        });
        assert!(c.fetch_preview(&format!("{}/a.mp3", slow.base_url), &token).unwrap_err().aborted);
        canceller.join().unwrap();
    }

    /// Live ElevenLabs smoke test. Run manually with a real key:
    /// `ELEVENLABS_API_KEY=… cargo test -p vox-tts live_elevenlabs_smoke -- --ignored --nocapture`
    /// (needs network access; bills a few characters).
    #[test]
    #[ignore]
    fn live_elevenlabs_smoke() {
        let key = std::env::var("ELEVENLABS_API_KEY").expect("set ELEVENLABS_API_KEY");
        let c = ElevenLabsClient::new(key_source(&key), ClientConfig::default());
        let r = c.validate_api_key(&key);
        assert!(r.ok, "{:?}", r.error);
        assert!(!c.list_voices(false).unwrap().is_empty());
        assert!(!c.list_models(false).unwrap().is_empty());
        let req = SynthesisRequest {
            voice_id: vox_core::constants::ELEVENLABS_DEFAULT_VOICE_ID.into(),
            ..request("Hello from Vox.")
        };
        let out = c.synthesize(&req, &CancelToken::new()).unwrap();
        assert!(out.audio.len() > 1000);
        assert!(out.alignment.is_some());
    }
}
