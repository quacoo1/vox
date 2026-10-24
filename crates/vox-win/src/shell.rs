//! Shell helpers (contracts.md §5.3, ui-settings-widget.md §9).
//!
//! COM: `ShellExecuteW` (which may delegate to COM shell extensions) runs inside a single-threaded apartment for the
//! call, the save dialog on its own STA thread, and the toast notifier (WinRT) in the process-wide MTA that
//! `ensure_mta` keeps alive — so callers on plain threads (the core, its workers) need no setup; a thread that already
//! lives in an apartment keeps it.

use std::path::{Path, PathBuf};

use windows::core::{HSTRING, PCWSTR};
use windows::Data::Xml::Dom::XmlDocument;
use windows::Win32::Foundation::{ERROR_CANCELLED, HWND};
use windows::Win32::System::Com::{
    CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwareness, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    PROCESS_PER_MONITOR_DPI_AWARE,
};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FOLDERID_Documents, FileSaveDialog, IFileSaveDialog, IShellItem, SHCreateItemFromParsingName, SHGetKnownFolderPath,
    SetCurrentProcessExplicitAppUserModelID, ShellExecuteW, FOS_FORCEFILESYSTEM, FOS_OVERWRITEPROMPT,
    FOS_PATHMUSTEXIST, KF_FLAG_DEFAULT, SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND, SW_SHOWNORMAL,
};
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};

use crate::window::{ensure_mta, hwnd_from, os_string_from_wide, ComGuard};

/// Opens an http(s) URL in the default browser (`ShellExecuteW open`). Callers validate with
/// `vox_core::read::is_http_url` first.
pub fn open_external(url: &str) -> Result<(), crate::WinError> {
    shell_open(&HSTRING::from(url))
}

/// Opens a folder in Explorer.
pub fn reveal_path(path: &Path) -> Result<(), crate::WinError> {
    shell_open(&HSTRING::from(path))
}

fn shell_open(target: &HSTRING) -> Result<(), crate::WinError> {
    let _com = ComGuard::new(COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
    // SAFETY: the strings outlive the call; no owner window.
    let result =
        unsafe { ShellExecuteW(None, &HSTRING::from("open"), target, PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL) };
    let code = result.0 as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(crate::WinError(shell_execute_error(code)))
    }
}

/// The message for a `ShellExecuteW` return value ≤ 32.
fn shell_execute_error(code: isize) -> String {
    let text = match code {
        0 | 8 => "The system is out of memory or resources",
        2 => "The file was not found",
        3 => "The path was not found",
        5 => "Access is denied",
        11 => "The file is not a valid application",
        26 => "A sharing violation occurred",
        27 => "The file association is incomplete or invalid",
        28..=30 => "The DDE transaction failed",
        31 => "No application is associated with this link",
        32 => "The required library was not found",
        _ => return format!("Could not open it (error {code})"),
    };
    text.to_string()
}

/// A silent OS notification (title `Vox`, body): `ToastNotificationManager::CreateToastNotifierWithId(APP_ID)` with
/// a `ToastGeneric` template (two text lines) and `<audio silent="true"/>`. Works for the portable, unpackaged exe
/// only because [`register_app_user_model`] registered the AUMID at startup (Windows silently drops toasts for an
/// unregistered AUMID). Failures are returned; the caller (`core::toasts`) logs the first failure of the session at
/// warn level (scope `notify`) and ignores later ones. Not shown for `success` toasts.
pub fn show_notification(title: &str, body: &str) -> Result<(), crate::WinError> {
    ensure_mta();
    let doc = XmlDocument::new()?;
    doc.LoadXml(&HSTRING::from(toast_xml(title, body)))?;
    let toast = ToastNotification::CreateToastNotification(&doc)?;
    let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(vox_core::constants::APP_ID))?;
    notifier.Show(&toast)?;
    Ok(())
}

/// The toast document: a `ToastGeneric` binding with the title and the body, and silent audio.
fn toast_xml(title: &str, body: &str) -> String {
    format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual>\
         <audio silent=\"true\"/></toast>",
        xml_escape(title),
        xml_escape(body)
    )
}

/// XML text escaping; characters XML 1.0 cannot carry (C0 controls other than tab/CR/LF, U+FFFE/U+FFFF) are dropped
/// so `LoadXml` never rejects a message.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if (c as u32) < 0x20 || c == '\u{FFFE}' || c == '\u{FFFF}' => {}
            c => out.push(c),
        }
    }
    out
}

