//! Foreground and window info (selection.md §7, §11). Every function is synchronous, fast and never panics; `None`
//! means unknown.

#![warn(dead_code, unused)]

use std::sync::OnceLock;

use parking_lot::Mutex;
use vox_core::constants::WINDOW_TITLE_MAX;
use vox_core::js::{collapse_whitespace, is_js_whitespace, js_trim};
use vox_core::read::ForegroundInfo;
use vox_core::text16::prefix16;
use windows::core::{w, HRESULT, PWSTR};
use windows::Win32::Foundation::{CloseHandle, ERROR_ACCESS_DENIED, HANDLE, HWND, LPARAM, POINT, WPARAM};
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel, TOKEN_MANDATORY_LABEL,
    TOKEN_QUERY,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Shell::SHQueryUserNotificationState;
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetClassNameW, GetCursorInfo, GetForegroundWindow, GetGUIThreadInfo, GetWindowLongW, GetWindowTextW,
    GetWindowThreadProcessId, InternalGetWindowText, LoadCursorW, PostMessageW, RegisterWindowMessageW,
    WindowFromPoint, CURSORINFO, CURSOR_SHOWING, CURSOR_SUPPRESSED, GA_ROOT, GA_ROOTOWNER, GUITHREADINFO, GWL_EXSTYLE,
    IDC_IBEAM, WS_EX_NOACTIVATE,
};

/// A described top-level window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowInfo {
    pub hwnd: isize,
    pub info: ForegroundInfo,
    pub class_name: String,
}

impl WindowInfo {
    /// The gesture identity: root HWND as decimal.
    pub fn window_id(&self) -> String {
        self.hwnd.to_string()
    }
}

/// `describe(GetForegroundWindow())` without the window id/class.
pub fn foreground_info() -> Option<ForegroundInfo> {
    foreground_window_info().map(|w| w.info)
}

pub fn foreground_window_info() -> Option<WindowInfo> {
    // SAFETY: no arguments; returns a possibly null handle.
    describe(unsafe { GetForegroundWindow() })
}

/// The foreground window and its process id only (no caption, class or image name): the hook watchdog's cheap probe
/// input. `None` without a foreground window (the secure desktop, a locked session).
pub(crate) fn foreground_hwnd_pid() -> Option<(isize, u32)> {
    // SAFETY: no arguments; returns a possibly null handle.
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_invalid() {
        return None;
    }
    let mut pid = 0u32;
    // SAFETY: `pid` outlives the call; a stale handle yields 0.
    let tid = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    (tid != 0 && pid != 0).then_some((hwnd.0 as isize, pid))
}

/// The root window under a physical point.
pub fn window_info_at(x: i32, y: i32) -> Option<WindowInfo> {
    describe(root_window_at(x, y)?)
}

