//! What the tray shows (shell.md §7): icon active while playing/loading; tooltip `format::tray_tooltip`; the menu is
//! rebuilt only when its key changes (status bucket, right-click chip, widget enabled, hotkeys enabled, the four
//! hotkey strings); hints only for `hotkey::tray_hint_ok` strings. Hints are display text appended after a tab
//! (`{label}` + TAB + `{hint}`), never registered as menu accelerators (`tray` module docs).
//!
//! The menu key holds the three hints instead of the four raw strings: the read-selection hotkey is not in the menu,
//! and a hint is a pure function of `hotkeysEnabled` + its string, so the key changes exactly when the menu would.

use vox_core::hotkey::HotkeyAction;
use vox_core::player::{PlayerState, PlayerStatus};
use vox_core::settings::Settings;

/// The tray's full desired state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayView {
    pub active: bool,
    pub tooltip: String,
    /// `playing` | `paused` | `loading` | `idle`.
    pub status_bucket: &'static str,
    pub right_click_chip: bool,
    pub widget_enabled: bool,
    pub hotkeys_enabled: bool,
    /// Display-only hints (`None` = no hint).
    pub hint_read_clipboard: Option<String>,
    pub hint_play_pause: Option<String>,
    pub hint_stop: Option<String>,
}

/// The status bucket of shell.md §7.4 (`playing` / `paused` / `loading`, anything else `idle`).
pub fn status_bucket(status: PlayerStatus) -> &'static str {
    match status {
        PlayerStatus::Playing => "playing",
        PlayerStatus::Paused => "paused",
        PlayerStatus::Loading => "loading",
        _ => "idle",
    }
}

impl TrayView {
    /// Pure mapping (tested).
    pub fn build(settings: &Settings, state: &PlayerState) -> TrayView {
        let triggers = &settings.triggers;
        let hint = |action: HotkeyAction| -> Option<String> {
            let accel = triggers.hotkeys.get(action);
            (triggers.hotkeys_enabled && !accel.is_empty() && vox_core::hotkey::tray_hint_ok(accel)).then(|| accel.to_string())
        };
        TrayView {
            active: matches!(state.status, PlayerStatus::Playing | PlayerStatus::Loading),
            tooltip: vox_core::format::tray_tooltip(state),
            status_bucket: status_bucket(state.status),
            right_click_chip: triggers.right_click_chip,
            widget_enabled: settings.widget.enabled,
            hotkeys_enabled: triggers.hotkeys_enabled,
            hint_read_clipboard: hint(HotkeyAction::ReadClipboard),
            hint_play_pause: hint(HotkeyAction::PlayPause),
            hint_stop: hint(HotkeyAction::Stop),
        }
    }
    /// The menu rebuild key.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn menu_key(&self) -> String {
        format!(
            "{}|{}|{}|{}|{:?}|{:?}|{:?}",
            self.status_bucket, self.right_click_chip, self.widget_enabled, self.hotkeys_enabled, self.hint_read_clipboard, self.hint_play_pause, self.hint_stop
        )
    }
    /// `Pause` / `Resume` item label (shell.md §7.3 row 2).
    pub fn pause_label(&self) -> &'static str {
        if self.status_bucket == "paused" {
            "Resume"
        } else {
            "Pause"
        }
    }
    /// Row 2 is enabled while playing or paused.
    pub fn pause_enabled(&self) -> bool {
        matches!(self.status_bucket, "playing" | "paused")
    }
    /// Row 3 (`Stop`) is enabled while playing, paused or loading.
    pub fn stop_enabled(&self) -> bool {
        matches!(self.status_bucket, "playing" | "paused" | "loading")
    }
}

