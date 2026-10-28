//! The selection service on Linux. There is no global mouse hook for an X11 client under Wayland, so the right-click
//! chip and the selection popup are unavailable (reported as `HookStatus(false)` when they are switched on, which shows
//! the Settings → Triggers notice). "Read selection" (hotkey, tray, CLI) reads the PRIMARY selection: the text that is
//! selected right now in any app, without touching the clipboard.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use vox_core::app::ChipMode;
use vox_core::history::ReadSource;
use vox_core::read::ForegroundInfo;
use vox_core::settings::Settings;

use crate::window::PhysRect;

/// The settings the service consumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionConfig {
    pub right_click_chip: bool,
    pub selection_popup: bool,
    /// Normalised lower-case exe names.
    pub excluded_apps: Vec<String>,
    pub restore_clipboard: bool,
}

impl From<&Settings> for SelectionConfig {
    fn from(s: &Settings) -> Self {
        Self {
            right_click_chip: s.triggers.right_click_chip,
            selection_popup: s.triggers.selection_popup,
            excluded_apps: s.triggers.excluded_apps.clone(),
            restore_clipboard: s.triggers.restore_clipboard,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Info,
    Error,
}

/// What the service reports.
#[derive(Debug, Clone, PartialEq)]
pub enum SelectionEvent {
    Read { text: String, source: ReadSource, foreground: Option<ForegroundInfo> },
    NoSelection(ReadSource),
    Notice { kind: NoticeKind, message: String },
    ChipShow { x: i32, y: i32, mode: ChipMode },
    ChipHide,
    HookStatus(bool),
}

pub type SelectionEvents = Arc<dyn Fn(SelectionEvent) + Send + Sync>;

/// Thread-safe handle.
#[derive(Clone)]
pub struct SelectionService {
    events: SelectionEvents,
    busy: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
}

impl SelectionService {
    pub fn new(events: SelectionEvents) -> Self {
        SelectionService { events, busy: Arc::new(AtomicBool::new(false)), stopped: Arc::new(AtomicBool::new(false)) }
    }

    pub fn start(&self, cfg: SelectionConfig) {
        self.stopped.store(false, Ordering::SeqCst);
        self.apply_settings(cfg);
    }

    pub fn apply_settings(&self, cfg: SelectionConfig) {
        if cfg.right_click_chip || cfg.selection_popup {
            (self.events)(SelectionEvent::HookStatus(false));
        }
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }

    pub fn flush(&self, _timeout: Duration) {}

    pub fn hook_available(&self) -> bool {
        false
    }

    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    /// Never blocks: reads PRIMARY on a helper thread and reports `Read` or `NoSelection`.
    pub fn read_selection(&self, source: ReadSource) {
        if self.stopped.load(Ordering::SeqCst) || self.busy.swap(true, Ordering::SeqCst) {
            return;
        }
        let events = self.events.clone();
        let busy = self.busy.clone();
        let spawned = std::thread::Builder::new().name("vox-selection".into()).spawn(move || {
            let text = super::clipboard::read_primary_text().map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
            busy.store(false, Ordering::SeqCst);
            match text {
                Some(text) => events(SelectionEvent::Read { text, source, foreground: None }),
                None => events(SelectionEvent::NoSelection(source)),
            }
        });
        if spawned.is_err() {
            self.busy.store(false, Ordering::SeqCst);
        }
    }

    pub fn set_chip_bounds(&self, _bounds: Option<PhysRect>) {}

    pub fn chip_activated(&self) {}

    pub fn chip_dismissed(&self) {}
}