/// `friendlyAppName(exe)` (table + title-case fallback; pure).
pub fn friendly_app_name(process_name: &str) -> String {
    let name = js_trim(process_name).to_lowercase();
    if name.is_empty() {
        return String::new();
    }
    if let Some(known) = known_app_name(&name) {
        return known.to_string();
    }
    let stem = name.strip_suffix(".exe").unwrap_or(&name);
    stem.split(|c: char| is_js_whitespace(c) || matches!(c, '.' | '_' | '-'))
        .filter(|part| !part.is_empty())
        .map(vox_core::js::capitalize_first)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The password-manager deny list (pure, case-insensitive).
pub fn is_password_manager(process_name: &str) -> bool {
    !process_name.is_empty() && PASSWORD_MANAGERS.contains(&process_name.to_lowercase().as_str())
}

/// `cleanTitle`: whitespace collapsed, trimmed, ≤ 300 units (pure).
pub fn clean_title(title: &str) -> String {
    let collapsed = collapse_whitespace(title);
    // `prefix16` cuts at 300 UTF-16 units and drops a dangling high surrogate.
    prefix16(&collapsed, WINDOW_TITLE_MAX).to_string()
}

/// Our own non-activating window (widget/chip) is under the point.
pub fn is_own_overlay_at(x: i32, y: i32) -> Option<bool> {
    let Some(hwnd) = root_window_at(x, y) else { return Some(false) };
    let mut pid = 0u32;
    // SAFETY: `hwnd` came from WindowFromPoint/GetAncestor; both calls tolerate a window that has gone meanwhile.
    let (tid, ex_style) =
        unsafe { (GetWindowThreadProcessId(hwnd, Some(&mut pid)), GetWindowLongW(hwnd, GWL_EXSTYLE)) };
    if tid == 0 {
        return Some(false);
    }
    Some(pid == own_pid() && (ex_style as u32 & WS_EX_NOACTIVATE.0) != 0)
}

/// Text-cursor evidence (I-beam, caret, or a document app in front → benefit of the doubt).
pub fn text_cursor_shown() -> Option<bool> {
    let shape = cursor_shape();
    if shape == CursorShape::IBeam || foreground_has_caret() {
        return Some(true);
    }
    if shape == CursorShape::Suppressed || document_app_in_front() {
        return None;
    }
    Some(false)
}

/// D3D fullscreen or presentation mode (`SHQueryUserNotificationState` 3 or 4).
pub fn fullscreen_app_running() -> bool {
    // SAFETY: no pointer arguments in the windows-rs wrapper.
    match unsafe { SHQueryUserNotificationState() } {
        Ok(state) => is_fullscreen_state(state.0),
        Err(_) => false,
    }
}

/// UIPI check (BRIEF.md Product rulings "Clipboard": the target app runs elevated and Vox does not, so injected
/// input would be dropped). `hwnd`/`pid` come from [`foreground_window_info`]. Algorithm (ARCHITECTURE.md §8 D-SEL-2):
/// 1. Our own token is High or System integrity → `Some(false)` (nothing is above us).
/// 2. libuiohook's direct probe: `PostMessageW(hwnd, RegisterWindowMessageW("VOX_UIPI_PROBE"), 0, 0)`. Fails with
///    `ERROR_ACCESS_DENIED` → `Some(true)` (UIPI blocks us); succeeds → `Some(false)` (the message is ignored by the
///    target).
/// 3. Only when the post fails for another reason: compare `TokenIntegrityLevel`s (`OpenProcess(PROCESS_QUERY_LIMITED_
///    INFORMATION)` + `OpenProcessToken(TOKEN_QUERY)`); an `ERROR_ACCESS_DENIED` from `OpenProcessToken` counts as
///    `Some(true)` (an elevated token's DACL denies us). An `OpenProcess` failure is **never** treated as elevated
///    (protected processes and another user's same-integrity windows are not UIPI-blocked) → `None`.
///
/// Note: `OpenProcess(PQLI)` succeeds on an elevated process of the same user (the right is exempt from no-read-up),
/// which is why the token check alone misses the case the ruling is about. Fast; safe on any thread. `#[ignore]`
/// desktop tests: an elevated Notepad → `Some(true)`; a "Run as different user" Notepad → `Some(false)`.
pub fn input_blocked_by_uipi(hwnd: isize, pid: u32) -> Option<bool> {
    if hwnd == 0 {
        // A null HWND would post to our own thread's queue: nothing to probe.
        return None;
    }
    uipi_verdict(own_integrity(), || post_probe(hwnd), || token_probe(pid))
}

// ───────────────────────────── pure data and rules ─────────────────────────────

/// selection.md §11.2.
const APP_NAMES: [(&str, &str); 38] = [
    ("chrome.exe", "Chrome"),
    ("msedge.exe", "Edge"),
    ("winword.exe", "Word"),
    ("acrord32.exe", "Acrobat"),
    ("acrobat.exe", "Acrobat"),
    ("notepad.exe", "Notepad"),
    ("code.exe", "VS Code"),
    ("firefox.exe", "Firefox"),
    ("outlook.exe", "Outlook"),
    ("olk.exe", "Outlook"),
    ("onenote.exe", "OneNote"),
    ("slack.exe", "Slack"),
    ("discord.exe", "Discord"),
    ("obsidian.exe", "Obsidian"),
    ("notion.exe", "Notion"),
    ("teams.exe", "Teams"),
    ("ms-teams.exe", "Teams"),
    ("explorer.exe", "Explorer"),
    ("brave.exe", "Brave"),
    ("opera.exe", "Opera"),
    ("vivaldi.exe", "Vivaldi"),
    ("sumatrapdf.exe", "SumatraPDF"),
    ("kindle.exe", "Kindle"),
    ("excel.exe", "Excel"),
    ("powerpnt.exe", "PowerPoint"),
    ("windowsterminal.exe", "Terminal"),
    ("wt.exe", "Terminal"),
    ("cmd.exe", "Command Prompt"),
    ("powershell.exe", "PowerShell"),
    ("pwsh.exe", "PowerShell"),
    ("notepad++.exe", "Notepad++"),
    ("sublime_text.exe", "Sublime Text"),
    ("cursor.exe", "Cursor"),
    ("thunderbird.exe", "Thunderbird"),
    ("whatsapp.exe", "WhatsApp"),
    ("telegram.exe", "Telegram"),
    ("foxitpdfreader.exe", "Foxit Reader"),
    ("msedgewebview2.exe", "Edge WebView"),
];

fn known_app_name(lower: &str) -> Option<&'static str> {
    APP_NAMES.iter().find(|(exe, _)| *exe == lower).map(|(_, name)| *name)
}

