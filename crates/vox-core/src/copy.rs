//! Exact user-facing copy for core-originated toasts and notices (contracts.md §5.6, selection.md §2.4,
//! shell.md §5.4, reader-playback.md §2) plus the copy the Product rulings added (old-version guard, key migration,
//! UIPI refusal, player failure). Every "Vox" comes from [`APP_NAME`].
//!
//! Decision (BRIEF.md Product rulings "Apostrophes are curly everywhere"; ARCHITECTURE.md §8 D-COPY-1): every
//! apostrophe in user-facing copy is U+2019 (`’`), core copy included. The strings that are compared by identity
//! (`eleven_messages::BAD_KEY`) are compared against the same constants, so nothing breaks. Log messages (never shown)
//! keep ASCII.

use crate::constants::APP_NAME;
use crate::format::format_number_en;

pub const ADD_KEY_TO_READ: &str = "Add your ElevenLabs API key to start reading";
pub const ADD_KEY_ACTION: &str = "Add key";
pub const OPEN_SETTINGS_ACTION: &str = "Open settings";
pub const NOTHING_TO_READ: &str = "Nothing to read in that selection";
pub const COPY_SOME_TEXT: &str = "Copy some text first";
pub const SELECT_SOME_TEXT: &str = "Select some text first";
pub const CLIPBOARD_TOO_LARGE: &str = "Couldn’t restore your previous clipboard (it was too large)";
pub const CLIPBOARD_UNREADABLE: &str = "Couldn’t restore your previous clipboard";
pub const EXPLORER_MENU_FAILED: &str = "Couldn’t update the Explorer menu";

/// Selection notice when the foreground app runs elevated and Vox does not (UIPI blocks the copy chord).
pub fn elevated_app_refused() -> String {
    format!("{APP_NAME} can\u{2019}t read from apps running as administrator")
}

/// Native message box when an Electron build of Vox is running (BRIEF.md "Do not run both builds at once").
pub fn old_version_running_title() -> String {
    APP_NAME.to_string()
}
pub fn old_version_running_body() -> String {
    format!("Close the old version of {APP_NAME} first")
}

/// Shown on Settings → Account and the onboarding key step when the Electron key could not be migrated
/// (persistence.md §4; BRIEF.md "API key").
pub fn key_migration_failed() -> String {
    format!("{APP_NAME} couldn\u{2019}t bring over the API key saved by the previous version. Paste it again to keep reading.")
}

/// Same place, when `secrets-dpapi.bin` exists but cannot be decrypted (profile copied from another account/PC).
pub fn key_unreadable() -> String {
    format!("{APP_NAME} couldn\u{2019}t read your saved API key on this account. Paste it again to keep reading.")
}

/// Settings → Triggers notice while the global input hook could not be installed (ui-settings-widget.md §2.5).
pub const HOOK_UNAVAILABLE_TITLE: &str = "Mouse and keyboard hook unavailable";
pub const HOOK_UNAVAILABLE_BODY: &str =
    "The right-click chip and the selection popup need it, so they\u{2019}re paused. Shortcuts, the browser extension and the tray menu still work.";

/// Error-card message after the audio player failed and was restarted (BRIEF.md "Audio").
pub const PLAYBACK_FAILED: &str = "Audio playback stopped unexpectedly. Check your speakers or headphones and try again.";
/// Error-card message when no audio output device can be opened.
pub const NO_AUDIO_OUTPUT: &str = "Couldn\u{2019}t open your speakers or headphones";

/// The generic failure text (also what `format::clean_error` returns for a blank message). Used for the answers to
/// request commands whose handler panicked (ARCHITECTURE.md §8 D-ARCH-3).
pub const SOMETHING_WENT_WRONG: &str = "Something went wrong";

/// Settings → Appearance intro and per-theme tile notes (BRIEF.md Product rulings "Copy": the old "Cards stay black…"
/// / "Black cards on …" copy described the old look; ARCHITECTURE.md §8 D-USW-20: the floating player and the chip are
/// always dark, the theme changes the main window only). Notes are in `ThemeMode::ALL` order (System, Paper, Night).
pub const APPEARANCE_INTRO: &str = "The theme changes this window. The floating player and the chip always stay dark.";
pub const THEME_NOTES: [&str; 3] = ["Follows Windows dark mode", "Light window", "Dark window"];

/// `Couldn't open the link: <cleaned error>` (History page links, Settings → Account / onboarding "Get a key").
pub fn open_link_failed(err: &str) -> String {
    format!("Couldn\u{2019}t open the link: {}", crate::format::clean_error(err))
}

