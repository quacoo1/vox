//! Shell helpers on Linux: xdg-open, the FileManager1 D-Bus service (over gdbus) to reveal a file, notify-send,
//! zenity for the save dialog and message boxes, xdg-user-dir for Documents.

use std::path::{Path, PathBuf};

use super::{gvariant_str, run_output, spawn_detached};

pub fn open_external(url: &str) -> Result<(), crate::WinError> {
    spawn_detached("xdg-open", &[url])
}

/// Shows a file selected in the file manager; a folder is opened.
pub fn reveal_path(path: &Path) -> Result<(), crate::WinError> {
    if path.is_dir() {
        return spawn_detached("xdg-open", &[&path.to_string_lossy()]);
    }
    let uri = file_uri(path);
    let ok = std::process::Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.freedesktop.FileManager1",
            "--object-path",
            "/org/freedesktop/FileManager1",
            "--method",
            "org.freedesktop.FileManager1.ShowItems",
            &format!("[{}]", gvariant_str(&uri)),
            "",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if ok {
        return Ok(());
    }
    let folder = path.parent().unwrap_or(path);
    spawn_detached("xdg-open", &[&folder.to_string_lossy()])
}

fn file_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for b in path.to_string_lossy().bytes() {
        if b.is_ascii_alphanumeric() || b"/-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn show_notification(title: &str, body: &str) -> Result<(), crate::WinError> {
    spawn_detached("notify-send", &["--app-name=Vox", title, body])
}

/// Windows-only (toast identity); nothing to register on Linux.
pub fn register_app_user_model(_id: &str, _display_name: &str, _icon_png: &Path) -> Result<(), crate::WinError> {
    Ok(())
}

/// A zenity save dialog (blocks until closed). `None` when cancelled or zenity is missing.
pub fn save_file_dialog(_owner: Option<isize>, title: &str, default_path: &Path, filter_name: &str, extension: &str) -> Option<PathBuf> {
    let filter = format!("{filter_name} | *.{extension}");
    let chosen = run_output(
        "zenity",
        &["--file-selection", "--save", "--confirm-overwrite", "--title", title, "--filename", &default_path.to_string_lossy(), "--file-filter", &filter],
    )?;
    if chosen.is_empty() {
        return None;
    }
    let mut path = PathBuf::from(chosen);
    if path.extension().is_none() {
        path.set_extension(extension);
    }
    Some(path)
}

pub fn set_app_user_model_id(_id: &str) {}

pub fn documents_folder() -> Option<PathBuf> {
    if let Some(dir) = run_output("xdg-user-dir", &["DOCUMENTS"]).filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Documents"))
}

pub fn set_per_monitor_dpi_aware() {}

/// A modal information box (zenity), else stderr.
pub fn message_box(title: &str, body: &str) {
    let shown = std::process::Command::new("zenity").args(["--info", "--title", title, "--text", body, "--no-markup"]).status().is_ok_and(|s| s.success());
    if !shown {
        eprintln!("{title}: {body}");
    }
}
