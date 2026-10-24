//! Registry integrations: the Explorer "Read with Vox" verb (integrations.md §3), launch at login (§4) and the OS app
//! theme (shell.md §5.6). Direct registry API (`winreg`), no `reg.exe`; `ERROR_FILE_NOT_FOUND` on delete = success.
//!
//! Uninstall (integrations.md §3.6) is deferred with R-Installer (D-INT-17): portable builds remove the nine
//! `…\shell\VoxRead` keys and the `Vox` Run/StartupApproved values only through the Settings toggles; because the Rust
//! build reuses the Electron names, uninstalling the old Electron build deletes them too, and the next start of
//! `vox.exe` repairs both (startup sync).
//!
//! Every value is written as `REG_SZ`; reads accept `REG_SZ` and `REG_EXPAND_SZ` and never expand. Nothing here logs
//! more than paths and OS error messages.

use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use vox_core::constants::{EXPLORER_EXTENSIONS, EXPLORER_VERB, HIDDEN_ARG};
use vox_core::js::is_js_whitespace;
use winreg::enums::{RegType, KEY_READ, KEY_SET_VALUE};
use winreg::{RegValue, HKCU};

/// Explorer verb install parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplorerMenuSpec {
    /// `<exe> --read-file "%1"` (built with [`build_verb_command`]).
    pub command: String,
    /// `READ_WITH_LABEL`.
    pub label: String,
    /// The `Icon` value: `"<exe>",0` for packaged **and** dev builds (D-INT-7; the exe's first icon group, embedded by
    /// vox-app's build.rs). `None` deletes `Icon`.
    pub icon_path: Option<String>,
}

/// Where the integrations live (all under `HKCU`). `Default` = the real keys: `classes_base` =
/// `Software\Classes\SystemFileAssociations`, `run_key` = `Software\Microsoft\Windows\CurrentVersion\Run`,
/// `startup_approved_key` = `Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run`. Tests use
/// `Software\VoxTest\<uuid>\…` and delete it afterwards, so they never touch the developer's real `Vox` Run value or
/// `VoxRead` verbs (the Electron build may be installed on the same machine).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryLocation {
    pub classes_base: String,
    pub run_key: String,
    pub startup_approved_key: String,
}

impl Default for RegistryLocation {
    fn default() -> Self {
        Self {
            classes_base: r"Software\Classes\SystemFileAssociations".into(),
            run_key: r"Software\Microsoft\Windows\CurrentVersion\Run".into(),
            startup_approved_key: r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run".into(),
        }
    }
}

/// The launch-at-login idempotency test (integrations.md §4.2; D-INT-9, D-INT-10). Pure: parses `stored` (the raw,
/// unexpanded `REG_SZ`/`REG_EXPAND_SZ` text) with `CommandLineToArgvW` rules; true when `argv[0]` equals `exe`
/// case-insensitively and every further argument equals `args` exactly (including `--hidden`), quoted or unquoted.
/// Vectors: `"C:\A B\vox.exe"` vs exe `c:\a b\VOX.EXE`, no args → true; unquoted `C:\A B\vox.exe` → false;
/// `"C:\x\vox.exe" --hidden` with no args → false and with `--hidden` → true; `C:\x\vox.exe --hidden` with `--hidden`
/// → true; `""` → false.
pub fn run_command_matches(stored: &str, exe: &Path, args: &[&str]) -> bool {
    let argv = command_line_to_argv(stored);
    let Some((first, rest)) = argv.split_first() else { return false };
    if first.is_empty() {
        return false;
    }
    let exe = exe.to_string_lossy();
    if first.to_lowercase() != exe.to_lowercase() {
        return false;
    }
    rest.len() == args.len() && rest.iter().zip(args).all(|(a, b)| a == b)
}

