//! App-shell types: routes, toasts, app info, chip modes, runtime flags (contracts.md §1.8, §3.8; ui-main.md §2.1).

use std::path::PathBuf;

use crate::constants::{READ_LABEL, READ_WITH_LABEL};
use crate::settings::is_settings_section;

/// Main-window pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum AppPage {
    #[default]
    Home,
    History,
    Analytics,
    Voices,
    Settings,
    Onboarding,
}

impl AppPage {
    /// Sidebar order (Ctrl+1..5).
    pub const NAV: [AppPage; 5] = [AppPage::Home, AppPage::History, AppPage::Analytics, AppPage::Voices, AppPage::Settings];
    pub const ALL: [AppPage; 6] = [AppPage::Home, AppPage::History, AppPage::Analytics, AppPage::Voices, AppPage::Settings, AppPage::Onboarding];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Home => "home",
            Self::History => "history",
            Self::Analytics => "analytics",
            Self::Voices => "voices",
            Self::Settings => "settings",
            Self::Onboarding => "onboarding",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == s)
    }
    /// Header title / nav label.
    pub fn title(self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::History => "History",
            Self::Analytics => "Analytics",
            Self::Voices => "Voices",
            Self::Settings => "Settings",
            Self::Onboarding => "Setup",
        }
    }
}

/// A route: page + optional anchor (settings section id).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AppRoute {
    pub page: AppPage,
    pub section: Option<String>,
}

impl AppRoute {
    pub fn page(page: AppPage) -> Self {
        Self { page, section: None }
    }
    /// `settings/<section>`.
    pub fn settings(section: &str) -> Self {
        Self { page: AppPage::Settings, section: Some(section.to_string()) }
    }
    /// `parseRoute` (main/ipc.ts): section kept only if it matches `^[A-Za-z0-9_-]{1,64}$`.
    pub fn validated(page: AppPage, section: Option<&str>) -> Self {
        Self { page, section: section.filter(|s| is_valid_section(s)).map(str::to_string) }
    }
    /// `#/<page>` or `#/<page>/<section>` (section percent-encoded like `encodeURIComponent`).
    pub fn to_hash(&self) -> String {
        match &self.section {
            Some(s) if !s.is_empty() => format!("#/{}/{}", self.page.as_str(), encode_uri_component(s)),
            _ => format!("#/{}", self.page.as_str()),
        }
    }
    /// `parseHash` (ui-main.md §2.1): unknown page → home (section dropped); section = remaining non-empty segments
    /// joined with `/`, URI-decoded (a malformed escape keeps the raw text).
    pub fn parse_hash(hash: &str) -> Self {
        let path = hash.trim_start_matches('#').trim_start_matches('/');
        let path = path.split(['?', '#']).next().unwrap_or("");
        let mut segs = path.split('/');
        let head = segs.next().unwrap_or("");
        let Some(page) = AppPage::parse(head) else { return Self::default() };
        let rest: Vec<&str> = segs.filter(|s| !s.is_empty()).collect();
        let section = (!rest.is_empty()).then(|| {
            let raw = rest.join("/");
            decode_uri_component(&raw).unwrap_or(raw)
        });
        Self { page, section }
    }
    /// The settings section to scroll to, if this is a valid settings deep link.
    pub fn settings_section(&self) -> Option<&str> {
        (self.page == AppPage::Settings).then_some(self.section.as_deref()).flatten().filter(|s| is_settings_section(s))
    }
}