/// `Reading the first 20,000 characters`.
pub fn reading_first(max_chars: u32) -> String {
    format!("Reading the first {} characters", format_number_en(max_chars as f64))
}

/// `Couldn't open <file name>`.
pub fn couldnt_open(file_name: &str) -> String {
    format!("Couldn’t open {file_name}")
}

/// `Nothing is playing. Select some text and press <hotkey>` / `… to read it`.
pub fn nothing_is_playing(read_selection_hotkey: Option<&str>) -> String {
    match read_selection_hotkey.map(str::trim).filter(|h| !h.is_empty()) {
        Some(h) => format!("Nothing is playing. Select some text and press {h}"),
        None => "Nothing is playing. Select some text to read it".into(),
    }
}

/// `That looks like a password, so Vox won't read it`.
pub fn secret_refused() -> String {
    format!("That looks like a password, so {APP_NAME} won’t read it")
}

/// `That text is marked private, so Vox won't read it`.
pub fn private_refused() -> String {
    format!("That text is marked private, so {APP_NAME} won’t read it")
}

/// `Vox doesn't read from password managers`.
pub fn password_manager_refused() -> String {
    format!("{APP_NAME} doesn’t read from password managers")
}

/// `Couldn't update the Explorer menu[: <err>]`.
pub fn explorer_menu_failed(err: &str) -> String {
    if err.trim().is_empty() {
        EXPLORER_MENU_FAILED.into()
    } else {
        format!("{EXPLORER_MENU_FAILED}: {err}")
    }
}

/// `Browser bridge couldn't use port <N>. Pick another port in Settings`.
pub fn bridge_port_failed(port: u16) -> String {
    format!("Browser bridge couldn’t use port {port}. Pick another port in Settings")
}

/// Bridge listen errors (integrations.md §1.2).
pub fn port_in_use(port: u16) -> String {
    format!("Port {port} is already in use by another program")
}
pub fn port_blocked(port: u16) -> String {
    format!("Port {port} is reserved or blocked by Windows \u{2014} choose another port")
}
pub const LOOPBACK_UNAVAILABLE: &str = "The loopback address 127.0.0.1 is not available";
pub fn invalid_port(port: impl std::fmt::Display) -> String {
    format!("Invalid port {port} (use 1024\u{2013}65535)")
}

/// Settings → Integrations note under the port field.
pub fn port_may_be_in_use(port: u16) -> String {
    format!("Port {port} may be in use by another app. Try another one.")
}

/// `Reading with George from now on`.
pub fn reading_with(name: &str) -> String {
    format!("Reading with {name} from now on")
}

/// Voice & model "Test voice" sample.
pub fn test_voice_sample(voice_name: &str) -> String {
    format!(
        "Hi, I\u{2019}m {voice_name}. This is how {APP_NAME} sounds when it reads to you \u{2014} select any text and I\u{2019}ll take it from there."
    )
}

/// Onboarding "Try it" sample.
pub fn onboarding_sample(voice_name: &str) -> String {
    let name = if voice_name.is_empty() { APP_NAME } else { voice_name };
    format!(
        "Hi, I’m {name}. Select any text on your screen, right-click it, and choose Read with {APP_NAME}. I’ll read emails, articles and documents aloud while you keep working."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy() {
        assert_eq!(reading_first(20_000), "Reading the first 20,000 characters");
        assert_eq!(nothing_is_playing(Some(" Ctrl+Alt+R ")), "Nothing is playing. Select some text and press Ctrl+Alt+R");
        assert_eq!(nothing_is_playing(Some("")), "Nothing is playing. Select some text to read it");
        assert_eq!(secret_refused(), "That looks like a password, so Vox won’t read it");
        assert_eq!(explorer_menu_failed(""), "Couldn’t update the Explorer menu");
        assert_eq!(explorer_menu_failed("x"), "Couldn’t update the Explorer menu: x");
        assert_eq!(invalid_port(80), "Invalid port 80 (use 1024\u{2013}65535)");
        assert_eq!(elevated_app_refused(), "Vox can\u{2019}t read from apps running as administrator");
        assert_eq!(old_version_running_body(), "Close the old version of Vox first");
        assert_eq!(couldnt_open("a.txt"), "Couldn\u{2019}t open a.txt");
        assert_eq!(open_link_failed("Error: no handler"), "Couldn\u{2019}t open the link: no handler");
        assert_eq!(open_link_failed(""), "Couldn\u{2019}t open the link: Something went wrong");
        assert_eq!(THEME_NOTES.len(), crate::settings::ThemeMode::ALL.len());
        assert!(!APPEARANCE_INTRO.contains('\''));
    }
}
