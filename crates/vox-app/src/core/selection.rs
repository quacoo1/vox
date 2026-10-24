//! Selection glue (shell.md §4.1): `Read{text, source, foreground}` → `reader.read` with
//! `sourceApp = appName, sourceProcess = processName, sourceTitle = title`; `NoSelection` → info toast
//! `Select some text first`; `Notice` → toast with its kind (password / private / password manager / clipboard
//! restore / **UIPI** `copy::elevated_app_refused()`); `HookStatus(b)` → log (`native input hook available` /
//! `… unavailable: chips are disabled, hotkeys/extension/tray still work`), store it as `hook_status = Some(b)` (the
//! core starts with `None`: no attempt yet) and publish a fresh `UiUpdate::AppInfo` with
//! `native_hook_available = vox_core::app::native_hook_available(hook_status)` (Settings → Triggers shows the hook
//! notice only after a failed attempt; tested with `None`, `Some(true)`, `Some(false)`). `ChipShow`/`ChipHide` never
//! reach the core: app.rs routes them straight to the UI thread (dropped while the UI is not attached).

use vox_core::read::{ForegroundInfo, ReadRequest};
use vox_core::history::ReadSource;

/// The read request for captured text (pure).
pub fn request_for(text: String, source: ReadSource, foreground: Option<&ForegroundInfo>) -> ReadRequest {
    let mut r = ReadRequest::new(text, source);
    if let Some(fg) = foreground {
        r.source_app = Some(fg.app_name.clone()).filter(|s| !s.is_empty());
        r.source_process = Some(fg.process_name.clone()).filter(|s| !s.is_empty());
        r.source_title = Some(fg.title.clone()).filter(|s| !s.is_empty());
    }
    r
}
