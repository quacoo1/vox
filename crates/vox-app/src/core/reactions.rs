//! Settings reactions (shell.md §5.1) — the dispatcher run after every effective change with `(next, prev)`:
//!
//! | # | Condition | Reaction |
//! |---|---|---|
//! | 1 | always | publish `UiUpdate::Settings` (every surface), then `SettingsSaved{req}` when the patch carried a request id |
//! | 2 | `hotkeys_changed` | re-register hotkeys (unless suspended / shots) → failure toast |
//! | 3 | not shots | `selection.apply_settings` |
//! | 4 | `browserExtension` or `server.port` | `bridge::sync_server`, then `UiUpdate::AppInfo` (always, even when nothing changed: same port running) |
//! | 5 | `explorerMenu` | Explorer verb install/remove (forced; not in demo) → failure toast; then `UiUpdate::AppInfo` (always, also in demo) |
//! | 6 | `launchAtLogin` or `startMinimized` | launch at login (not in demo); then `UiUpdate::AppInfo` |
//! | 7 | `app.theme` | resolve with the OS theme → `UiUpdate::Theme` (the main window + its DWM caption colours; the widget and chip stay dark, D-USW-20) |
//! | 8 | `widget.enabled` true→false; else placement keys changed | `Widget(Hide)`; else `Widget(Reposition)` |
//! | 9 | `retentionDays` | arm the 60 s retention-change timer |
//! | 10 | `cacheLimitMB` decreased | prune now (log `cache limit lowered: evicted N cached reads`) |
//! | 11 | `mediaKeys` | `MediaSession::set_enabled` live (decision: no restart toast) |
//! | 12 | `playbackRate` / `volume` changed | apply to the reading player (only when the value changed: an unrelated write never overrides a session rate set from the bridge); `volume` also goes to the preview player while a sample is loading or playing (ui-settings-widget.md §4.6). Also runs **once at startup** (the core sends `Rate{playbackRate}` and `Volume{volume}` after publishing Settings, reader-playback.md §7) |
//! | 13 | `hasApiKey` changed or key replaced | credits refresh (forced), refetch voices/models |
//! | 14 | always | tray refresh happens on the UI side from `UiUpdate::Settings` |
//!
//! Row 3 runs only when a setting the selection service reads changed (`rightClickChip`, `selectionPopup`,
//! `excludedApps`, `restoreClipboard`; ARCHITECTURE.md §4.15) — `apply_settings` also retries a failed hook (D-SEL-1).

use vox_core::settings::{hotkeys_changed, Settings};

/// Which reactions apply (pure; tested).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reactions {
    pub hotkeys: bool,
    pub selection: bool,
    pub bridge: bool,
    pub explorer_menu: bool,
    pub launch_at_login: bool,
    pub theme: bool,
    pub hide_widget: bool,
    pub reposition_widget: bool,
    pub retention: bool,
    pub prune_cache: bool,
    pub media_keys: bool,
    pub player_rate: bool,
    pub player_volume: bool,
    pub key_changed: bool,
}

/// Compares `next` with `prev`. `key_replaced` is true when the API key changed while `has_api_key` stayed true.
pub fn diff(next: &Settings, prev: &Settings, key_replaced: bool, shots: bool) -> Reactions {
    let (nt, pt) = (&next.triggers, &prev.triggers);
    let (nw, pw) = (&next.widget, &prev.widget);
    let hide_widget = pw.enabled && !nw.enabled;
    let placement_changed = nw.position != pw.position || nw.custom_x != pw.custom_x || nw.custom_y != pw.custom_y;
    Reactions {
        hotkeys: hotkeys_changed(next, prev),
        selection: !shots
            && (nt.right_click_chip != pt.right_click_chip
                || nt.selection_popup != pt.selection_popup
                || nt.excluded_apps != pt.excluded_apps
                || nt.restore_clipboard != pt.restore_clipboard),
        bridge: nt.browser_extension != pt.browser_extension || next.server.port != prev.server.port,
        explorer_menu: nt.explorer_menu != pt.explorer_menu,
        launch_at_login: next.app.launch_at_login != prev.app.launch_at_login
            || next.app.start_minimized != prev.app.start_minimized,
        theme: next.app.theme != prev.app.theme,
        hide_widget,
        reposition_widget: !hide_widget && nw.enabled && placement_changed,
        retention: next.library.retention_days != prev.library.retention_days,
        prune_cache: next.library.cache_limit_mb < prev.library.cache_limit_mb,
        media_keys: nt.media_keys != pt.media_keys,
        player_rate: next.playback_rate != prev.playback_rate,
        player_volume: next.volume != prev.volume,
        key_changed: next.has_api_key != prev.has_api_key || key_replaced,
    }
}

#[cfg(test)]
mod tests {
    use vox_core::settings::{ThemeMode, WidgetPosition};

    use super::*;

    fn base() -> Settings {
        Settings::default()
    }

