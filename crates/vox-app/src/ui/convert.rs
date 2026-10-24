//! Small converters from domain data to Slint model structs.
//!
//! **Credits per surface** ([`credits_data`]; ui-main.md §2.6, §3.6, §5.12, §6.1; ui-settings-widget.md §2.2;
//! D-UM-25; apostrophes curly, D-COPY-1). `ready` → the CreditsSummary fields (`percent` `39%`, `percent-accessible`
//! `39 percent of credits left`, `facts` `38.8K of 100K left`, `reset` `Resets in 12 days`, `fraction`, `low` below
//! 15 %, `detail`), plus the tier badge:
//!
//! | Surface | tier badge | `no-key` (title / message / action) | `restricted` | `error` |
//! |---|---|---|---|---|
//! | `Home` | `format::title_case_words` (empty → no badge) | `No API key` / `Add your ElevenLabs key to start reading.` / `Add key` | `Usage hidden` / `This key can’t read account usage. Reading still works.` / `Manage key` | `Credits unavailable` / `Couldn’t reach ElevenLabs.` / `Retry` |
//! | `Analytics` | `format::tier_label` (empty → `Plan`) | — / `Add your ElevenLabs API key to see your credit balance.` / `Add API key` | — / `This API key can’t read account details, so the balance isn’t available.` / no button | — / `Couldn’t reach ElevenLabs.` / `Retry` |
//! | `SettingsAccount` | `voices::title_case` (`starter_plus` → `Starter plus`) | — / `Add a key to see your monthly credits.` / — (empty meter) | — / `This key can’t read account info, so credits aren’t shown. Reading still works.` / — | — / `Couldn’t load your account: {error}` / `Try again` (an error with older data keeps the data) |
//! | `Sidebar` | — | the `add-key` button instead of the line | line hidden | line hidden |
//! | `Onboarding` | `title_case_words` | — | — | — |
//!
//! `CreditsState.demo` is produced **only** for `SettingsAccount` (its `Demo account` badge); Home, Analytics, Sidebar
//! and Onboarding map a demo `Ready` to `ready`. `refreshing` = `CreditsUpdate.refreshing` (the Analytics `Refresh
//! balance` button spins and is disabled while `refreshing || state == loading`, D-UM-20). Tests: every surface ×
//! state, including the §6.1 example (`39%`, `38.8K of 100K left`, `Resets in 12 days`).
//!
//! Details fixed here (the table leaves them open):
//! - Settings Account treats a `Ready` account with `characterLimit ≤ 0` as `restricted` (ui-settings-widget.md §2.2
//!   body 4: "data is null or characterLimit == 0"); the other surfaces show the summary (0 %) like the old app.
//! - The Sidebar's `no-key` block carries `action-label` = `Add your ElevenLabs key` (its warning button); the shell
//!   decides the button from `settings.hasApiKey` (ui-main.md §2.6), this block only supplies the copy.
//! - The error message of `SettingsAccount` is the cleaned error (`format::clean_error`).

use slint::{ModelRc, SharedString, VecModel};
use vox_core::app::{Toast, ToastKind};
use vox_core::format;
use vox_core::voices::AccountInfo;

use super::generated::{ChoiceItem, CreditsData, CreditsState, ToastData, ToastTone};
use crate::update::{CreditsUpdate, CreditsView};

/// Which surface a `CreditsData` is for (the copy differs per surface; module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreditsSurface {
    Sidebar,
    Home,
    Analytics,
    SettingsAccount,
    /// The onboarding key step's connected caption (tier badge only); part of the documented surface set.
    #[allow(dead_code)]
    Onboarding,
}

/// `[(value, label)]` → `[ChoiceItem]` (accessible label = label).
pub fn choices(items: &[(String, String)]) -> ModelRc<ChoiceItem> {
    let v: Vec<ChoiceItem> = items
        .iter()
        .map(|(value, label)| ChoiceItem { value: value.into(), label: label.into(), accessible_label: label.into() })
        .collect();
    ModelRc::new(VecModel::from(v))
}

/// Keycap labels for an accelerator (`vox_core::hotkey::display_labels`).
pub fn keycaps(accel: &str) -> ModelRc<SharedString> {
    let v: Vec<SharedString> = vox_core::hotkey::display_labels(accel).into_iter().map(Into::into).collect();
    ModelRc::new(VecModel::from(v))
}