/// Registers the App User Model ID for toast notifications of an unpackaged app (R-Installer: no Start-menu shortcut
/// carries the AUMID): writes `HKCU\Software\Classes\AppUserModelId\<id>` with `DisplayName = display_name` and
/// `IconUri = icon_png` (a PNG the caller wrote once to `<profile>\vox-icon.png` from the embedded app icon).
/// Idempotent (rewrites only differing values). Called by `app::run` step 1 (not in shots mode) before
/// [`set_app_user_model_id`]; both profiles use the same `APP_ID`. Desktop checklist: widget off, main window
/// hidden, a read without a key → a silent Windows notification appears in Action Center.
pub fn register_app_user_model(id: &str, display_name: &str, icon_png: &Path) -> Result<(), crate::WinError> {
    if id.trim().is_empty() || id.contains('\\') {
        return Err(crate::WinError(format!("invalid AppUserModelID: {id:?}")));
    }
    let path = format!(r"Software\Classes\AppUserModelId\{id}");
    let (key, _) = winreg::HKCU.create_subkey(&path)?;
    set_string_if_different(&key, "DisplayName", display_name)?;
    set_string_if_different(&key, "IconUri", &icon_png.to_string_lossy())?;
    Ok(())
}

fn set_string_if_different(key: &winreg::RegKey, name: &str, value: &str) -> Result<(), crate::WinError> {
    let current: Option<String> = key.get_value(name).ok();
    if current.as_deref() != Some(value) {
        key.set_value(name, &value.to_string())?;
    }
    Ok(())
}

/// `IFileSaveDialog` on a dedicated STA thread (blocking the caller): title, default path, one filter `(name, ext)`.
/// `owner` = the main window's HWND (`Command::ExportHistory{owner}`), so the dialog is modal to it. `None` =
/// cancelled.
pub fn save_file_dialog(
    owner: Option<isize>,
    title: &str,
    default_path: &Path,
    filter_name: &str,
    extension: &str,
) -> Option<PathBuf> {
    let title = title.to_string();
    let default_path = default_path.to_path_buf();
    let filter_name = filter_name.to_string();
    let extension = extension.trim_start_matches('.').to_string();
    let worker = std::thread::Builder::new().name("vox-save-dialog".into()).spawn(move || {
        let _com = ComGuard::new(COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE);
        match run_save_dialog(owner, &title, &default_path, &filter_name, &extension) {
            Ok(path) => path,
            Err(e) => {
                tracing::warn!(target: "shell", "save dialog failed: {}", e.message());
                None
            }
        }
    });
    match worker {
        Ok(handle) => handle.join().unwrap_or_else(|_| {
            tracing::warn!(target: "shell", "save dialog thread panicked");
            None
        }),
        Err(e) => {
            tracing::warn!(target: "shell", "cannot start the save dialog thread: {e}");
            None
        }
    }
}

fn run_save_dialog(
    owner: Option<isize>,
    title: &str,
    default_path: &Path,
    filter_name: &str,
    extension: &str,
) -> windows::core::Result<Option<PathBuf>> {
    // SAFETY: COM is initialised (STA) on this thread for the whole function; every string outlives its call.
    unsafe {
        let dialog: IFileSaveDialog = CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER)?;
        dialog.SetTitle(&HSTRING::from(title))?;
        let name = HSTRING::from(filter_name);
        let spec = HSTRING::from(format!("*.{extension}"));
        dialog.SetFileTypes(&[COMDLG_FILTERSPEC { pszName: PCWSTR(name.as_ptr()), pszSpec: PCWSTR(spec.as_ptr()) }])?;
        dialog.SetFileTypeIndex(1)?;
        if !extension.is_empty() {
            dialog.SetDefaultExtension(&HSTRING::from(extension))?;
        }
        let options = dialog.GetOptions()?;
        dialog.SetOptions(options | FOS_OVERWRITEPROMPT | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST)?;
        if let Some(dir) = default_path.parent().filter(|d| !d.as_os_str().is_empty()) {
            match SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(dir), None) {
                Ok(folder) => {
                    let _ = dialog.SetFolder(&folder);
                }
                Err(e) => tracing::debug!(target: "shell", "save dialog folder unavailable: {}", e.message()),
            }
        }
        if let Some(file) = default_path.file_name() {
            dialog.SetFileName(&HSTRING::from(file))?;
        }
        let owner_hwnd: Option<HWND> = owner.filter(|h| *h != 0).map(hwnd_from);
        if let Err(e) = dialog.Show(owner_hwnd) {
            if e.code() == ERROR_CANCELLED.to_hresult() {
                return Ok(None);
            }
            return Err(e);
        }
        let item = dialog.GetResult()?;
        let raw = item.GetDisplayName(SIGDN_FILESYSPATH)?;
        let path = os_string_from_wide(raw.as_wide());
        CoTaskMemFree(Some(raw.0 as *const _));
        Ok(if path.is_empty() { None } else { Some(PathBuf::from(path)) })
    }
}