/// A menu label with its display-only hint after a tab (Win32 menus right-align the text after `\t`).
pub fn with_hint(label: &str, hint: Option<&str>) -> String {
    match hint {
        Some(h) if !h.is_empty() => format!("{label}\t{h}"),
        _ => label.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(status: PlayerStatus, title: &str) -> PlayerState {
        PlayerState { status, title: title.into(), ..PlayerState::idle() }
    }

    #[test]
    fn active_tooltip_and_bucket_follow_the_status() {
        let s = Settings::default();
        let v = TrayView::build(&s, &state(PlayerStatus::Playing, "An essay"));
        assert!(v.active);
        assert_eq!(v.status_bucket, "playing");
        assert_eq!(v.tooltip, "Vox \u{00B7} Reading \u{201C}An essay\u{201D}");
        assert_eq!((v.pause_label(), v.pause_enabled(), v.stop_enabled()), ("Pause", true, true));

        let v = TrayView::build(&s, &state(PlayerStatus::Loading, ""));
        assert!(v.active);
        assert_eq!(v.status_bucket, "loading");
        assert_eq!(v.tooltip, "Vox \u{00B7} Preparing audio\u{2026}");
        assert_eq!((v.pause_label(), v.pause_enabled(), v.stop_enabled()), ("Pause", false, true));

        let v = TrayView::build(&s, &state(PlayerStatus::Paused, ""));
        assert!(!v.active);
        assert_eq!(v.tooltip, "Vox \u{00B7} Paused");
        assert_eq!((v.pause_label(), v.pause_enabled(), v.stop_enabled()), ("Resume", true, true));

        for st in [PlayerStatus::Idle, PlayerStatus::Ended, PlayerStatus::Error] {
            let v = TrayView::build(&s, &state(st, "x"));
            assert!(!v.active);
            assert_eq!(v.status_bucket, "idle");
            assert_eq!(v.tooltip, "Vox");
            assert_eq!((v.pause_label(), v.pause_enabled(), v.stop_enabled()), ("Pause", false, false));
        }
    }

    #[test]
    fn hints_only_for_enabled_simple_accelerators() {
        let mut s = Settings::default();
        s.triggers.hotkeys.read_clipboard = "Ctrl+Alt+V".into();
        s.triggers.hotkeys.play_pause = "Ctrl+Alt+;".into();
        s.triggers.hotkeys.stop = String::new();
        let v = TrayView::build(&s, &PlayerState::idle());
        assert_eq!(v.hint_read_clipboard.as_deref(), Some("Ctrl+Alt+V"));
        assert_eq!(v.hint_play_pause, None);
        assert_eq!(v.hint_stop, None);
        s.triggers.hotkeys_enabled = false;
        let off = TrayView::build(&s, &PlayerState::idle());
        assert_eq!((off.hint_read_clipboard, off.hotkeys_enabled), (None, false));
        assert_eq!(with_hint("Read clipboard", Some("Ctrl+Alt+V")), "Read clipboard\tCtrl+Alt+V");
        assert_eq!(with_hint("Stop", None), "Stop");
    }

    #[test]
    fn menu_key_changes_only_with_menu_inputs() {
        let mut s = Settings::default();
        let a = TrayView::build(&s, &state(PlayerStatus::Playing, "one"));
        let b = TrayView::build(&s, &state(PlayerStatus::Playing, "two"));
        assert_ne!(a.tooltip, b.tooltip);
        assert_eq!(a.menu_key(), b.menu_key());
        // Ended / error / idle share the idle bucket.
        let e = TrayView::build(&s, &state(PlayerStatus::Ended, ""));
        let i = TrayView::build(&s, &PlayerState::idle());
        assert_eq!(e.menu_key(), i.menu_key());
        assert_ne!(a.menu_key(), i.menu_key());
        // Checkbox values and hotkeys change it.
        s.widget.enabled = !s.widget.enabled;
        assert_ne!(TrayView::build(&s, &PlayerState::idle()).menu_key(), i.menu_key());
        let mut s2 = Settings::default();
        s2.triggers.right_click_chip = !s2.triggers.right_click_chip;
        assert_ne!(TrayView::build(&s2, &PlayerState::idle()).menu_key(), i.menu_key());
        let mut s3 = Settings::default();
        s3.triggers.hotkeys.stop = "Ctrl+Alt+F9".into();
        assert_ne!(TrayView::build(&s3, &PlayerState::idle()).menu_key(), i.menu_key());
    }
}
