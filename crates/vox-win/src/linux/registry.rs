//! Linux counterparts of the Windows registry integrations:
//! - "Explorer" verb → a Nautilus script (`~/.local/share/nautilus/scripts/<label>`, right-click → Scripts) that runs
//!   `vox --read-file <file>`;
//! - launch at login → `~/.config/autostart/<name>.desktop`;
//! - app theme → GNOME `org.gnome.desktop.interface color-scheme`;
//! - system proxy → `HTTPS_PROXY` / `ALL_PROXY` (+ `NO_PROXY`) from the environment.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplorerMenuSpec {
    /// `<exe> --read-file "$1"` (built with [`build_verb_command`]).
    pub command: String,
    /// The script's name in Nautilus' Scripts menu.
    pub label: String,
    pub icon_path: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuState {
    Absent,
    Stale,
    Current,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemProxy {
    pub url: String,
    pub bypass: Vec<String>,
    pub bypass_local: bool,
}

const SCRIPT_MARK: &str = "# Vox: read the selected file aloud";

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

fn data_home() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).or_else(|| home().map(|h| h.join(".local/share")))
}

fn config_home() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).or_else(|| home().map(|h| h.join(".config")))
}

fn scripts_dir() -> Option<PathBuf> {
    data_home().map(|d| d.join("nautilus").join("scripts"))
}

/// Every Vox script in the Nautilus scripts folder (found by its marker line, whatever its label).
fn our_scripts() -> Vec<(PathBuf, String)> {
    let Some(dir) = scripts_dir() else { return Vec::new() };
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let text = std::fs::read_to_string(e.path()).ok()?;
            text.contains(SCRIPT_MARK).then(|| (e.path(), text))
        })
        .collect()
}

pub fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+=:,".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// `<quote(exe)> [quote(arg)…] --read-file "$1"`. Pure.
pub fn build_verb_command(exe: &str, extra_args: &[&str]) -> String {
    let mut parts = vec![shell_quote(exe)];
    parts.extend(extra_args.iter().map(|a| shell_quote(a)));
    parts.push("--read-file".into());
    parts.push("\"$1\"".into());
    parts.join(" ")
}

fn script_text(spec: &ExplorerMenuSpec) -> String {
    format!("#!/bin/sh\n{SCRIPT_MARK}\nexec {}\n", spec.command)
}

pub fn set_explorer_menu(enabled: bool, spec: &ExplorerMenuSpec) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    for (path, _) in our_scripts() {
        std::fs::remove_file(&path).map_err(|e| e.to_string())?;
    }
    if !enabled {
        return Ok(());
    }
    let dir = scripts_dir().ok_or("cannot resolve the Nautilus scripts folder")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let name: String = spec.label.chars().map(|c| if c == '/' { '-' } else { c }).collect();
    let path = dir.join(name);
    std::fs::write(&path, script_text(spec)).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    Ok(())
}

pub fn explorer_menu_state(expected_command: &str) -> MenuState {
    let scripts = our_scripts();
    if scripts.is_empty() {
        MenuState::Absent
    } else if scripts.iter().any(|(_, text)| text.lines().any(|l| l == format!("exec {expected_command}"))) {
        MenuState::Current
    } else {
        MenuState::Stale
    }
}

pub fn is_explorer_menu_installed() -> bool {
    !our_scripts().is_empty()
}

/// Writes or removes `~/.config/autostart/<value_name>.desktop`.
pub fn apply_launch_at_login(enabled: bool, start_hidden: bool, value_name: &str, exe: &Path) -> Result<(), String> {
    let dir = config_home().ok_or("cannot resolve the config folder")?.join("autostart");
    let path = dir.join(format!("{}.desktop", value_name.to_lowercase()));
    if !enabled {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        };
    }
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut exec = shell_quote(&exe.to_string_lossy());
    if start_hidden {
        exec.push_str(" --hidden");
    }
    let text = format!(
        "[Desktop Entry]\nType=Application\nName={value_name}\nComment=Read selected text aloud\nExec={exec}\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
    );
    std::fs::write(&path, text).map_err(|e| e.to_string())
}

/// GNOME's colour scheme: `prefer-dark` → `Some(false)`; `default` / `prefer-light` → `Some(true)`.
pub fn apps_use_light_theme() -> Option<bool> {
    let v = super::run_output("gsettings", &["get", "org.gnome.desktop.interface", "color-scheme"])?;
    Some(!v.contains("dark"))
}

/// The proxy from `HTTPS_PROXY` / `https_proxy` / `ALL_PROXY` / `all_proxy`, bypass list from `NO_PROXY`.
pub fn system_proxy() -> Option<SystemProxy> {
    let var = |n: &str| std::env::var(n).ok();
    let raw = ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
        .iter()
        .filter_map(|n| var(n))
        .map(|v| v.trim().to_string())
        .find(|v| !v.is_empty())?;
    let url = if raw.contains("://") { raw } else { format!("http://{raw}") };
    let no_proxy = ["NO_PROXY", "no_proxy"].iter().filter_map(|n| var(n)).find(|v| !v.trim().is_empty()).unwrap_or_default();
    let bypass = no_proxy.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect();
    Some(SystemProxy { url, bypass, bypass_local: false })
}