/// `CommandLineToArgvW` in Rust (the shell32 algorithm, as documented by Wine and checked against the real function
/// in the tests): argv[0] is everything up to the next quote when it starts with one, else up to the first space or
/// tab; the other arguments follow the MSVCRT backslash/quote rules, including the "three quotes make one" rule.
/// Unlike the API, an empty command line yields no arguments (the API would return the current executable).
fn command_line_to_argv(cmd: &str) -> Vec<String> {
    let s: Vec<char> = cmd.chars().collect();
    if s.is_empty() {
        return Vec::new();
    }
    let blank = |c: char| c == ' ' || c == '\t';
    let mut args = Vec::new();
    let mut i = 0;
    let mut first = String::new();
    if s[0] == '"' {
        i = 1;
        while i < s.len() && s[i] != '"' {
            first.push(s[i]);
            i += 1;
        }
        if i < s.len() {
            i += 1;
        }
    } else {
        while i < s.len() && !blank(s[i]) {
            first.push(s[i]);
            i += 1;
        }
    }
    args.push(first);
    while i < s.len() && blank(s[i]) {
        i += 1;
    }
    if i >= s.len() {
        return args;
    }
    let mut cur = String::new();
    let mut quotes = 0u32;
    let mut backslashes = 0usize;
    while i < s.len() {
        let c = s[i];
        if blank(c) && quotes == 0 {
            args.push(std::mem::take(&mut cur));
            backslashes = 0;
            while i < s.len() && blank(s[i]) {
                i += 1;
            }
            if i >= s.len() {
                return args;
            }
        } else if c == '\\' {
            cur.push('\\');
            backslashes += 1;
            i += 1;
        } else if c == '"' {
            if backslashes.is_multiple_of(2) {
                for _ in 0..backslashes / 2 {
                    cur.pop();
                }
                quotes += 1;
            } else {
                for _ in 0..backslashes / 2 + 1 {
                    cur.pop();
                }
                cur.push('"');
            }
            i += 1;
            backslashes = 0;
            while i < s.len() && s[i] == '"' {
                quotes += 1;
                if quotes == 3 {
                    cur.push('"');
                    quotes = 0;
                }
                i += 1;
            }
            if quotes == 2 {
                quotes = 0;
            }
        } else {
            cur.push(c);
            backslashes = 0;
            i += 1;
        }
    }
    args.push(cur);
    args
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuState {
    Absent,
    Stale,
    Current,
}

/// `quoteWindowsArg` (MSVCRT / `CommandLineToArgvW` rules; unquoted when no whitespace and no `"`). Pure.
pub fn quote_windows_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.chars().any(|c| is_js_whitespace(c) || c == '"') {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        if ch == '\\' {
            backslashes += 1;
            continue;
        }
        if ch == '"' {
            out.push_str(&"\\".repeat(2 * backslashes + 1));
            out.push('"');
        } else {
            out.push_str(&"\\".repeat(backslashes));
            out.push(ch);
        }
        backslashes = 0;
    }
    out.push_str(&"\\".repeat(2 * backslashes));
    out.push('"');
    out
}

/// `<quote(exe)> [quote(arg)…] --read-file "%1"`. Pure.
pub fn build_verb_command(exe: &str, extra_args: &[&str]) -> String {
    let mut parts = vec![quote_windows_arg(exe)];
    parts.extend(extra_args.iter().map(|a| quote_windows_arg(a)));
    parts.push("--read-file".into());
    parts.push("\"%1\"".into());
    parts.join(" ")
}

/// A command without a `%1`, `%L` or `%V` placeholder (case-insensitive) gets ` "%1"` appended (integrations.md §3.2).
fn with_placeholder(command: &str) -> String {
    let b = command.as_bytes();
    let has = b.windows(2).any(|w| w[0] == b'%' && matches!(w[1], b'1' | b'l' | b'L' | b'v' | b'V'));
    if has {
        command.to_string()
    } else {
        format!("{command} \"%1\"")
    }
}

/// Serializes Explorer-verb installs and removals so a quick on/off toggle never interleaves registry writes.
static VERB_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// Installs (all 9 extensions; any failure rolls everything back) or removes the verb. Serialized internally.
/// `Err` carries the toast detail (`Could not add the Explorer entry (<ext>: <reason>)`). = `set_explorer_menu_at`
/// with the default location.
pub fn set_explorer_menu(enabled: bool, spec: &ExplorerMenuSpec) -> Result<(), String> {
    set_explorer_menu_at(&RegistryLocation::default(), enabled, spec)
}

/// [`set_explorer_menu`] under `loc` (tests).
pub fn set_explorer_menu_at(loc: &RegistryLocation, enabled: bool, spec: &ExplorerMenuSpec) -> Result<(), String> {
    let _serial = VERB_LOCK.lock();
    if !enabled {
        return remove_all_verbs(loc);
    }
    let command = spec.command.trim();
    let label = spec.label.trim();
    if command.is_empty() || label.is_empty() {
        return Err("Missing command or label".into());
    }
    let command = with_placeholder(command);
    // Each extension's steps run in order; a failure stops only that extension, the first failure in extension order
    // is reported (integrations.md §3.3).
    let mut first_failure: Option<(&str, String)> = None;
    for ext in EXPLORER_EXTENSIONS {
        if let Err(e) = install_verb(loc, ext, label, &command, spec.icon_path.as_deref()) {
            if first_failure.is_none() {
                first_failure = Some((ext, os_reason(&e)));
            }
        }
    }
    match first_failure {
        None => Ok(()),
        Some((ext, reason)) => {
            // Never leave a half-installed menu behind; the rollback's own errors are ignored.
            let _ = remove_all_verbs(loc);
            Err(format!("Could not add the Explorer entry ({ext}: {reason})"))
        }
    }
}

fn verb_key_path(loc: &RegistryLocation, ext: &str) -> String {
    format!(r"{}\{}\shell\{}", loc.classes_base, ext, EXPLORER_VERB)
}