/// A string list model.
pub fn strings(items: impl IntoIterator<Item = String>) -> ModelRc<SharedString> {
    let v: Vec<SharedString> = items.into_iter().map(Into::into).collect();
    ModelRc::new(VecModel::from(v))
}

/// An empty block in `state` (every text empty, meter at 0).
fn blank(state: CreditsState, refreshing: bool) -> CreditsData {
    CreditsData {
        state,
        title: SharedString::new(),
        tier: SharedString::new(),
        percent: SharedString::new(),
        percent_accessible: SharedString::new(),
        fraction: 0.0,
        low: false,
        facts: SharedString::new(),
        reset: SharedString::new(),
        detail: SharedString::new(),
        message: SharedString::new(),
        action_label: SharedString::new(),
        refreshing,
    }
}

/// A block with a message (and optionally a title and an action button).
fn notice(state: CreditsState, title: &str, message: &str, action: &str, refreshing: bool) -> CreditsData {
    CreditsData { title: title.into(), message: message.into(), action_label: action.into(), ..blank(state, refreshing) }
}

/// The CreditsSummary numbers (ui-main.md §6.1) for an account.
fn summary(state: CreditsState, account: &AccountInfo, tier: String, now_ms: f64, refreshing: bool) -> CreditsData {
    let count = if account.character_count.is_finite() { account.character_count } else { 0.0 };
    let limit = if account.character_limit.is_finite() { account.character_limit } else { 0.0 };
    let left = format::credits_left_fraction(count, limit);
    let pct = format::percent(left);
    let pct_number = pct.trim_end_matches('%').to_string();
    let remaining = (limit - count).max(0.0);
    let reset = format::reset_label(account.next_reset_unix, now_ms);
    let detail = format::credits_detail(remaining, limit.max(0.0), reset.as_deref());
    CreditsData {
        state,
        title: SharedString::new(),
        tier: tier.into(),
        percent: pct.into(),
        percent_accessible: format!("{pct_number} percent of credits left").into(),
        fraction: left as f32,
        low: format::credits_low(left),
        facts: format::credits_facts(remaining, limit.max(0.0)).into(),
        reset: reset.unwrap_or_default().into(),
        detail: detail.into(),
        message: SharedString::new(),
        action_label: SharedString::new(),
        refreshing,
    }
}

/// The credits block for one surface (module docs table).
pub fn credits_data(credits: &CreditsUpdate, surface: CreditsSurface, demo: bool, now_ms: f64) -> CreditsData {
    use CreditsSurface::*;
    let refreshing = credits.refreshing;
    match &credits.view {
        CreditsView::Loading => blank(CreditsState::Loading, refreshing),
        CreditsView::NoKey => match surface {
            Home => notice(CreditsState::NoKey, "No API key", "Add your ElevenLabs key to start reading.", "Add key", refreshing),
            Analytics => notice(CreditsState::NoKey, "", "Add your ElevenLabs API key to see your credit balance.", "Add API key", refreshing),
            SettingsAccount => notice(CreditsState::NoKey, "", "Add a key to see your monthly credits.", "", refreshing),
            Sidebar => notice(CreditsState::NoKey, "", "", "Add your ElevenLabs key", refreshing),
            Onboarding => blank(CreditsState::NoKey, refreshing),
        },
        CreditsView::Restricted => match surface {
            Home => notice(
                CreditsState::Restricted,
                "Usage hidden",
                "This key can\u{2019}t read account usage. Reading still works.",
                "Manage key",
                refreshing,
            ),
            Analytics => notice(
                CreditsState::Restricted,
                "",
                "This API key can\u{2019}t read account details, so the balance isn\u{2019}t available.",
                "",
                refreshing,
            ),
            SettingsAccount => notice(
                CreditsState::Restricted,
                "",
                "This key can\u{2019}t read account info, so credits aren\u{2019}t shown. Reading still works.",
                "",
                refreshing,
            ),
            Sidebar | Onboarding => blank(CreditsState::Restricted, refreshing),
        },
        CreditsView::Error(error) => match surface {
            Home => notice(CreditsState::Error, "Credits unavailable", "Couldn\u{2019}t reach ElevenLabs.", "Retry", refreshing),
            Analytics => notice(CreditsState::Error, "", "Couldn\u{2019}t reach ElevenLabs.", "Retry", refreshing),
            SettingsAccount => notice(
                CreditsState::Error,
                "",
                &format!("Couldn\u{2019}t load your account: {}", format::clean_error(error)),
                "Try again",
                refreshing,
            ),
            Sidebar | Onboarding => blank(CreditsState::Error, refreshing),
        },
        CreditsView::Ready(account) => match surface {
            Home | Onboarding => summary(CreditsState::Ready, account, format::title_case_words(&account.tier), now_ms, refreshing),
            Analytics => summary(CreditsState::Ready, account, format::tier_label(&account.tier), now_ms, refreshing),
            Sidebar => summary(CreditsState::Ready, account, String::new(), now_ms, refreshing),
            SettingsAccount => {
                // `characterLimit ≤ 0` (or not a number) means the key cannot read the numbers.
                if account.character_limit.is_nan() || account.character_limit <= 0.0 {
                    return notice(
                        CreditsState::Restricted,
                        "",
                        "This key can\u{2019}t read account info, so credits aren\u{2019}t shown. Reading still works.",
                        "",
                        refreshing,
                    );
                }
                let state = if demo { CreditsState::Demo } else { CreditsState::Ready };
                summary(state, account, vox_core::voices::title_case(&account.tier), now_ms, refreshing)
            }
        },
    }
}

