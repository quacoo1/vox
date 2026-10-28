//! The profile folder and every file in it (persistence.md §1.3; ARCHITECTURE.md §6.1).
//!
//! ```text
//! %APPDATA%\Vox\                          (demo / screenshot mode: %APPDATA%\Vox-Demo, never the real one)
//!   settings.json (+ .bak .tmp .tmp-sync .corrupt)   shared with the Electron build, same schema
//!   history.json  (+ .bak .tmp .tmp-sync .corrupt)   shared with the Electron build, same schema
//!   secrets.bin                                       Electron's key; READ-ONLY for the Rust app (migration source)
//!   secrets-dpapi.bin (+ .tmp)                        new: the Rust app's key (raw DPAPI blob; 0 bytes = cleared)
//!   window-state.json (+ .tmp)                        new: main window size / position / maximized
//!   ui-state.json (+ .tmp)                            new: small UI preferences (Analytics range)
//!   audio-cache\<readId>\manifest.json + <index>.<ext>   shared with the Electron build, same format
//!   logs\main.log, main.1.log                         shared (both builds append the same line format)
//!   Session\Local State                               Electron's; read once for the key migration, never written
//! ```
//! Never create a folder named `cache` (case-insensitively it would collide with Chromium's `Cache`).

use std::path::{Path, PathBuf};

use vox_core::constants::APP_NAME;

use crate::error::StoreError;

/// `FOLDERID_RoamingAppData` (`SHGetKnownFolderPath`), falling back to `%APPDATA%`.
fn roaming_app_data() -> Result<PathBuf, StoreError> {
    if let Some(p) = known_folder_roaming() {
        return Ok(p);
    }
    match std::env::var_os("APPDATA") {
        Some(v) if !v.is_empty() => Ok(PathBuf::from(v)),
        _ => Err(StoreError::Other("could not resolve the roaming AppData folder".into())),
    }
}

#[cfg(windows)]
fn known_folder_roaming() -> Option<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_RoamingAppData, SHGetKnownFolderPath, KF_FLAG_DEFAULT};

    // SAFETY: SHGetKnownFolderPath returns a CoTaskMem-allocated, NUL-terminated wide string on success, which is
    // read once and then freed exactly once.
    unsafe {
        use std::os::windows::ffi::OsStringExt;
        let pwstr = SHGetKnownFolderPath(&FOLDERID_RoamingAppData, KF_FLAG_DEFAULT, None).ok()?;
        let path = std::ffi::OsString::from_wide(pwstr.as_wide());
        CoTaskMemFree(Some(pwstr.0 as *const core::ffi::c_void));
        (!path.is_empty()).then(|| PathBuf::from(path))
    }
}

/// `$XDG_CONFIG_HOME`, else `~/.config` (the Linux home of the profile folder).
#[cfg(not(windows))]
fn known_folder_roaming() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").filter(|v| !v.is_empty()).map(|h| PathBuf::from(h).join(".config")))
}

/// A resolved profile folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    root: PathBuf,
    demo: bool,
}

impl Profile {
    /// `%APPDATA%\Vox` (`FOLDERID_RoamingAppData` via `SHGetKnownFolderPath` + `APP_NAME`), or `%APPDATA%\Vox-Demo`
    /// when `demo`. Creates the folder. Fails only if the known folder cannot be resolved or created.
    pub fn resolve(demo: bool) -> Result<Profile, StoreError> {
        let app_data = roaming_app_data()?;
        let name = if demo { format!("{APP_NAME}-Demo") } else { APP_NAME.to_string() };
        let root = app_data.join(name);
        std::fs::create_dir_all(&root)?;
        Ok(Profile { root, demo })
    }

    /// A profile at an explicit folder (tests, tools). Does not create it.
    pub fn at(root: impl Into<PathBuf>, demo: bool) -> Profile {
        Profile { root: root.into(), demo }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn is_demo(&self) -> bool {
        self.demo
    }
    pub fn settings_file(&self) -> PathBuf {
        self.root.join("settings.json")
    }
    pub fn history_file(&self) -> PathBuf {
        self.root.join("history.json")
    }
    /// Electron's `secrets.bin` (read-only for the Rust app).
    pub fn secrets_file(&self) -> PathBuf {
        self.root.join("secrets.bin")
    }
    /// The Rust app's `secrets-dpapi.bin`.
    pub fn secrets_dpapi_file(&self) -> PathBuf {
        self.root.join("secrets-dpapi.bin")
    }
    pub fn window_state_file(&self) -> PathBuf {
        self.root.join("window-state.json")
    }
    pub fn ui_state_file(&self) -> PathBuf {
        self.root.join("ui-state.json")
    }
    pub fn audio_cache_dir(&self) -> PathBuf {
        self.root.join("audio-cache")
    }
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }
    pub fn log_file(&self) -> PathBuf {
        self.logs_dir().join("main.log")
    }
    /// Where Electron kept `Local State` (with the OSCrypt key): `Session\Local State` first, then the root.
    pub fn local_state_candidates(&self) -> [PathBuf; 2] {
        [self.root.join("Session").join("Local State"), self.root.join("Local State")]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)] // Windows path fixtures
    fn file_names() {
        let p = Profile::at("C:\\x\\Vox", false);
        assert!(p.secrets_dpapi_file().ends_with("secrets-dpapi.bin"));
        assert!(p.window_state_file().ends_with("window-state.json"));
        assert!(p.audio_cache_dir().ends_with("audio-cache"));
        assert_eq!(p.local_state_candidates()[0], PathBuf::from("C:\\x\\Vox\\Session\\Local State"));
        assert_eq!(p.local_state_candidates()[1], PathBuf::from("C:\\x\\Vox\\Local State"));
        assert!(p.settings_file().ends_with("settings.json"));
        assert!(p.history_file().ends_with("history.json"));
        assert!(p.secrets_file().ends_with("secrets.bin"));
        assert!(p.ui_state_file().ends_with("ui-state.json"));
        assert_eq!(p.log_file(), PathBuf::from("C:\\x\\Vox\\logs\\main.log"));
        assert!(!p.is_demo());
        assert_eq!(p.root(), Path::new("C:\\x\\Vox"));
    }

    #[test]
    #[cfg(windows)]
    fn roaming_app_data_resolves() {
        let p = roaming_app_data().unwrap();
        assert!(p.is_absolute());
        assert!(p.exists());
    }

    /// Creates `%APPDATA%\Vox-Demo` on this machine (harmless, but it touches the real AppData).
    /// Run with `cargo test -p vox-store -- --ignored resolve_creates_the_demo_folder`.
    #[test]
    #[ignore = "creates %APPDATA%\\Vox-Demo on the desktop running the tests"]
    fn resolve_creates_the_demo_folder() {
        let p = Profile::resolve(true).unwrap();
        assert!(p.is_demo());
        assert!(p.root().ends_with("Vox-Demo"));
        assert!(p.root().is_dir());
    }
}
