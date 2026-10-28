//! Plain-text CLIPBOARD and PRIMARY access through arboard (X11; Mutter mirrors both selections between Wayland and
//! XWayland clients). One clipboard object lives for the whole process so text Vox copies stays available.

use parking_lot::Mutex;

fn with_clipboard<R>(f: impl FnOnce(&mut arboard::Clipboard) -> R) -> Option<R> {
    static CLIPBOARD: Mutex<Option<arboard::Clipboard>> = Mutex::new(None);
    let mut guard = CLIPBOARD.lock();
    if guard.is_none() {
        match arboard::Clipboard::new() {
            Ok(c) => *guard = Some(c),
            Err(e) => {
                tracing::warn!(target: "clipboard", "clipboard unavailable: {e}");
                return None;
            }
        }
    }
    guard.as_mut().map(f)
}

/// The CLIPBOARD text, `None` when empty or not text.
pub fn read_clipboard_text() -> Option<String> {
    with_clipboard(|c| c.get_text().ok()).flatten().filter(|t| !t.is_empty())
}

pub fn write_clipboard_text(text: &str) -> Result<(), crate::WinError> {
    with_clipboard(|c| c.set_text(text.to_string()))
        .ok_or_else(|| crate::WinError("the clipboard is unavailable".into()))?
        .map_err(|e| crate::WinError(e.to_string()))
}

/// The PRIMARY selection (the text currently selected in any app), `None` when nothing is selected.
pub fn read_primary_text() -> Option<String> {
    use arboard::{GetExtLinux, LinuxClipboardKind};
    with_clipboard(|c| c.get().clipboard(LinuxClipboardKind::Primary).text().ok()).flatten().filter(|t| !t.trim().is_empty())
}