/// The tone of a toast kind.
pub fn toast_tone(kind: ToastKind) -> ToastTone {
    match kind {
        ToastKind::Info => ToastTone::Info,
        ToastKind::Success => ToastTone::Success,
        ToastKind::Error => ToastTone::Error,
    }
}

/// A core or UI toast for the app stack (`leaving`, `key` from the stack entry) or the widget slot (`false`, its own
/// key).
pub fn toast_data(toast: &Toast, leaving: bool, key: u64) -> ToastData {
    ToastData {
        id: toast.id.as_str().into(),
        tone: toast_tone(toast.kind),
        title: toast.title.as_deref().unwrap_or_default().into(),
        message: toast.message.as_str().into(),
        action_label: toast.action.as_ref().map(|a| a.label.as_str()).unwrap_or_default().into(),
        leaving,
        // Slint ints are i32: wrap instead of saturating so a long session keeps producing new keys.
        key: (key % (i32::MAX as u64)) as i32,
    }
}

/// A Slint image from straight RGBA8 pixels (`None` when the buffer does not match the size).
pub fn rgba_image(rgba: &[u8], width: u32, height: u32) -> Option<slint::Image> {
    if width == 0 || height == 0 || rgba.len() != (width as usize) * (height as usize) * 4 {
        return None;
    }
    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(rgba, width, height);
    Some(slint::Image::from_rgba8(buffer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use vox_core::app::AppRoute;
    use vox_core::locale::{with_locale, Locale};

    const NOW_MS: f64 = 1_760_000_000_000.0;
    const DAY_MS: f64 = 86_400_000.0;

    fn account(count: f64, limit: f64, reset_days: Option<f64>, tier: &str) -> AccountInfo {
        AccountInfo {
            tier: tier.into(),
            character_count: count,
            character_limit: limit,
            next_reset_unix: reset_days.map(|d| (NOW_MS + d * DAY_MS - 3_600_000.0) / 1000.0),
            status: None,
        }
    }

    fn update(view: CreditsView) -> CreditsUpdate {
        CreditsUpdate { view, refreshing: false }
    }

    const ALL: [CreditsSurface; 5] =
        [CreditsSurface::Sidebar, CreditsSurface::Home, CreditsSurface::Analytics, CreditsSurface::SettingsAccount, CreditsSurface::Onboarding];

    #[test]
    fn spec_example_on_every_surface() {
        with_locale(Locale::EN_US, || {
            let u = update(CreditsView::Ready(account(61_234.0, 100_000.0, Some(12.0), "creator")));
            for surface in ALL {
                let c = credits_data(&u, surface, false, NOW_MS);
                assert_eq!(c.state, CreditsState::Ready, "{surface:?}");
                assert_eq!(c.percent, "39%");
                assert_eq!(c.percent_accessible, "39 percent of credits left");
                assert_eq!(c.facts, "38.8K of 100K left");
                assert_eq!(c.reset, "Resets in 12 days");
                assert_eq!(c.detail, "38,766 of 100,000 characters left. Resets in 12 days");
                assert!((c.fraction - 0.38766).abs() < 1e-5);
                assert!(!c.low);
                assert!(c.message.is_empty() && c.action_label.is_empty() && c.title.is_empty());
            }
        });
    }

    #[test]
    fn tier_badge_per_surface() {
        with_locale(Locale::EN_US, || {
            let u = update(CreditsView::Ready(account(10.0, 100.0, None, "starter_plus")));
            assert_eq!(credits_data(&u, CreditsSurface::Home, false, NOW_MS).tier, "Starter Plus");
            assert_eq!(credits_data(&u, CreditsSurface::Onboarding, false, NOW_MS).tier, "Starter Plus");
            assert_eq!(credits_data(&u, CreditsSurface::Analytics, false, NOW_MS).tier, "Starter Plus");
            assert_eq!(credits_data(&u, CreditsSurface::SettingsAccount, false, NOW_MS).tier, "Starter plus");
            assert_eq!(credits_data(&u, CreditsSurface::Sidebar, false, NOW_MS).tier, "");
            let empty = update(CreditsView::Ready(account(10.0, 100.0, None, "")));
            assert_eq!(credits_data(&empty, CreditsSurface::Home, false, NOW_MS).tier, "");
            assert_eq!(credits_data(&empty, CreditsSurface::Analytics, false, NOW_MS).tier, "Plan");
            // No reset → empty reset label and a detail without it.
            let c = credits_data(&u, CreditsSurface::Home, false, NOW_MS);
            assert_eq!(c.reset, "");
            assert_eq!(c.detail, "90 of 100 characters left");
        });
    }

    #[test]
    fn low_credits_and_zero_limit() {
        with_locale(Locale::EN_US, || {
            let u = update(CreditsView::Ready(account(460_388.0, 500_000.0, Some(3.0), "pro")));
            let c = credits_data(&u, CreditsSurface::Analytics, false, NOW_MS);
            assert_eq!(c.percent, "8%");
            assert!(c.low);
            assert_eq!(c.tier, "Pro");
            let zero = update(CreditsView::Ready(account(0.0, 0.0, None, "free")));
            let home = credits_data(&zero, CreditsSurface::Home, false, NOW_MS);
            assert_eq!((home.state, home.percent.as_str(), home.low), (CreditsState::Ready, "0%", true));
            let settings = credits_data(&zero, CreditsSurface::SettingsAccount, false, NOW_MS);
            assert_eq!(settings.state, CreditsState::Restricted);
            assert!(settings.message.contains("can\u{2019}t read account info"));
        });
    }

    #[test]
    fn no_key_per_surface() {
        with_locale(Locale::EN_US, || {
            let u = update(CreditsView::NoKey);
            let home = credits_data(&u, CreditsSurface::Home, false, NOW_MS);
            assert_eq!(
                (home.state, home.title.as_str(), home.message.as_str(), home.action_label.as_str()),
                (CreditsState::NoKey, "No API key", "Add your ElevenLabs key to start reading.", "Add key")
            );
            let an = credits_data(&u, CreditsSurface::Analytics, false, NOW_MS);
            assert_eq!(
                (an.title.as_str(), an.message.as_str(), an.action_label.as_str()),
                ("", "Add your ElevenLabs API key to see your credit balance.", "Add API key")
            );
            let st = credits_data(&u, CreditsSurface::SettingsAccount, false, NOW_MS);
            assert_eq!((st.message.as_str(), st.action_label.as_str(), st.fraction), ("Add a key to see your monthly credits.", "", 0.0));
            let sb = credits_data(&u, CreditsSurface::Sidebar, false, NOW_MS);
            assert_eq!((sb.state, sb.action_label.as_str()), (CreditsState::NoKey, "Add your ElevenLabs key"));
            let ob = credits_data(&u, CreditsSurface::Onboarding, false, NOW_MS);
            assert_eq!((ob.state, ob.message.as_str()), (CreditsState::NoKey, ""));
        });
    }

    #[test]
    fn restricted_per_surface() {
        with_locale(Locale::EN_US, || {
            let u = update(CreditsView::Restricted);
            let home = credits_data(&u, CreditsSurface::Home, false, NOW_MS);
            assert_eq!(
                (home.title.as_str(), home.message.as_str(), home.action_label.as_str()),
                ("Usage hidden", "This key can\u{2019}t read account usage. Reading still works.", "Manage key")
            );
            let an = credits_data(&u, CreditsSurface::Analytics, false, NOW_MS);
            assert_eq!(
                (an.message.as_str(), an.action_label.as_str()),
                ("This API key can\u{2019}t read account details, so the balance isn\u{2019}t available.", "")
            );
            let st = credits_data(&u, CreditsSurface::SettingsAccount, false, NOW_MS);
            assert_eq!(st.message, "This key can\u{2019}t read account info, so credits aren\u{2019}t shown. Reading still works.");
            for s in [CreditsSurface::Sidebar, CreditsSurface::Onboarding] {
                let c = credits_data(&u, s, false, NOW_MS);
                assert_eq!((c.state, c.message.as_str(), c.action_label.as_str()), (CreditsState::Restricted, "", ""));
            }
        });
    }

    #[test]
    fn error_per_surface() {
        with_locale(Locale::EN_US, || {
            let u = update(CreditsView::Error("TypeError: Can\u{2019}t reach ElevenLabs. Check your connection.".into()));
            let home = credits_data(&u, CreditsSurface::Home, false, NOW_MS);
            assert_eq!(
                (home.state, home.title.as_str(), home.message.as_str(), home.action_label.as_str()),
                (CreditsState::Error, "Credits unavailable", "Couldn\u{2019}t reach ElevenLabs.", "Retry")
            );
            let an = credits_data(&u, CreditsSurface::Analytics, false, NOW_MS);
            assert_eq!((an.title.as_str(), an.message.as_str(), an.action_label.as_str()), ("", "Couldn\u{2019}t reach ElevenLabs.", "Retry"));
            let st = credits_data(&u, CreditsSurface::SettingsAccount, false, NOW_MS);
            assert_eq!(st.message, "Couldn\u{2019}t load your account: Can\u{2019}t reach ElevenLabs. Check your connection.");
            assert_eq!(st.action_label, "Try again");
            let sb = credits_data(&u, CreditsSurface::Sidebar, false, NOW_MS);
            assert_eq!((sb.state, sb.message.as_str()), (CreditsState::Error, ""));
        });
    }

    #[test]
    fn loading_refreshing_and_demo() {
        with_locale(Locale::EN_US, || {
            let loading = CreditsUpdate { view: CreditsView::Loading, refreshing: true };
            for s in ALL {
                let c = credits_data(&loading, s, true, NOW_MS);
                assert_eq!((c.state, c.refreshing), (CreditsState::Loading, true));
            }
            let ready = CreditsUpdate { view: CreditsView::Ready(account(28_000.0, 100_000.0, Some(3.0), "demo")), refreshing: true };
            assert_eq!(credits_data(&ready, CreditsSurface::SettingsAccount, true, NOW_MS).state, CreditsState::Demo);
            assert_eq!(credits_data(&ready, CreditsSurface::SettingsAccount, true, NOW_MS).tier, "Demo");
            for s in [CreditsSurface::Sidebar, CreditsSurface::Home, CreditsSurface::Analytics, CreditsSurface::Onboarding] {
                let c = credits_data(&ready, s, true, NOW_MS);
                assert_eq!((c.state, c.refreshing, c.percent.as_str()), (CreditsState::Ready, true, "72%"));
            }
            assert_eq!(credits_data(&ready, CreditsSurface::SettingsAccount, false, NOW_MS).state, CreditsState::Ready);
        });
    }

    #[test]
    fn toast_data_maps_every_field() {
            let t = Toast::error("Couldn\u{2019}t open the browser").with_id("open-external").with_title("Oops").with_action("Open settings", Some(AppRoute::settings("account")));
            let d = toast_data(&t, true, 7);
            assert_eq!(d.id, "open-external");
            assert_eq!(d.tone, ToastTone::Error);
            assert_eq!(d.title, "Oops");
            assert_eq!(d.message, "Couldn\u{2019}t open the browser");
            assert_eq!(d.action_label, "Open settings");
            assert!(d.leaving);
            assert_eq!(d.key, 7);
            let plain = toast_data(&Toast::success("Saved"), false, (i32::MAX as u64) + 5);
            assert_eq!((plain.tone, plain.title.as_str(), plain.action_label.as_str(), plain.key), (ToastTone::Success, "", "", 5));
            assert_eq!(toast_data(&Toast::info("x"), false, 1).tone, ToastTone::Info);
    }

    #[test]
    fn rgba_image_checks_the_buffer() {
            assert!(rgba_image(&[0; 16], 2, 2).is_some());
            assert!(rgba_image(&[0; 15], 2, 2).is_none());
            assert!(rgba_image(&[], 0, 0).is_none());
    }
}