/// `SetCurrentProcessExplicitAppUserModelID`.
pub fn set_app_user_model_id(id: &str) {
    // SAFETY: the string outlives the call.
    if let Err(e) = unsafe { SetCurrentProcessExplicitAppUserModelID(&HSTRING::from(id)) } {
        tracing::warn!(target: "shell", "SetCurrentProcessExplicitAppUserModelID failed: {}", e.message());
    }
}

/// `FOLDERID_Documents`.
pub fn documents_folder() -> Option<PathBuf> {
    // SAFETY: the returned buffer is read once and freed with CoTaskMemFree.
    unsafe {
        let raw = SHGetKnownFolderPath(&FOLDERID_Documents, KF_FLAG_DEFAULT, None).ok()?;
        let path = os_string_from_wide(raw.as_wide());
        CoTaskMemFree(Some(raw.0 as *const _));
        if path.is_empty() {
            None
        } else {
            Some(PathBuf::from(path))
        }
    }
}

/// Declares Per-Monitor-V2 DPI awareness for the process (before any window exists). A manifest that already
/// declared it makes the call fail with "access denied", which is fine.
pub fn set_per_monitor_dpi_aware() {
    // SAFETY: process-wide flag set before any window exists.
    unsafe {
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2).is_err() {
            // Older Windows 10 builds: per-monitor (v1) is the best available.
            let _ = SetProcessDpiAwareness(PROCESS_PER_MONITOR_DPI_AWARE);
        }
    }
}

/// A blocking native message box (`MessageBoxW`, `MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND`, no owner). Used
/// before any UI exists (the old-version guard).
pub fn message_box(title: &str, body: &str) {
    // SAFETY: the strings outlive the (blocking) call.
    unsafe {
        MessageBoxW(None, &HSTRING::from(body), &HSTRING::from(title), MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toast_text_is_escaped_and_silent() {
        let xml = toast_xml("Vox", "Couldn’t open <a & b>.txt \"x\" 'y'");
        assert_eq!(
            xml,
            "<toast><visual><binding template=\"ToastGeneric\"><text>Vox</text><text>Couldn’t open &lt;a &amp; b&gt;.txt \
             &quot;x&quot; &apos;y&apos;</text></binding></visual><audio silent=\"true\"/></toast>"
        );
    }

    #[test]
    fn xml_escape_drops_characters_xml_cannot_hold() {
        assert_eq!(xml_escape("a\u{0}b\u{1B}c\td\ne\u{FFFF}"), "abc\td\ne");
        assert_eq!(xml_escape("😀 é"), "😀 é");
    }

    #[test]
    fn shell_execute_errors_have_messages() {
        assert_eq!(shell_execute_error(2), "The file was not found");
        assert_eq!(shell_execute_error(31), "No application is associated with this link");
        assert_eq!(shell_execute_error(17), "Could not open it (error 17)");
    }

    #[test]
    fn aumid_ids_are_validated() {
        assert!(register_app_user_model("", "Vox", Path::new("x.png")).is_err());
        assert!(register_app_user_model("a\\b", "Vox", Path::new("x.png")).is_err());
    }

    /// `cargo test -p vox-win documents_folder_exists -- --ignored`.
    #[test]
    #[ignore = "reads the user's known folders"]
    fn documents_folder_exists() {
        let docs = documents_folder().expect("FOLDERID_Documents");
        assert!(docs.is_dir(), "{docs:?}");
    }

    /// Desktop checklist (D-SH-19): registers the AUMID exactly as vox-app does (icon `%APPDATA%\Vox\vox-icon.png`, so
    /// the real registration is left as it was), then shows one silent notification.
    /// `cargo test -p vox-win silent_notification_appears -- --ignored`, then look at Action Center.
    #[test]
    #[ignore = "writes HKCU\\Software\\Classes\\AppUserModelId and shows a Windows notification"]
    fn silent_notification_appears() {
        let appdata = std::env::var_os("APPDATA").expect("APPDATA");
        let icon = PathBuf::from(appdata).join("Vox").join("vox-icon.png");
        register_app_user_model(vox_core::constants::APP_ID, vox_core::constants::APP_NAME, &icon).expect("register");
        // Idempotent.
        register_app_user_model(vox_core::constants::APP_ID, vox_core::constants::APP_NAME, &icon).expect("again");
        show_notification("Vox", "Test notification from cargo test").expect("show");
    }
}