/// `^[A-Za-z0-9_-]{1,64}$`.
pub fn is_valid_section(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn encode_uri_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn decode_uri_component(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(h, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Toast kinds. The core never emits `Success` (UI-local toasts only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToastKind {
    Info,
    Success,
    Error,
}

impl ToastKind {
    /// App-window auto-dismiss duration (ms), before the action rule.
    pub fn duration_ms(self) -> u64 {
        use crate::constants::*;
        match self {
            Self::Info => TOAST_INFO_MS,
            Self::Success => TOAST_SUCCESS_MS,
            Self::Error => TOAST_ERROR_MS,
        }
    }
}

/// A toast action button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToastAction {
    pub label: String,
    /// Where the button navigates the main window (`None` = just show it).
    pub route: Option<AppRoute>,
}

/// A toast. `id` dedupes: a new toast with the same id replaces the old one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub id: String,
    pub kind: ToastKind,
    /// Optional bold title line (e.g. "History exported").
    pub title: Option<String>,
    pub message: String,
    pub action: Option<ToastAction>,
}

impl Toast {
    /// A toast with a fresh UUID id.
    pub fn new(kind: ToastKind, message: impl Into<String>) -> Self {
        Self { id: crate::ids::new_uuid(), kind, title: None, message: message.into(), action: None }
    }
    pub fn info(message: impl Into<String>) -> Self {
        Self::new(ToastKind::Info, message)
    }
    pub fn error(message: impl Into<String>) -> Self {
        Self::new(ToastKind::Error, message)
    }
    pub fn success(message: impl Into<String>) -> Self {
        Self::new(ToastKind::Success, message)
    }
    /// Sets a fixed dedupe id.
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }
    pub fn with_action(mut self, label: impl Into<String>, route: Option<AppRoute>) -> Self {
        self.action = Some(ToastAction { label: label.into(), route });
        self
    }
    /// `Open settings` → `settings/<section>`.
    pub fn with_settings_action(self, section: &str) -> Self {
        self.with_action("Open settings", Some(AppRoute::settings(section)))
    }
    /// App-window duration: kind default, at least 6000 ms with an action.
    pub fn app_duration_ms(&self) -> u64 {
        let d = self.kind.duration_ms();
        if self.action.is_some() {
            d.max(crate::constants::TOAST_ACTION_MIN_MS)
        } else {
            d
        }
    }
    /// Widget duration: 2500 ms, 5000 ms with an action.
    pub fn widget_duration_ms(&self) -> u64 {
        if self.action.is_some() {
            crate::constants::WIDGET_TOAST_ACTION_MS
        } else {
            crate::constants::WIDGET_TOAST_MS
        }
    }
}

/// App facts for Settings → About / Triggers / Integrations / Account (contracts.md §3.8; `electronVersion` replaced by
/// `runtime` + `build`, BRIEF.md "About shows the app version and build hash").
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AppInfo {
    pub name: String,
    pub version: String,
    /// e.g. `Rust 1.98 · Slint 1.18.1`.
    pub runtime: String,
    /// Short build hash (`git rev-parse --short=10 HEAD` at build time, `dev` when unknown).
    pub build: String,
    /// Ready-to-show notice for Settings → Account and the onboarding key step when the stored key could not be
    /// used: `copy::key_migration_failed()` / `copy::key_unreadable()`. `None` once a key is set or nothing failed.
    pub api_key_notice: Option<String>,
    pub is_packaged: bool,
    pub user_data_path: PathBuf,
    pub extension_path: PathBuf,
    /// Bound port while running, else the last requested port, else `settings.server.port`.
    pub server_port: u16,
    pub server_running: bool,
    /// `false` only after an install attempt of the global input hook failed ([`native_hook_available`] over the
    /// core's last `SelectionEvent::HookStatus`): before any attempt (chips off at launch, shots mode) it is `true`, so
    /// Settings → Triggers never shows the "hook unavailable" notice when nothing failed.
    pub native_hook_available: bool,
    pub explorer_menu_installed: bool,
    pub demo_mode: bool,
    /// Amazon Polly connection (no secrets); `None` when not connected.
    pub polly: Option<crate::providers::PollyStatus>,
}

/// `AppInfo.native_hook_available` from the core's hook status (contracts.md §3.8, §1.5; ARCHITECTURE.md §3.2
/// `Selection(HookStatus)`): `None` (no install attempt reported yet) and `Some(true)` → available; only
/// `Some(false)` (an attempt failed) → unavailable.
pub fn native_hook_available(hook_status: Option<bool>) -> bool {
    hook_status != Some(false)
}

/// Chip modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ChipMode {
    /// Beside the native context menu: "Read with Vox".
    #[default]
    Context,
    /// PopClip-style after a selection: "Read".
    Selection,
}