/// selection.md §7.3.
const PASSWORD_MANAGERS: [&str; 17] = [
    "keepass.exe",
    "keepassxc.exe",
    "keeweb.exe",
    "1password.exe",
    "bitwarden.exe",
    "enpass.exe",
    "roboform.exe",
    "robotaskbaricon.exe",
    "dashlane.exe",
    "keeperpasswordmanager.exe",
    "nordpass.exe",
    "proton pass.exe",
    "lastpass.exe",
    "passwordsafe.exe",
    "pwsafe.exe",
    "sticky password.exe",
    "stpass.exe",
];

/// Document readers that may draw their own text cursor (selection.md §11.5).
const DOCUMENT_APPS: [&str; 15] = [
    "winword.exe",
    "acrord32.exe",
    "acrobat.exe",
    "foxitpdfreader.exe",
    "foxitpdfeditor.exe",
    "foxitreader.exe",
    "pdfxedit.exe",
    "sumatrapdf.exe",
    "kindle.exe",
    "ebook-viewer.exe",
    "onenote.exe",
    "outlook.exe",
    "olk.exe",
    "soffice.bin",
    "wps.exe",
];

fn is_document_app(process_name: &str) -> bool {
    DOCUMENT_APPS.contains(&process_name)
}

/// `basename`: the text after the last `\` or `/`.
fn basename(path: &str) -> &str {
    path.rsplit(['\\', '/']).next().unwrap_or(path)
}

/// The app name for a window: the friendly name, except that UWP apps hosted by `ApplicationFrameHost` take the text
/// after the last ` - ` of their caption ("Inbox - Mail" → "Mail") when it is not blank.
fn app_name_for(process_name: &str, title: &str) -> String {
    let friendly = friendly_app_name(process_name);
    if process_name == "applicationframehost.exe" && !title.is_empty() {
        let last = js_trim(title.rsplit(" - ").next().unwrap_or(title));
        if !last.is_empty() {
            return last.to_string();
        }
    }
    friendly
}

/// `QUNS_RUNNING_D3D_FULL_SCREEN (3)` or `QUNS_PRESENTATION_MODE (4)`; deliberately not `QUNS_BUSY (2)` (F11
/// browsers and fullscreen readers are reading surfaces).
fn is_fullscreen_state(state: i32) -> bool {
    state == 3 || state == 4
}