fn install_verb(loc: &RegistryLocation, ext: &str, label: &str, command: &str, icon: Option<&str>) -> io::Result<()> {
    let (verb, _) = HKCU.create_subkey(verb_key_path(loc, ext))?;
    verb.set_value("", &label.to_string())?;
    verb.set_value("MultiSelectModel", &"Single".to_string())?;
    match icon {
        Some(icon) => verb.set_value("Icon", &icon.to_string())?,
        // Removes a stale icon from an older install; failures (e.g. absent) are ignored.
        None => {
            let _ = verb.delete_value("Icon");
        }
    }
    let (cmd, _) = verb.create_subkey("command")?;
    cmd.set_value("", &command.to_string())?;
    Ok(())
}

fn remove_all_verbs(loc: &RegistryLocation) -> Result<(), String> {
    let mut first_failure: Option<(&str, String)> = None;
    for ext in EXPLORER_EXTENSIONS {
        if let Err(e) = delete_tree(&verb_key_path(loc, ext)) {
            if first_failure.is_none() {
                first_failure = Some((ext, os_reason(&e)));
            }
        }
    }
    match first_failure {
        None => Ok(()),
        Some((ext, reason)) => Err(format!("Could not remove the Explorer entry ({ext}: {reason})")),
    }
}

/// Deletes a key and everything below it; a missing key is success.
fn delete_tree(path: &str) -> io::Result<()> {
    match HKCU.delete_subkey_all(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    }
    // RegDeleteTreeW removes the key itself too; make sure (a no-op "not found" otherwise).
    match HKCU.delete_subkey(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// The OS message of an I/O error without Rust's ` (os error N)` suffix.
fn os_reason(e: &io::Error) -> String {
    let text = e.to_string();
    match text.rfind(" (os error ") {
        Some(i) if text.ends_with(')') => text[..i].trim().to_string(),
        _ => text.trim().to_string(),
    }
}

/// `explorerMenuState(expected)` (case-insensitive command comparison): 0 of 9 `command` keys → `Absent`; all 9 equal
/// ignoring case → `Current`; anything else (8 of 9, one different command) → `Stale`. A `REG_EXPAND_SZ` value is
/// compared raw (unexpanded).
pub fn explorer_menu_state(expected_command: &str) -> MenuState {
    explorer_menu_state_at(&RegistryLocation::default(), expected_command)
}

/// [`explorer_menu_state`] under `loc` (tests).
pub fn explorer_menu_state_at(loc: &RegistryLocation, expected_command: &str) -> MenuState {
    let commands: Vec<Option<String>> = EXPLORER_EXTENSIONS.iter().map(|ext| read_verb_command(loc, ext)).collect();
    menu_state(&commands, expected_command)
}

/// The state from each extension's command (`None` = no `command` key). Pure.
fn menu_state(commands: &[Option<String>], expected: &str) -> MenuState {
    let present = commands.iter().filter(|c| c.is_some()).count();
    if present == 0 {
        return MenuState::Absent;
    }
    let expected = expected.trim().to_lowercase();
    let all_equal = commands.iter().all(|c| c.as_ref().is_some_and(|c| c.trim().to_lowercase() == expected));
    if present == commands.len() && present == EXPLORER_EXTENSIONS.len() && all_equal {
        MenuState::Current
    } else {
        MenuState::Stale
    }
}

/// The `command` key's default value (`""` when it is missing or not a string); `None` when the key is absent.
fn read_verb_command(loc: &RegistryLocation, ext: &str) -> Option<String> {
    let key = HKCU.open_subkey_with_flags(format!(r"{}\command", verb_key_path(loc, ext)), KEY_READ).ok()?;
    Some(key.get_raw_value("").ok().and_then(|v| reg_string(&v)).unwrap_or_default())
}

/// A `REG_SZ` / `REG_EXPAND_SZ` value as text, raw (never expanded), without its terminating NULs.
fn reg_string(v: &RegValue) -> Option<String> {
    if !matches!(v.vtype, RegType::REG_SZ | RegType::REG_EXPAND_SZ) {
        return None;
    }
    let units: Vec<u16> = v.bytes.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    Some(String::from_utf16_lossy(&units[..end]))
}

/// The `command` key exists for all nine extensions.
pub fn is_explorer_menu_installed() -> bool {
    is_explorer_menu_installed_at(&RegistryLocation::default())
}

/// [`is_explorer_menu_installed`] under `loc` (tests).
pub fn is_explorer_menu_installed_at(loc: &RegistryLocation) -> bool {
    EXPLORER_EXTENSIONS
        .iter()
        .all(|ext| HKCU.open_subkey_with_flags(format!(r"{}\command", verb_key_path(loc, ext)), KEY_READ).is_ok())
}

/// `applyLaunchAtLogin(enabled, startHidden)` under `HKCU\…\Run` value `value_name` (`Vox` / `Vox (dev)`): enabling
/// is idempotent when [`run_command_matches`] (same exe case-insensitively, same args exactly), otherwise writes
/// `"<exe>"[ --hidden]` and deletes the `StartupApproved\Run` value; disabling deletes both.
pub fn apply_launch_at_login(enabled: bool, start_hidden: bool, value_name: &str, exe: &Path) -> Result<(), String> {
    apply_launch_at_login_at(&RegistryLocation::default(), enabled, start_hidden, value_name, exe)
}

/// [`apply_launch_at_login`] under `loc` (tests).
pub fn apply_launch_at_login_at(
    loc: &RegistryLocation,
    enabled: bool,
    start_hidden: bool,
    value_name: &str,
    exe: &Path,
) -> Result<(), String> {
    let fail = |e: io::Error| format!("could not update the login item: {}", os_reason(&e));
    if !enabled {
        delete_value(&loc.run_key, value_name).map_err(fail)?;
        delete_value(&loc.startup_approved_key, value_name).map_err(fail)?;
        return Ok(());
    }
    let args: &[&str] = if start_hidden { &[HIDDEN_ARG] } else { &[] };
    let stored = HKCU
        .open_subkey_with_flags(&loc.run_key, KEY_READ)
        .ok()
        .and_then(|k| k.get_raw_value(value_name).ok())
        .and_then(|v| reg_string(&v));
    if stored.as_deref().is_some_and(|s| run_command_matches(s, exe, args)) {
        // Already registered: leave the Task Manager (StartupApproved) state alone.
        return Ok(());
    }
    let (run, _) = HKCU.create_subkey(&loc.run_key).map_err(fail)?;
    run.set_value(value_name, &run_value(exe, start_hidden)).map_err(fail)?;
    delete_value(&loc.startup_approved_key, value_name).map_err(fail)?;
    Ok(())
}

/// The Run value data: `"<exe>"` or `"<exe>" --hidden`.
fn run_value(exe: &Path, start_hidden: bool) -> String {
    let mut v = format!("\"{}\"", exe.to_string_lossy());
    if start_hidden {
        v.push(' ');
        v.push_str(HIDDEN_ARG);
    }
    v
}

/// Deletes a value; a missing key or value is success.
fn delete_value(key_path: &str, name: &str) -> io::Result<()> {
    let key = match HKCU.open_subkey_with_flags(key_path, KEY_SET_VALUE) {
        Ok(k) => k,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    match key.delete_value(name) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// `AppsUseLightTheme` (`Some(false)` = dark apps); `None` when unreadable.
pub fn apps_use_light_theme() -> Option<bool> {
    let key =
        HKCU.open_subkey_with_flags(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize", KEY_READ).ok()?;
    let v: u32 = key.get_value("AppsUseLightTheme").ok()?;
    Some(v != 0)
}

/// The Windows (WinINET) proxy for the current user, for the ElevenLabs client (BRIEF.md "honour the Windows system
/// proxy"; ARCHITECTURE.md §8 D-NET-1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemProxy {
    /// `http://host:port`.
    pub url: String,
    /// `ProxyOverride` entries except `<local>`, as written (`*.corp`, `10.*`, hosts).
    pub bypass: Vec<String>,
    /// `<local>` present: hosts without a dot bypass the proxy.
    pub bypass_local: bool,
}

/// Reads, in order: the `HTTPS_PROXY`/`https_proxy`/`ALL_PROXY` environment variables (a developer override), then
/// `HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings`: `ProxyEnable == 1` and `ProxyServer` — either
/// `host:port` or per-protocol `http=h:p;https=h2:p2;…` (pick `https=`, else `http=`, else the first entry);
/// `ProxyOverride` split on `;`. `None` when disabled, absent or unparsable. Auto-config (`AutoConfigURL`, WPAD) is
/// **not** evaluated (documented limitation): such users get a direct connection.
///
/// With an environment proxy, `NO_PROXY` / `no_proxy` (comma separated) fill `bypass`.
pub fn system_proxy() -> Option<SystemProxy> {
    if let Some(proxy) = proxy_from_env(|name| std::env::var(name).ok()) {
        return Some(proxy);
    }
    let key =
        HKCU.open_subkey_with_flags(r"Software\Microsoft\Windows\CurrentVersion\Internet Settings", KEY_READ).ok()?;
    let auto_config: Option<String> = key.get_value("AutoConfigURL").ok();
    if auto_config.is_some_and(|s| !s.trim().is_empty()) {
        static LOGGED: AtomicBool = AtomicBool::new(false);
        if !LOGGED.swap(true, Ordering::Relaxed) {
            tracing::info!(target: "net", "proxy auto-config (AutoConfigURL) is not supported; it is ignored");
        }
    }
    let enabled: u32 = key.get_value("ProxyEnable").unwrap_or(0);
    if enabled != 1 {
        return None;
    }
    let server: String = key.get_value("ProxyServer").ok()?;
    let url = parse_proxy_server(&server)?;
    let overrides: String = key.get_value("ProxyOverride").unwrap_or_default();
    let (bypass, bypass_local) = parse_proxy_override(&overrides);
    Some(SystemProxy { url, bypass, bypass_local })
}

/// The environment override (pure over a variable lookup).
fn proxy_from_env(var: impl Fn(&str) -> Option<String>) -> Option<SystemProxy> {
    let raw = ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
        .iter()
        .filter_map(|n| var(n))
        .map(|v| v.trim().to_string())
        .find(|v| !v.is_empty())?;
    let url = if raw.contains("://") { raw } else { format!("http://{raw}") };
    let no_proxy =
        ["NO_PROXY", "no_proxy"].iter().filter_map(|n| var(n)).find(|v| !v.trim().is_empty()).unwrap_or_default();
    let bypass = no_proxy.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect();
    Some(SystemProxy { url, bypass, bypass_local: false })
}

/// `ProxyOverride` → (entries, `<local>` present).
fn parse_proxy_override(value: &str) -> (Vec<String>, bool) {
    let mut local = false;
    let mut out = Vec::new();
    for entry in value.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        if entry.eq_ignore_ascii_case("<local>") {
            local = true;
        } else {
            out.push(entry.to_string());
        }
    }
    (out, local)
}

/// Pure parser for a `ProxyServer` value (tested): `"proxy:8080"` → `http://proxy:8080`; `"http=a:1;https=b:2"` →
/// `http://b:2`; `""` → `None`.
pub fn parse_proxy_server(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let chosen = if value.contains('=') {
        let entries: Vec<(String, String)> = value
            .split(|c: char| c == ';' || c.is_whitespace())
            .filter_map(|e| {
                let (proto, addr) = e.split_once('=')?;
                let addr = addr.trim();
                (!addr.is_empty()).then(|| (proto.trim().to_ascii_lowercase(), addr.to_string()))
            })
            .collect();
        let pick = |p: &str| entries.iter().find(|(proto, _)| proto == p).map(|(_, a)| a.clone());
        pick("https").or_else(|| pick("http")).or_else(|| entries.first().map(|(_, a)| a.clone()))?
    } else {
        value.split(';').map(str::trim).find(|s| !s.is_empty())?.to_string()
    };
    if chosen.chars().any(char::is_whitespace) {
        return None;
    }
    let address = chosen.strip_prefix("http://").unwrap_or(&chosen).trim_end_matches('/');
    if address.is_empty() {
        return None;
    }
    Some(if address.contains("://") { address.to_string() } else { format!("http://{address}") })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn quote_windows_arg_follows_msvcrt_rules() {
        assert_eq!(
            quote_windows_arg(r"C:\Users\Kwaku\AppData\Local\Programs\Vox\Vox.exe"),
            r"C:\Users\Kwaku\AppData\Local\Programs\Vox\Vox.exe"
        );
        assert_eq!(quote_windows_arg(r"C:\Program Files\Vox\Vox.exe"), r#""C:\Program Files\Vox\Vox.exe""#);
        assert_eq!(quote_windows_arg(""), r#""""#);
        assert_eq!(quote_windows_arg(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quote_windows_arg(r"C:\A B\"), r#""C:\A B\\""#);
        assert_eq!(quote_windows_arg(r#"a\"b c"#), r#""a\\\"b c""#);
        assert_eq!(quote_windows_arg(r"a\\b c"), r#""a\\b c""#);
        assert_eq!(quote_windows_arg("tab\there"), "\"tab\there\"");
        // JS \s includes NBSP: quoted.
        assert_eq!(quote_windows_arg("a\u{A0}b"), "\"a\u{A0}b\"");
        // Round trip through the argv parser for every quoted form.
        for arg in [r"C:\A B\", r#"a"b"#, r#"a\"b c"#, "", r"x\\y z\\", "plain"] {
            let line = format!("exe {}", quote_windows_arg(arg));
            assert_eq!(command_line_to_argv(&line), vec!["exe".to_string(), arg.to_string()], "{arg:?}");
        }
    }

    #[test]
    fn verb_command_matches_the_spec_example() {
        assert_eq!(
            build_verb_command(r"C:\Users\Kwaku\AppData\Local\Programs\Vox\Vox.exe", &[]),
            r#"C:\Users\Kwaku\AppData\Local\Programs\Vox\Vox.exe --read-file "%1""#
        );
        assert_eq!(
            build_verb_command(r"C:\Program Files\Vox\vox.exe", &[]),
            r#""C:\Program Files\Vox\vox.exe" --read-file "%1""#
        );
        assert_eq!(
            build_verb_command(r"C:\dev\electron.exe", &[r"C:\my app"]),
            r#"C:\dev\electron.exe "C:\my app" --read-file "%1""#
        );
    }

    #[test]
    fn placeholder_is_appended_only_when_missing() {
        assert_eq!(with_placeholder(r"x.exe --read-file"), r#"x.exe --read-file "%1""#);
        assert_eq!(with_placeholder(r#"x.exe "%1""#), r#"x.exe "%1""#);
        assert_eq!(with_placeholder(r#"x.exe "%l""#), r#"x.exe "%l""#);
        assert_eq!(with_placeholder(r#"x.exe %V"#), r#"x.exe %V"#);
        assert_eq!(with_placeholder(r#"x.exe %2"#), r#"x.exe %2 "%1""#);
    }

    #[test]
    fn run_command_matches_vectors() {
        let ab = PathBuf::from(r"c:\a b\VOX.EXE");
        assert!(run_command_matches(r#""C:\A B\vox.exe""#, &ab, &[]));
        assert!(!run_command_matches(r"C:\A B\vox.exe", &PathBuf::from(r"C:\A B\vox.exe"), &[]));
        let x = PathBuf::from(r"C:\x\vox.exe");
        assert!(!run_command_matches(r#""C:\x\vox.exe" --hidden"#, &x, &[]));
        assert!(run_command_matches(r#""C:\x\vox.exe" --hidden"#, &x, &["--hidden"]));
        assert!(run_command_matches(r"C:\x\vox.exe --hidden", &x, &["--hidden"]));
        assert!(!run_command_matches("", &x, &[]));
        assert!(!run_command_matches(r#""""#, &x, &[]));
        // Arguments compare exactly (case-sensitive), every one of them.
        assert!(!run_command_matches(r#""C:\x\vox.exe" --HIDDEN"#, &x, &["--hidden"]));
        assert!(!run_command_matches(r#""C:\x\vox.exe" --hidden extra"#, &x, &["--hidden"]));
        assert!(!run_command_matches(r#""C:\y\vox.exe""#, &x, &[]));
        // Trailing whitespace adds no argument.
        assert!(run_command_matches("\"C:\\x\\vox.exe\"  \t", &x, &[]));
        // Non-ASCII user names compare case-insensitively too.
        assert!(run_command_matches(r#""C:\Users\ÉVA\vox.exe""#, &PathBuf::from(r"c:\users\éva\vox.exe"), &[]));
        // What we write always matches itself.
        for hidden in [false, true] {
            let args: &[&str] = if hidden { &["--hidden"] } else { &[] };
            assert!(run_command_matches(&run_value(&ab, hidden), &ab, args));
            assert!(!run_command_matches(&run_value(&ab, hidden), &ab, if hidden { &[] } else { &["--hidden"] }));
        }
        assert_eq!(run_value(&x, true), r#""C:\x\vox.exe" --hidden"#);
    }

    #[test]
    fn argv_parser_agrees_with_command_line_to_argv_w() {
        let cases = [
            r#""C:\A B\vox.exe" --hidden"#,
            r"C:\x\vox.exe --hidden",
            r#"a "b c" d"#,
            r#"a b\"c d"#,
            r#"a "b\\" c"#,
            r#"a "b\\\"" c"#,
            r#"a """b""" c"#,
            r#"a "" b"#,
            r#"a """" b"#,
            r#"a b"c"d e"#,
            "\"x\"y z",
            "x  \t y\t",
            r#"a \\\\"b c" d"#,
            r#"a "unterminated b"#,
            r"a \\server\share\ b",
        ];
        for case in cases {
            assert_eq!(command_line_to_argv(case), real_argv(case), "{case}");
        }
    }

    fn real_argv(cmd: &str) -> Vec<String> {
        use windows::core::HSTRING;
        use windows::Win32::Foundation::{LocalFree, HLOCAL};
        use windows::Win32::UI::Shell::CommandLineToArgvW;
        let mut n = 0i32;
        // SAFETY: the string outlives the call; the returned block is read, then freed with LocalFree.
        unsafe {
            let argv = CommandLineToArgvW(&HSTRING::from(cmd), &mut n);
            assert!(!argv.is_null());
            let out = (0..n as usize).map(|i| (*argv.add(i)).to_string().unwrap_or_default()).collect();
            let _ = LocalFree(Some(HLOCAL(argv as *mut _)));
            out
        }
    }

    #[test]
    fn menu_state_vectors() {
        let expected = r#"C:\x\vox.exe --read-file "%1""#;
        let none: Vec<Option<String>> = vec![None; 9];
        assert_eq!(menu_state(&none, expected), MenuState::Absent);
        let all: Vec<Option<String>> = vec![Some(expected.to_uppercase()); 9];
        assert_eq!(menu_state(&all, expected), MenuState::Current);
        let mut eight = all.clone();
        eight[4] = None;
        assert_eq!(menu_state(&eight, expected), MenuState::Stale);
        let mut different = all.clone();
        different[8] = Some(r#"C:\old\vox.exe --read-file "%1""#.into());
        assert_eq!(menu_state(&different, expected), MenuState::Stale);
        let mut blank = all;
        blank[0] = Some(String::new());
        assert_eq!(menu_state(&blank, expected), MenuState::Stale);
        // Surrounding whitespace on either side is ignored.
        let padded: Vec<Option<String>> = vec![Some(format!("  {expected} ")); 9];
        assert_eq!(menu_state(&padded, &format!("{expected}\n")), MenuState::Current);
    }

    #[test]
    fn os_reason_drops_the_rust_suffix() {
        assert_eq!(os_reason(&io::Error::from_raw_os_error(5)), "Access is denied.");
        assert_eq!(os_reason(&io::Error::other("custom")), "custom");
    }

    #[test]
    fn proxy_server_values() {
        assert_eq!(parse_proxy_server("proxy:8080").as_deref(), Some("http://proxy:8080"));
        assert_eq!(parse_proxy_server("http=a:1;https=b:2").as_deref(), Some("http://b:2"));
        assert_eq!(parse_proxy_server("").as_deref(), None);
        assert_eq!(parse_proxy_server("   ").as_deref(), None);
        assert_eq!(parse_proxy_server("http=a:1;ftp=f:21").as_deref(), Some("http://a:1"));
        assert_eq!(parse_proxy_server("ftp=f:21;socks=s:1080").as_deref(), Some("http://f:21"));
        assert_eq!(parse_proxy_server("HTTPS=b:2 http=a:1").as_deref(), Some("http://b:2"));
        assert_eq!(parse_proxy_server("https=;http=a:1").as_deref(), Some("http://a:1"));
        assert_eq!(parse_proxy_server("http://proxy.corp:3128/").as_deref(), Some("http://proxy.corp:3128"));
        assert_eq!(parse_proxy_server("10.0.0.1:8080;").as_deref(), Some("http://10.0.0.1:8080"));
        assert_eq!(parse_proxy_server("a b:1").as_deref(), None);
        assert_eq!(parse_proxy_server("https=").as_deref(), None);
    }

    #[test]
    fn proxy_override_and_environment() {
        assert_eq!(
            parse_proxy_override("*.corp;<local>; 10.* ;;intranet"),
            (vec!["*.corp".to_string(), "10.*".to_string(), "intranet".to_string()], true)
        );
        assert_eq!(parse_proxy_override(""), (vec![], false));
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |n: &str| pairs.iter().find(|(k, _)| *k == n).map(|(_, v)| v.to_string())
        };
        assert_eq!(proxy_from_env(env(&[])), None);
        assert_eq!(proxy_from_env(env(&[("HTTPS_PROXY", "  ")])), None);
        assert_eq!(
            proxy_from_env(env(&[("https_proxy", "p:1"), ("NO_PROXY", "a, .b ,")])),
            Some(SystemProxy { url: "http://p:1".into(), bypass: vec!["a".into(), ".b".into()], bypass_local: false })
        );
        assert_eq!(
            proxy_from_env(env(&[("ALL_PROXY", "socks5://s:1080"), ("HTTPS_PROXY", "")])).map(|p| p.url),
            Some("socks5://s:1080".into())
        );
        assert_eq!(
            proxy_from_env(env(&[("HTTPS_PROXY", "http://h:3"), ("ALL_PROXY", "x:1")])).map(|p| p.url),
            Some("http://h:3".into())
        );
    }

    /// A private `HKCU\Software\VoxTest\<unique>` subtree, deleted on drop (also when a test panics).
    struct TestHive {
        root: String,
        loc: RegistryLocation,
    }

    impl TestHive {
        fn new(tag: &str) -> TestHive {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let nanos =
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
            let id = format!("{tag}-{}-{nanos}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed));
            let root = format!(r"Software\VoxTest\{id}");
            let loc = RegistryLocation {
                classes_base: format!(r"{root}\Classes\SystemFileAssociations"),
                run_key: format!(r"{root}\Run"),
                startup_approved_key: format!(r"{root}\StartupApproved\Run"),
            };
            TestHive { root, loc }
        }
    }

    impl Drop for TestHive {
        fn drop(&mut self) {
            let _ = delete_tree(&self.root);
            let _ = HKCU.delete_subkey(r"Software\VoxTest");
        }
    }

    #[test]
    fn explorer_verb_install_state_and_remove_under_a_test_location() {
        let hive = TestHive::new("verb");
        let loc = &hive.loc;
        let command = build_verb_command(r"C:\x y\vox.exe", &[]);
        let spec = ExplorerMenuSpec {
            command: command.clone(),
            label: "Read with Vox".into(),
            icon_path: Some(r#""C:\x y\vox.exe",0"#.into()),
        };
        assert_eq!(explorer_menu_state_at(loc, &command), MenuState::Absent);
        assert!(!is_explorer_menu_installed_at(loc));

        set_explorer_menu_at(loc, true, &spec).expect("install");
        assert_eq!(explorer_menu_state_at(loc, &command), MenuState::Current);
        assert_eq!(explorer_menu_state_at(loc, &command.to_uppercase()), MenuState::Current);
        assert_eq!(explorer_menu_state_at(loc, "other.exe --read-file \"%1\""), MenuState::Stale);
        assert!(is_explorer_menu_installed_at(loc));
        let verb = HKCU.open_subkey(verb_key_path(loc, ".md")).expect("verb key");
        assert_eq!(verb.get_value::<String, _>("").unwrap_or_default(), "Read with Vox");
        assert_eq!(verb.get_value::<String, _>("MultiSelectModel").unwrap_or_default(), "Single");
        assert_eq!(verb.get_value::<String, _>("Icon").unwrap_or_default(), r#""C:\x y\vox.exe",0"#);
        // Idempotent re-install.
        set_explorer_menu_at(loc, true, &spec).expect("install again");
        assert_eq!(explorer_menu_state_at(loc, &command), MenuState::Current);

        // 8 of 9 → stale.
        delete_tree(&verb_key_path(loc, ".htm")).expect("delete one");
        assert_eq!(explorer_menu_state_at(loc, &command), MenuState::Stale);
        assert!(!is_explorer_menu_installed_at(loc));
        set_explorer_menu_at(loc, true, &spec).expect("repair");

        // A REG_EXPAND_SZ value is compared raw.
        let (cmd, _) = HKCU.create_subkey(format!(r"{}\command", verb_key_path(loc, ".txt"))).expect("command key");
        let raw: Vec<u8> = "%SystemRoot%\\vox.exe\0".encode_utf16().flat_map(u16::to_le_bytes).collect();
        cmd.set_raw_value("", &RegValue { bytes: raw.into(), vtype: RegType::REG_EXPAND_SZ }).expect("expand sz");
        assert_eq!(read_verb_command(loc, ".txt").as_deref(), Some("%SystemRoot%\\vox.exe"));
        assert_eq!(explorer_menu_state_at(loc, &command), MenuState::Stale);

        // Icon None deletes it.
        set_explorer_menu_at(loc, true, &ExplorerMenuSpec { icon_path: None, ..spec.clone() }).expect("no icon");
        let verb = HKCU.open_subkey(verb_key_path(loc, ".md")).expect("verb key");
        assert!(verb.get_value::<String, _>("Icon").is_err());

        // Missing pieces are refused before any write.
        assert_eq!(
            set_explorer_menu_at(loc, true, &ExplorerMenuSpec { label: " ".into(), ..spec.clone() }),
            Err("Missing command or label".into())
        );

        set_explorer_menu_at(loc, false, &spec).expect("remove");
        assert_eq!(explorer_menu_state_at(loc, &command), MenuState::Absent);
        // Removing again is fine (not found = success).
        set_explorer_menu_at(loc, false, &spec).expect("remove again");
    }

    #[test]
    fn launch_at_login_is_idempotent_under_a_test_location() {
        let hive = TestHive::new("run");
        let loc = &hive.loc;
        let exe = PathBuf::from(r"C:\Program Files\Vox\vox.exe");
        let name = "Vox (test)";
        let read = |path: &str| -> Option<RegValue> { HKCU.open_subkey(path).ok()?.get_raw_value(name).ok() };
        let approved = |bytes: &[u8]| RegValue { bytes: bytes.to_vec().into(), vtype: RegType::REG_BINARY };

        apply_launch_at_login_at(loc, true, false, name, &exe).expect("enable");
        assert_eq!(
            read(&loc.run_key).and_then(|v| reg_string(&v)).as_deref(),
            Some(r#""C:\Program Files\Vox\vox.exe""#)
        );

        // The user disables it in Task Manager; an unchanged enable must keep that.
        let (sa, _) = HKCU.create_subkey(&loc.startup_approved_key).expect("startup approved");
        sa.set_raw_value(name, &approved(&[3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])).expect("disable in task manager");
        apply_launch_at_login_at(loc, true, false, name, &exe).expect("enable again");
        assert!(read(&loc.startup_approved_key).is_some());

        // An Electron-style unquoted value for the same exe in another case also counts as registered.
        let (run, _) = HKCU.create_subkey(&loc.run_key).expect("run key");
        run.set_value(name, &r#""c:\program files\vox\VOX.exe""#.to_string()).expect("electron value");
        apply_launch_at_login_at(loc, true, false, name, &exe).expect("enable electron");
        assert!(read(&loc.startup_approved_key).is_some());

        // Turning on "start minimized" rewrites the value and re-enables it.
        apply_launch_at_login_at(loc, true, true, name, &exe).expect("hidden");
        assert_eq!(
            read(&loc.run_key).and_then(|v| reg_string(&v)).as_deref(),
            Some(r#""C:\Program Files\Vox\vox.exe" --hidden"#)
        );
        assert!(read(&loc.startup_approved_key).is_none());

        apply_launch_at_login_at(loc, false, true, name, &exe).expect("disable");
        assert!(read(&loc.run_key).is_none());
        apply_launch_at_login_at(loc, false, false, name, &exe).expect("disable again");
    }

    /// `cargo test -p vox-win reads_the_real_os_settings -- --ignored --nocapture` (read-only).
    #[test]
    #[ignore = "reads the user's theme and proxy settings"]
    fn reads_the_real_os_settings() {
        println!("apps use light theme: {:?}", apps_use_light_theme());
        // The URL itself is not printed (it may carry credentials).
        let proxy = system_proxy();
        println!(
            "system proxy: {}, {} bypass entries, <local>: {}",
            if proxy.is_some() { "set" } else { "none" },
            proxy.as_ref().map_or(0, |p| p.bypass.len()),
            proxy.as_ref().is_some_and(|p| p.bypass_local)
        );
        println!("verb installed: {}", is_explorer_menu_installed());
    }
}