impl ChipMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Context => READ_WITH_LABEL,
            Self::Selection => READ_LABEL,
        }
    }
    /// The read source an activation produces.
    pub fn read_source(self) -> crate::history::ReadSource {
        match self {
            Self::Context => crate::history::ReadSource::ContextChip,
            Self::Selection => crate::history::ReadSource::SelectionPopup,
        }
    }
    /// Default window size before the UI reports one (DIP).
    pub fn default_size(self) -> (f64, f64) {
        match self {
            Self::Context => crate::constants::DEFAULT_CHIP_SIZE_CONTEXT,
            Self::Selection => crate::constants::DEFAULT_CHIP_SIZE_SELECTION,
        }
    }
}

/// Runtime flags from the environment (contracts.md §1.8, shell.md §1.1).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RuntimeFlags {
    /// `VOX_DEMO` or shots mode: separate `%APPDATA%\Vox-Demo` profile, seeded history, mock TTS.
    pub demo: bool,
    /// `demo || VOX_MOCK_TTS`.
    pub mock_tts: bool,
    /// `VOX_SHOTS_DIR` (any non-empty value), resolved to absolute by the caller.
    pub shots_dir: Option<PathBuf>,
    /// `VOX_DEBUG`.
    pub debug: bool,
}

impl RuntimeFlags {
    /// Reads the flags through `get` (`std::env::var` in production). A boolean flag is on only for exactly `1` or
    /// `true`.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let on = |k: &str| matches!(get(k).as_deref(), Some("1") | Some("true"));
        let shots_dir = get("VOX_SHOTS_DIR").filter(|v| !v.is_empty()).map(PathBuf::from);
        let demo = on("VOX_DEMO") || shots_dir.is_some();
        Self { demo, mock_tts: demo || on("VOX_MOCK_TTS"), shots_dir, debug: on("VOX_DEBUG") }
    }
    /// Screenshot-harness mode.
    pub fn shots(&self) -> bool {
        self.shots_dir.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_routes() {
        assert_eq!(AppRoute::parse_hash("#/settings/account"), AppRoute::settings("account"));
        assert_eq!(AppRoute::parse_hash("#/foo/bar"), AppRoute::default());
        assert_eq!(AppRoute::parse_hash("#///history//x//y"), AppRoute { page: AppPage::History, section: Some("x/y".into()) });
        assert_eq!(AppRoute::parse_hash(""), AppRoute::page(AppPage::Home));
        assert_eq!(AppRoute::parse_hash("#/settings/a%20b").section.as_deref(), Some("a b"));
        assert_eq!(AppRoute::parse_hash("#/settings/%E0").section.as_deref(), Some("%E0"));
        assert_eq!(AppRoute::settings("a b").to_hash(), "#/settings/a%20b");
        assert_eq!(AppRoute::validated(AppPage::Settings, Some("bad section")).section, None);
        assert_eq!(AppRoute::settings("triggers").settings_section(), Some("triggers"));
        assert_eq!(AppRoute::settings("nope").settings_section(), None);
        // ui-main.md §13 route vectors.
        assert_eq!(AppRoute::settings("a/b").to_hash(), "#/settings/a%2Fb");
        assert_eq!(AppRoute::parse_hash("#/settings/account?x=1"), AppRoute::settings("account"));
        assert_eq!(AppRoute::parse_hash("#/History"), AppRoute::page(AppPage::Home));
    }

    #[test]
    fn hook_availability() {
        assert!(native_hook_available(None));
        assert!(native_hook_available(Some(true)));
        assert!(!native_hook_available(Some(false)));
    }

    #[test]
    fn flags() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
        };
        let f = RuntimeFlags::from_lookup(env(&[("VOX_DEMO", "TRUE"), ("VOX_MOCK_TTS", "1")]));
        assert!(!f.demo && f.mock_tts);
        let f = RuntimeFlags::from_lookup(env(&[("VOX_SHOTS_DIR", "shots")]));
        assert!(f.demo && f.mock_tts && f.shots());
    }

    #[test]
    fn toasts() {
        let t = Toast::error("x").with_settings_action("account");
        assert_eq!(t.app_duration_ms(), 6_500);
        assert_eq!(Toast::info("x").with_action("Undo", None).app_duration_ms(), 6_000);
        assert_eq!(t.widget_duration_ms(), 5_000);
        assert_eq!(ChipMode::Selection.label(), "Read");
    }
}