    fn only(r: Reactions) -> Vec<&'static str> {
        let mut v = Vec::new();
        let pairs = [
            ("hotkeys", r.hotkeys),
            ("selection", r.selection),
            ("bridge", r.bridge),
            ("explorer_menu", r.explorer_menu),
            ("launch_at_login", r.launch_at_login),
            ("theme", r.theme),
            ("hide_widget", r.hide_widget),
            ("reposition_widget", r.reposition_widget),
            ("retention", r.retention),
            ("prune_cache", r.prune_cache),
            ("media_keys", r.media_keys),
            ("player_rate", r.player_rate),
            ("player_volume", r.player_volume),
            ("key_changed", r.key_changed),
        ];
        for (n, on) in pairs {
            if on {
                v.push(n);
            }
        }
        v
    }

    fn changed(f: impl FnOnce(&mut Settings)) -> Vec<&'static str> {
        let prev = base();
        let mut next = prev.clone();
        f(&mut next);
        only(diff(&next, &prev, false, false))
    }

    #[test]
    fn no_change_no_reaction() {
        assert!(changed(|_| {}).is_empty());
        // Next-read settings react nowhere (§4.15 rows "next read").
        assert!(changed(|s| s.voice_id = "x".into()).is_empty());
        assert!(changed(|s| s.model_id = "x".into()).is_empty());
        assert!(changed(|s| s.library.save_history = !s.library.save_history).is_empty());
        assert!(changed(|s| s.library.cache_audio = !s.library.cache_audio).is_empty());
        assert!(changed(|s| s.app.close_to_tray = !s.app.close_to_tray).is_empty());
        assert!(changed(|s| s.widget.auto_hide = !s.widget.auto_hide).is_empty());
        assert!(changed(|s| s.onboarded = !s.onboarded).is_empty());
    }

    #[test]
    fn every_row() {
        assert_eq!(changed(|s| s.triggers.hotkeys.stop = "Ctrl+Alt+X".into()), ["hotkeys"]);
        assert_eq!(changed(|s| s.triggers.hotkeys_enabled = false), ["hotkeys"]);
        assert_eq!(changed(|s| s.triggers.right_click_chip = !s.triggers.right_click_chip), ["selection"]);
        assert_eq!(changed(|s| s.triggers.selection_popup = !s.triggers.selection_popup), ["selection"]);
        assert_eq!(changed(|s| s.triggers.excluded_apps = vec!["x.exe".into()]), ["selection"]);
        assert_eq!(changed(|s| s.triggers.restore_clipboard = !s.triggers.restore_clipboard), ["selection"]);
        assert_eq!(changed(|s| s.triggers.browser_extension = false), ["bridge"]);
        assert_eq!(changed(|s| s.server.port = 50_000), ["bridge"]);
        assert_eq!(changed(|s| s.triggers.explorer_menu = true), ["explorer_menu"]);
        assert_eq!(changed(|s| s.app.launch_at_login = true), ["launch_at_login"]);
        assert_eq!(changed(|s| s.app.start_minimized = true), ["launch_at_login"]);
        assert_eq!(changed(|s| s.app.theme = ThemeMode::Night), ["theme"]);
        assert_eq!(changed(|s| s.widget.enabled = false), ["hide_widget"]);
        assert_eq!(changed(|s| s.widget.position = WidgetPosition::TopRight), ["reposition_widget"]);
        assert_eq!(changed(|s| s.widget.custom_x = Some(10)), ["reposition_widget"]);
        assert_eq!(changed(|s| s.widget.custom_y = Some(10)), ["reposition_widget"]);
        assert_eq!(changed(|s| s.library.retention_days = 30), ["retention"]);
        assert_eq!(changed(|s| s.library.cache_limit_mb = 100), ["prune_cache"]);
        assert_eq!(changed(|s| s.triggers.media_keys = false), ["media_keys"]);
        assert_eq!(changed(|s| s.playback_rate = 1.5), ["player_rate"]);
        assert_eq!(changed(|s| s.volume = 0.5), ["player_volume"]);
        assert_eq!(changed(|s| s.has_api_key = !s.has_api_key), ["key_changed"]);
    }

    #[test]
    fn widget_rules() {
        // Disabling while moving: hide only (the reposition is skipped).
        assert_eq!(
            changed(|s| {
                s.widget.enabled = false;
                s.widget.position = WidgetPosition::Remember;
            }),
            ["hide_widget"]
        );
        // Turning the widget on shows nothing (it appears with the next read or toast).
        let mut prev = base();
        prev.widget.enabled = false;
        let mut next = prev.clone();
        next.widget.enabled = true;
        assert!(only(diff(&next, &prev, false, false)).is_empty());
    }

    #[test]
    fn cache_limit_increase_does_nothing() {
        let mut prev = base();
        prev.library.cache_limit_mb = 100;
        let mut next = prev.clone();
        next.library.cache_limit_mb = 200;
        assert!(only(diff(&next, &prev, false, false)).is_empty());
    }

    #[test]
    fn key_replacement_and_shots_mode() {
        let s = base();
        assert_eq!(only(diff(&s, &s, true, false)), ["key_changed"]);
        let mut next = base();
        next.triggers.right_click_chip = !next.triggers.right_click_chip;
        assert!(only(diff(&next, &base(), false, true)).is_empty(), "the selection service never runs in shots mode");
    }

    /// reader-playback.md §12 #30: a settings write that does not change `playbackRate`/`volume` never overrides a
    /// rate/volume set by a command.
    #[test]
    fn unrelated_writes_never_reapply_rate_or_volume() {
        let r = diff(
            &{
                let mut s = base();
                s.app.theme = ThemeMode::Paper;
                s
            },
            &base(),
            false,
            false,
        );
        assert!(!r.player_rate && !r.player_volume);
    }
}
