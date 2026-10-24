//! Toast routing (reader-playback.md §2.11, contracts.md §5.3 `app:toast`, shell.md §5.3).
//!
//! Every core toast (core-raised, or `Command::Toast` from a core worker) is published once as `UiUpdate::Toast` (the
//! main window's stack and the widget both receive it). Then: main window visible **and** focused → nothing else;
//! else `widget.enabled` → `UiUpdate::Widget(Show)` (the widget shows it as a banner on the player card, or a
//! standalone card when idle, then hides itself again); else, for kinds other than `success`, a silent OS notification
//! (`vox_win::shell::show_notification("Vox", msg)`; the first failure of the session is logged at warn level, scope
//! `notify`, later ones are ignored). There is no widget-readiness wait (the widget exists before the core starts).
//!
//! **Startup** (integrations.md §1.3, shell.md §3.4/§5.3): the router's `ui_ready` is false until the first
//! `WindowEvent::MainState` (which `AppUi::create` sends right after the startup visibility decision). Toasts raised
//! while `!ui_ready` (hotkey failures, the bridge port, the Explorer verb) are published at once, but their extra route
//! (`Widget(Show)` / OS notification) is decided when that first `MainState` arrives — so a startup failure toast does
//! not pop the widget next to a main window that is opening focused.

use vox_core::app::{Toast, ToastKind};

/// What the router needs to know about the windows (kept up to date from `WindowEvent`s).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SurfaceState {
    pub main_visible: bool,
    pub main_focused: bool,
    pub widget_visible: bool,
}

/// Where a toast goes besides the two UI surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastRoute {
    /// The focused main window shows it.
    AppOnly,
    /// Show the widget (no activation) so it can display the toast.
    ShowWidget,
    /// Neither: a silent OS notification.
    OsNotification,
    /// Neither and a success toast: nothing more.
    Nowhere,
}

/// Pure routing decision.
pub fn route(toast: &Toast, surfaces: SurfaceState, widget_enabled: bool) -> ToastRoute {
    if surfaces.main_visible && surfaces.main_focused {
        ToastRoute::AppOnly
    } else if widget_enabled {
        ToastRoute::ShowWidget
    } else if toast.kind != ToastKind::Success {
        ToastRoute::OsNotification
    } else {
        ToastRoute::Nowhere
    }
}

/// The router's state (pure; tested: routing before the UI is ready is deferred and resolved by the first
/// `MainState`).
#[derive(Debug, Default)]
pub struct ToastRouter {
    pub surfaces: SurfaceState,
    /// The first `WindowEvent::MainState` arrived.
    pub ui_ready: bool,
    /// Toasts published before `ui_ready` whose extra route is still to be decided.
    deferred: Vec<Toast>,
    /// A notification failure was already logged this session.
    pub notify_failure_logged: bool,
}

impl ToastRouter {
    /// A new core toast: returns its extra route now (`None` = deferred until the UI is ready).
    pub fn on_toast(&mut self, toast: &Toast, widget_enabled: bool) -> Option<ToastRoute> {
        if !self.ui_ready {
            self.deferred.push(toast.clone());
            return None;
        }
        Some(route(toast, self.surfaces, widget_enabled))
    }
    /// A `MainState` report: updates the surfaces; on the first one, returns the routes of the deferred toasts.
    pub fn on_main_state(&mut self, visible: bool, focused: bool, widget_enabled: bool) -> Vec<(Toast, ToastRoute)> {
        self.surfaces.main_visible = visible;
        self.surfaces.main_focused = focused;
        if self.ui_ready {
            return Vec::new();
        }
        self.ui_ready = true;
        let surfaces = self.surfaces;
        std::mem::take(&mut self.deferred)
            .into_iter()
            .map(|t| {
                let r = route(&t, surfaces, widget_enabled);
                (t, r)
            })
            .collect()
    }
    /// The widget became visible / hidden.
    pub fn on_widget_visible(&mut self, visible: bool) {
        self.surfaces.widget_visible = visible;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn surfaces(main_visible: bool, main_focused: bool) -> SurfaceState {
        SurfaceState { main_visible, main_focused, widget_visible: false }
    }

    #[test]
    fn routing_rules() {
        let err = Toast::error("e");
        let ok = Toast::success("s");
        assert_eq!(route(&err, surfaces(true, true), true), ToastRoute::AppOnly);
        // Visible but unfocused: the widget is the fallback surface (D-USW-11).
        assert_eq!(route(&err, surfaces(true, false), true), ToastRoute::ShowWidget);
        assert_eq!(route(&err, surfaces(false, false), true), ToastRoute::ShowWidget);
        assert_eq!(route(&err, surfaces(false, false), false), ToastRoute::OsNotification);
        assert_eq!(route(&Toast::info("i"), surfaces(true, false), false), ToastRoute::OsNotification);
        assert_eq!(route(&ok, surfaces(false, false), false), ToastRoute::Nowhere);
        assert_eq!(route(&ok, surfaces(true, true), false), ToastRoute::AppOnly);
    }

    #[test]
    fn toasts_before_the_first_main_state_are_deferred() {
        let mut r = ToastRouter::default();
        let a = Toast::error("hotkeys");
        let b = Toast::error("bridge");
        assert_eq!(r.on_toast(&a, true), None);
        assert_eq!(r.on_toast(&b, true), None);
        // The main window opens focused: the deferred toasts need nothing more.
        let routes = r.on_main_state(true, true, true);
        assert_eq!(routes, vec![(a, ToastRoute::AppOnly), (b, ToastRoute::AppOnly)]);
        assert!(r.ui_ready);
        // Later MainStates only update the surfaces.
        assert!(r.on_main_state(false, false, true).is_empty());
        assert_eq!(r.on_toast(&Toast::info("x"), true), Some(ToastRoute::ShowWidget));
    }

    #[test]
    fn deferred_toasts_with_a_hidden_main_window_use_the_widget_or_the_os() {
        let mut r = ToastRouter::default();
        let a = Toast::error("x");
        r.on_toast(&a, false);
        assert_eq!(r.on_main_state(false, false, false), vec![(a, ToastRoute::OsNotification)]);
        r.on_widget_visible(true);
        assert!(r.surfaces.widget_visible);
    }
}
