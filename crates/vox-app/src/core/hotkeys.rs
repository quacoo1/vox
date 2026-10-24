//! Hotkey glue (shell.md §8): `vox_core::hotkey::plan_registration(triggers, vox_win::hotkeys::alt_gr_character)`
//! → `HotkeyService::register` (failures come back sorted in registration order) → warn line `hotkeys not
//! registered: <vox_core::hotkey::failures_log_line>` and the failure toast (`describe_failures`, kind error, "Open
//! settings" → settings/triggers) — at startup, on every hotkey change and when a suspension ends. Not in shots mode,
//! not while suspended. Actions: readSelection → `selection.read_selection(Hotkey)`, readClipboard → reader,
//! playPause → `toggle_playback`, stop → `reader::command(Stop)` (which also hides the widget when a session existed).
//!
//! Suspension (`app:suspend-shortcuts`, shell.md §8.7) is the pure [`HotkeyState`] machine (tested): `suspend` while
//! not suspended unregisters every hotkey; `suspend` always (re)arms the 30 s `TimerId::HotkeySuspend` safety timer
//! (true twice re-arms); `resume` while not suspended does nothing; `resume` cancels the timer and re-registers from
//! the **current** settings (the recorder sends its `UpdateSettings` before `SuspendHotkeys(false)` on the same FIFO
//! channel, so the new accelerator is what gets registered); the timer firing resumes the same way and publishes
//! `UiUpdate::HotkeysResumed` (the recorder stops recording). A non-`true` IPC payload counted as false in the
//! original; here the command is typed (`SuspendHotkeys(bool)`).

use vox_core::hotkey::{sort_failures, HotkeyFailure, HotkeyFailureReason, RegistrationPlan};

/// Suspension state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HotkeyState {
    pub suspended: bool,
}

/// What the core must do after a suspension transition.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HotkeyEffects {
    /// `HotkeyService::unregister_all`.
    pub unregister: bool,
    /// Re-register from the current settings (plan → register → failure toast).
    pub register: bool,
    /// (Re)arm `TimerId::HotkeySuspend` (30 s).
    pub arm_timer: bool,
    pub cancel_timer: bool,
    /// Publish `UiUpdate::HotkeysResumed`.
    pub notify_resumed: bool,
}

impl HotkeyState {
    /// `SuspendHotkeys(true)`.
    pub fn suspend(&mut self) -> HotkeyEffects {
        let unregister = !self.suspended;
        self.suspended = true;
        HotkeyEffects { unregister, arm_timer: true, ..HotkeyEffects::default() }
    }
    /// `SuspendHotkeys(false)`.
    pub fn resume(&mut self) -> HotkeyEffects {
        if !self.suspended {
            return HotkeyEffects::default();
        }
        self.suspended = false;
        HotkeyEffects { register: true, cancel_timer: true, ..HotkeyEffects::default() }
    }
    /// The 30 s safety timer fired.
    pub fn on_timer(&mut self) -> HotkeyEffects {
        if !self.suspended {
            return HotkeyEffects::default();
        }
        self.suspended = false;
        HotkeyEffects { register: true, notify_resumed: true, ..HotkeyEffects::default() }
    }
}

/// The failures to report when the hotkey thread could not start: the plan's own failures plus `InUse` for every
/// accelerator that would have been registered, in registration order.
pub fn failures_without_service(plan: &RegistrationPlan) -> Vec<HotkeyFailure> {
    let mut failures = plan.failures.clone();
    failures.extend(plan.register.iter().map(|(action, accel, _)| HotkeyFailure {
        action: *action,
        accelerator: accel.clone(),
        reason: HotkeyFailureReason::InUse,
    }));
    sort_failures(&mut failures);
    failures
}

#[cfg(test)]
mod tests {
    use vox_core::settings::Settings;

    use super::*;

    #[test]
    fn suspend_twice_rearms_without_unregistering_again() {
        let mut s = HotkeyState::default();
        let e = s.suspend();
        assert!(e.unregister && e.arm_timer && !e.register);
        let e = s.suspend();
        assert!(!e.unregister && e.arm_timer, "true twice re-arms the safety timer");
        assert!(s.suspended);
    }

    #[test]
    fn resume_while_not_suspended_does_nothing() {
        let mut s = HotkeyState::default();
        assert_eq!(s.resume(), HotkeyEffects::default());
        assert_eq!(s.on_timer(), HotkeyEffects::default());
    }

    #[test]
    fn resume_reregisters_and_cancels_the_timer() {
        let mut s = HotkeyState::default();
        s.suspend();
        let e = s.resume();
        assert!(e.register && e.cancel_timer && !e.notify_resumed && !e.unregister);
        assert!(!s.suspended);
    }

    #[test]
    fn the_timer_resumes_and_notifies_the_recorder() {
        let mut s = HotkeyState::default();
        s.suspend();
        let e = s.on_timer();
        assert!(e.register && e.notify_resumed && !e.arm_timer);
        assert!(!s.suspended);
    }

    #[test]
    fn without_a_hotkey_thread_every_accelerator_is_in_use() {
        let mut settings = Settings::default();
        settings.triggers.hotkeys.play_pause = "Ctrl+C".into(); // reserved
        let plan = vox_core::hotkey::plan_registration(&settings.triggers, |_, _| None);
        let failures = failures_without_service(&plan);
        let ids: Vec<(&str, &str)> = failures.iter().map(|f| (f.action.id(), f.reason.id())).collect();
        assert_eq!(ids, [("readSelection", "in-use"), ("readClipboard", "in-use"), ("playPause", "reserved"), ("stop", "in-use")]);
    }
}