/// `SECURITY_MANDATORY_HIGH_RID`.
const HIGH_INTEGRITY_RID: u32 = 0x3000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PostProbe {
    Delivered,
    AccessDenied,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenProbe {
    Integrity(u32),
    TokenAccessDenied,
    ProcessUnavailable,
    Failed,
}

/// D-SEL-2 decision over the probe results (pure; the probes run lazily in this order).
fn uipi_verdict(
    own: Option<u32>,
    post: impl FnOnce() -> PostProbe,
    token: impl FnOnce() -> TokenProbe,
) -> Option<bool> {
    if own.is_some_and(|rid| rid >= HIGH_INTEGRITY_RID) {
        return Some(false);
    }
    match post() {
        PostProbe::Delivered => Some(false),
        PostProbe::AccessDenied => Some(true),
        PostProbe::Failed => match token() {
            TokenProbe::Integrity(target) => own.map(|o| target > o),
            TokenProbe::TokenAccessDenied => Some(true),
            TokenProbe::ProcessUnavailable | TokenProbe::Failed => None,
        },
    }
}

// ───────────────────────────── Win32 plumbing ─────────────────────────────

fn own_pid() -> u32 {
    static PID: OnceLock<u32> = OnceLock::new();
    // SAFETY: no arguments.
    *PID.get_or_init(|| unsafe { GetCurrentProcessId() })
}

fn hwnd_of(raw: isize) -> HWND {
    HWND(raw as *mut core::ffi::c_void)
}

/// `GetAncestor(WindowFromPoint(pt), GA_ROOT) ?? hwnd`, `None` when nothing is there.
fn root_window_at(x: i32, y: i32) -> Option<HWND> {
    // SAFETY: plain value arguments; the handles may be stale, which every later call tolerates.
    let hit = unsafe { WindowFromPoint(POINT { x, y }) };
    if hit.is_invalid() {
        return None;
    }
    // SAFETY: as above.
    let root = unsafe { GetAncestor(hit, GA_ROOT) };
    Some(if root.is_invalid() { hit } else { root })
}

fn describe(hwnd: HWND) -> Option<WindowInfo> {
    if hwnd.is_invalid() {
        return None;
    }
    let mut pid = 0u32;
    // SAFETY: `pid` outlives the call; a stale handle yields 0.
    let tid = unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if tid == 0 || pid == 0 {
        return None;
    }
    let process_name = process_image_path(pid).map(|p| basename(&p).to_lowercase()).unwrap_or_default();
    let mut title = clean_title(&window_text(hwnd, pid));
    if title.is_empty() {
        // Owned popups (a browser's context menu, a tool window) are captionless: use the owner's caption.
        // SAFETY: stale handles are tolerated.
        let owner = unsafe { GetAncestor(hwnd, GA_ROOTOWNER) };
        if !owner.is_invalid() && owner != hwnd {
            let mut owner_pid = 0u32;
            // SAFETY: as above.
            unsafe { GetWindowThreadProcessId(owner, Some(&mut owner_pid)) };
            title = clean_title(&window_text(owner, owner_pid));
        }
    }
    let app_name = app_name_for(&process_name, &title);
    Some(WindowInfo {
        hwnd: hwnd.0 as isize,
        info: ForegroundInfo { process_name, app_name, title, pid },
        class_name: class_name(hwnd),
    })
}

/// The caption (≤ 511 units). Our own windows are read with `InternalGetWindowText`, which never sends
/// `WM_GETTEXT` (that would be a cross-thread `SendMessage` to the UI thread); other processes' captions are read
/// without a message anyway.
fn window_text(hwnd: HWND, pid: u32) -> String {
    let mut buf = [0u16; 512];
    // SAFETY: the buffer is valid for its length; a stale handle returns 0.
    let n = unsafe {
        if pid == own_pid() {
            InternalGetWindowText(hwnd, &mut buf)
        } else {
            GetWindowTextW(hwnd, &mut buf)
        }
    };
    let n = usize::try_from(n).unwrap_or(0).min(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    // SAFETY: the buffer is valid for its length.
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    let n = usize::try_from(n).unwrap_or(0).min(buf.len());
    String::from_utf16_lossy(&buf[..n])
}

/// Closes a handle when dropped.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: we own the handle and close it once.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

fn open_process(pid: u32) -> Option<OwnedHandle> {
    // SAFETY: plain value arguments.
    unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok().map(OwnedHandle)
}

/// Full Win32 image path (`None` when the process can't be queried or the path needs 1024+ units).
fn process_image_path(pid: u32) -> Option<String> {
    let process = open_process(pid)?;
    let mut buf = [0u16; 1024];
    let mut size = buf.len() as u32;
    // SAFETY: `buf` and `size` outlive the call; `size` holds the buffer length in units.
    unsafe { QueryFullProcessImageNameW(process.0, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut size) }.ok()?;
    let n = (size as usize).min(buf.len());
    Some(String::from_utf16_lossy(&buf[..n]))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CursorShape {
    IBeam,
    Suppressed,
    Hidden,
    Other,
}

fn ibeam_cursor() -> isize {
    static IBEAM: OnceLock<isize> = OnceLock::new();
    // SAFETY: loading a shared system cursor; the handle is never freed.
    *IBEAM.get_or_init(|| unsafe { LoadCursorW(None, IDC_IBEAM) }.map(|h| h.0 as isize).unwrap_or(0))
}

fn cursor_shape() -> CursorShape {
    let mut ci = CURSORINFO { cbSize: std::mem::size_of::<CURSORINFO>() as u32, ..Default::default() };
    // SAFETY: `ci` is a properly sized CURSORINFO.
    if unsafe { GetCursorInfo(&mut ci) }.is_err() {
        return CursorShape::Other;
    }
    if ci.flags.0 & CURSOR_SUPPRESSED.0 != 0 {
        return CursorShape::Suppressed;
    }
    if ci.flags.0 & CURSOR_SHOWING.0 == 0 {
        return CursorShape::Hidden;
    }
    let ibeam = ibeam_cursor();
    if ibeam != 0 && ci.hCursor.0 as isize == ibeam {
        CursorShape::IBeam
    } else {
        CursorShape::Other
    }
}

fn foreground_has_caret() -> bool {
    let mut gti = GUITHREADINFO { cbSize: std::mem::size_of::<GUITHREADINFO>() as u32, ..Default::default() };
    // SAFETY: `gti` is a properly sized GUITHREADINFO; thread 0 = the foreground thread.
    unsafe { GetGUIThreadInfo(0, &mut gti) }.is_ok() && !gti.hwndCaret.is_invalid()
}

/// The foreground window belongs to a document reader; cached per `(hwnd, pid)` so the process query runs only when
/// the foreground window changes.
fn document_app_in_front() -> bool {
    static CACHE: Mutex<(isize, u32, bool)> = Mutex::new((0, 0, false));
    // SAFETY: no arguments.
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_invalid() {
        return false;
    }
    let mut pid = 0u32;
    // SAFETY: `pid` outlives the call.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    let mut cache = CACHE.lock();
    if cache.0 != hwnd.0 as isize || cache.1 != pid {
        let exe = process_image_path(pid).map(|p| basename(&p).to_lowercase()).unwrap_or_default();
        *cache = (hwnd.0 as isize, pid, is_document_app(&exe));
    }
    cache.2
}

fn post_probe(hwnd: isize) -> PostProbe {
    static PROBE_MSG: OnceLock<u32> = OnceLock::new();
    // SAFETY: a static wide string literal.
    let msg = *PROBE_MSG.get_or_init(|| unsafe { RegisterWindowMessageW(w!("VOX_UIPI_PROBE")) });
    if msg == 0 {
        return PostProbe::Failed;
    }
    // SAFETY: plain value arguments; the target ignores an unknown registered message.
    match unsafe { PostMessageW(Some(hwnd_of(hwnd)), msg, WPARAM(0), LPARAM(0)) } {
        Ok(()) => PostProbe::Delivered,
        Err(e) if e.code() == HRESULT::from_win32(ERROR_ACCESS_DENIED.0) => PostProbe::AccessDenied,
        Err(_) => PostProbe::Failed,
    }
}

fn token_probe(pid: u32) -> TokenProbe {
    let Some(process) = open_process(pid) else { return TokenProbe::ProcessUnavailable };
    let mut token = HANDLE::default();
    // SAFETY: `token` outlives the call and is closed by `OwnedHandle`.
    match unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &mut token) } {
        Ok(()) => {
            let token = OwnedHandle(token);
            integrity_of(token.0).map_or(TokenProbe::Failed, TokenProbe::Integrity)
        }
        Err(e) if e.code() == HRESULT::from_win32(ERROR_ACCESS_DENIED.0) => TokenProbe::TokenAccessDenied,
        Err(_) => TokenProbe::Failed,
    }
}

/// Our own integrity RID (read once).
fn own_integrity() -> Option<u32> {
    static OWN: OnceLock<Option<u32>> = OnceLock::new();
    *OWN.get_or_init(|| {
        let mut token = HANDLE::default();
        // SAFETY: the pseudo handle needs no closing; `token` is closed by `OwnedHandle`.
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.ok()?;
        let token = OwnedHandle(token);
        integrity_of(token.0)
    })
}

/// The mandatory-label RID of a token (`SECURITY_MANDATORY_*_RID`).
fn integrity_of(token: HANDLE) -> Option<u32> {
    let mut needed = 0u32;
    // SAFETY: a size query with no buffer; it fails with ERROR_INSUFFICIENT_BUFFER and sets `needed`.
    let _ = unsafe { GetTokenInformation(token, TokenIntegrityLevel, None, 0, &mut needed) };
    if needed == 0 || needed > 4096 {
        return None;
    }
    // u64 storage keeps the TOKEN_MANDATORY_LABEL (which starts with a pointer) aligned.
    let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: `buf` holds at least `needed` bytes.
    unsafe {
        GetTokenInformation(token, TokenIntegrityLevel, Some(buf.as_mut_ptr().cast()), needed, &mut needed).ok()?;
        let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
        let sid = label.Label.Sid;
        if sid.is_invalid() {
            return None;
        }
        let count = *GetSidSubAuthorityCount(sid);
        if count == 0 {
            return None;
        }
        Some(*GetSidSubAuthority(sid, u32::from(count) - 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn friendly_names_come_from_the_table() {
        let table = [
            ("chrome.exe", "Chrome"),
            ("msedge.exe", "Edge"),
            ("winword.exe", "Word"),
            ("acrord32.exe", "Acrobat"),
            ("acrobat.exe", "Acrobat"),
            ("notepad.exe", "Notepad"),
            ("code.exe", "VS Code"),
            ("firefox.exe", "Firefox"),
            ("outlook.exe", "Outlook"),
            ("olk.exe", "Outlook"),
            ("onenote.exe", "OneNote"),
            ("slack.exe", "Slack"),
            ("discord.exe", "Discord"),
            ("obsidian.exe", "Obsidian"),
            ("notion.exe", "Notion"),
            ("teams.exe", "Teams"),
            ("ms-teams.exe", "Teams"),
            ("explorer.exe", "Explorer"),
            ("brave.exe", "Brave"),
            ("opera.exe", "Opera"),
            ("vivaldi.exe", "Vivaldi"),
            ("sumatrapdf.exe", "SumatraPDF"),
            ("kindle.exe", "Kindle"),
            ("excel.exe", "Excel"),
            ("powerpnt.exe", "PowerPoint"),
            ("windowsterminal.exe", "Terminal"),
            ("wt.exe", "Terminal"),
            ("cmd.exe", "Command Prompt"),
            ("powershell.exe", "PowerShell"),
            ("pwsh.exe", "PowerShell"),
            ("notepad++.exe", "Notepad++"),
            ("sublime_text.exe", "Sublime Text"),
            ("cursor.exe", "Cursor"),
            ("thunderbird.exe", "Thunderbird"),
            ("whatsapp.exe", "WhatsApp"),
            ("telegram.exe", "Telegram"),
            ("foxitpdfreader.exe", "Foxit Reader"),
            ("msedgewebview2.exe", "Edge WebView"),
        ];
        for (exe, name) in table {
            assert_eq!(friendly_app_name(exe), name, "{exe}");
        }
        // Lower-cased and trimmed first.
        assert_eq!(friendly_app_name("  CHROME.EXE "), "Chrome");
        assert_eq!(friendly_app_name("WinWord.exe"), "Word");
    }

    #[test]
    fn friendly_names_fall_back_to_the_title_cased_stem() {
        assert_eq!(friendly_app_name("my_tool.exe"), "My Tool");
        assert_eq!(friendly_app_name("foo-bar.baz.exe"), "Foo Bar Baz");
        assert_eq!(friendly_app_name("__x__.exe"), "X");
        assert_eq!(friendly_app_name("soffice.bin"), "Soffice Bin");
        assert_eq!(friendly_app_name("app"), "App");
        assert_eq!(friendly_app_name("über tool.EXE"), "Über Tool");
        assert_eq!(friendly_app_name(".exe"), "");
        assert_eq!(friendly_app_name(""), "");
        assert_eq!(friendly_app_name("   "), "");
    }

    #[test]
    fn password_managers_match_case_insensitively() {
        for exe in PASSWORD_MANAGERS {
            assert!(is_password_manager(exe), "{exe}");
            assert!(is_password_manager(&exe.to_uppercase()), "{exe}");
        }
        assert!(is_password_manager("KeePassXC.exe"));
        assert!(is_password_manager("Proton Pass.exe"));
        assert!(!is_password_manager(""));
        assert!(!is_password_manager("keepass"));
        assert!(!is_password_manager("chrome.exe"));
    }

    #[test]
    fn clean_title_collapses_trims_and_cuts_at_300_units() {
        assert_eq!(clean_title("  Inbox \t\n -  Mail  "), "Inbox - Mail");
        assert_eq!(clean_title("a\u{00A0}\u{2028}b\u{FEFF}"), "a b");
        assert_eq!(clean_title(""), "");
        let long = "x".repeat(400);
        assert_eq!(clean_title(&long).len(), 300);
        // 299 units then an emoji (2 units): the dangling high surrogate is dropped.
        let s = format!("{}😀tail", "y".repeat(299));
        let t = clean_title(&s);
        assert_eq!(t, "y".repeat(299));
        // 298 + emoji fits exactly.
        let s = format!("{}😀tail", "y".repeat(298));
        assert_eq!(clean_title(&s), format!("{}😀", "y".repeat(298)));
    }

    #[test]
    fn application_frame_host_takes_the_app_from_the_caption() {
        assert_eq!(app_name_for("applicationframehost.exe", "Inbox - Mail"), "Mail");
        assert_eq!(app_name_for("applicationframehost.exe", "A - B - Calculator"), "Calculator");
        assert_eq!(app_name_for("applicationframehost.exe", "Settings"), "Settings");
        // A blank last part falls back to the friendly name.
        assert_eq!(app_name_for("applicationframehost.exe", "Inbox - "), "Applicationframehost");
        assert_eq!(app_name_for("applicationframehost.exe", ""), "Applicationframehost");
        assert_eq!(app_name_for("chrome.exe", "Inbox - Mail"), "Chrome");
    }

    #[test]
    fn basename_splits_on_either_slash() {
        assert_eq!(basename(r"C:\Program Files\Google\chrome.exe"), "chrome.exe");
        assert_eq!(basename("C:/x/y/Code.exe"), "Code.exe");
        assert_eq!(basename("plain.exe"), "plain.exe");
        assert_eq!(basename(r"C:\dir\"), "");
    }

    #[test]
    fn quns_3_and_4_suppress_chips_but_busy_does_not() {
        assert!(is_fullscreen_state(3));
        assert!(is_fullscreen_state(4));
        assert!(!is_fullscreen_state(2));
        assert!(!is_fullscreen_state(5));
        assert!(!is_fullscreen_state(1));
    }

    #[test]
    fn document_apps_include_soffice_bin() {
        assert!(is_document_app("soffice.bin"));
        assert!(is_document_app("winword.exe"));
        assert!(!is_document_app("notepad.exe"));
    }

    #[test]
    fn uipi_verdict_follows_d_sel_2() {
        let never_post = || -> PostProbe { panic!("the post probe must not run") };
        let never_token = || -> TokenProbe { panic!("the token probe must not run") };
        // Elevated or System Vox: nothing is above us.
        assert_eq!(uipi_verdict(Some(0x3000), never_post, never_token), Some(false));
        assert_eq!(uipi_verdict(Some(0x4000), never_post, never_token), Some(false));
        // Medium Vox: the direct probe decides.
        assert_eq!(uipi_verdict(Some(0x2000), || PostProbe::AccessDenied, never_token), Some(true));
        assert_eq!(uipi_verdict(Some(0x2000), || PostProbe::Delivered, never_token), Some(false));
        // The post failed otherwise: compare integrity levels.
        assert_eq!(uipi_verdict(Some(0x2000), || PostProbe::Failed, || TokenProbe::Integrity(0x3000)), Some(true));
        assert_eq!(uipi_verdict(Some(0x2000), || PostProbe::Failed, || TokenProbe::Integrity(0x2000)), Some(false));
        assert_eq!(uipi_verdict(Some(0x2000), || PostProbe::Failed, || TokenProbe::Integrity(0x1000)), Some(false));
        assert_eq!(uipi_verdict(Some(0x2000), || PostProbe::Failed, || TokenProbe::TokenAccessDenied), Some(true));
        // OpenProcess failing is never "elevated".
        assert_eq!(uipi_verdict(Some(0x2000), || PostProbe::Failed, || TokenProbe::ProcessUnavailable), None);
        assert_eq!(uipi_verdict(Some(0x2000), || PostProbe::Failed, || TokenProbe::Failed), None);
        // Own level unknown: the probe still decides; a level comparison can't.
        assert_eq!(uipi_verdict(None, || PostProbe::AccessDenied, never_token), Some(true));
        assert_eq!(uipi_verdict(None, || PostProbe::Failed, || TokenProbe::Integrity(0x3000)), None);
        // The token probe runs only after a failed post.
        let ran = Cell::new(false);
        uipi_verdict(
            Some(0x2000),
            || PostProbe::Delivered,
            || {
                ran.set(true);
                TokenProbe::Failed
            },
        );
        assert!(!ran.get());
    }

    #[test]
    fn a_null_window_is_never_probed() {
        assert_eq!(input_blocked_by_uipi(0, 1234), None);
    }

    // ───────────── desktop tests (run with `cargo test -p vox-win -- --ignored` on an interactive desktop) ─────────

    /// Prints what the foreground window looks like; run from a terminal (the terminal is the foreground window).
    #[test]
    #[ignore = "needs an interactive desktop"]
    fn describes_the_foreground_window() {
        let info = foreground_window_info().expect("a foreground window");
        assert!(info.hwnd != 0);
        assert!(info.info.pid != 0);
        assert!(!info.info.process_name.is_empty());
        assert_eq!(info.info.process_name, info.info.process_name.to_lowercase());
        assert!(info.info.title.chars().count() <= 300);
        println!("{info:?} text cursor: {:?} fullscreen: {}", text_cursor_shown(), fullscreen_app_running());
        // Our own process is never an elevated target of itself.
        assert_eq!(own_integrity().map(|rid| rid > 0), Some(true));
    }

    /// Start an elevated Notepad (Run as administrator) and bring it to the front within 5 s of starting the test:
    /// `cargo test -p vox-win uipi_against_elevated_notepad -- --ignored --nocapture` (from a non-elevated terminal).
    /// For the "Run as different user" case, expect `Some(false)` instead.
    #[test]
    #[ignore = "needs an elevated Notepad in the foreground"]
    fn uipi_against_elevated_notepad() {
        std::thread::sleep(std::time::Duration::from_secs(5));
        let info = foreground_window_info().expect("a foreground window");
        let verdict = input_blocked_by_uipi(info.hwnd, info.info.pid);
        println!("{} → {verdict:?}", info.info.process_name);
        assert_eq!(verdict, Some(true));
    }
}
