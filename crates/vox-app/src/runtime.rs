//! Runtime facts resolved once at startup (shell.md §1; ARCHITECTURE.md §2.9): environment flags, the profile
//! folder, executable and browser-extension paths, packaged vs dev, version strings.

use std::path::{Path, PathBuf};

use vox_core::app::RuntimeFlags;
use vox_store::Profile;

/// Everything the rest of the app needs to know about how it was started.
#[derive(Debug, Clone)]
pub struct Runtime {
    pub flags: RuntimeFlags,
    pub profile: Profile,
    /// `std::env::current_exe()`.
    pub exe_path: PathBuf,
    /// The unpacked browser extension shipped next to the exe (`<exe dir>\browser-extension`; dev builds: the repo's
    /// `browser-extension\`, found by walking up from the exe to the folder containing `Cargo.toml` + that folder).
    /// Decision D-INT-13: portable layout `<dir>\vox.exe` + `<dir>\browser-extension\` (installer later).
    pub extension_dir: PathBuf,
    /// Release build (decision D-INT-7: `!cfg!(debug_assertions)`); dev builds use the `Vox (dev)` Run value name and
    /// the exe itself as the Explorer verb icon.
    pub is_packaged: bool,
    /// `Rust 1.98 · Slint 1.18.1` (About → runtime row).
    pub runtime_label: String,
    /// `env!("VOX_BUILD_HASH")` (build.rs), `dev` when unknown (About → build row).
    pub build_hash: String,
    /// The process's own argv and cwd (handled after bootstrap). Read with `std::env::args_os()` and converted with
    /// `to_string_lossy()` (never `env::args()`, which panics on an unpaired surrogate in an NTFS name passed by the
    /// Explorer verb); such a path then fails to open with the normal `Couldn’t open <name>` toast.
    pub argv: Vec<String>,
    pub cwd: PathBuf,
}

impl Runtime {
    /// Reads `VOX_*` from the environment (`RuntimeFlags::from_lookup(std::env::var)`; `VOX_SHOTS_DIR` made absolute
    /// against the cwd), resolves the profile (`%APPDATA%\Vox` or `Vox-Demo`), the exe and extension paths.
    pub fn detect() -> anyhow::Result<Runtime> {
        let argv: Vec<String> = std::env::args_os().map(|a| a.to_string_lossy().into_owned()).collect();
        let exe_path = std::env::current_exe().map_err(|e| anyhow::anyhow!("cannot locate vox.exe: {e}"))?;
        let exe_dir = exe_path.parent().map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        // A vanished working directory must not stop the app: fall back to the exe's folder.
        let cwd = std::env::current_dir().unwrap_or_else(|_| exe_dir.clone());
        let mut flags = RuntimeFlags::from_lookup(|k| std::env::var(k).ok());
        flags.shots_dir = flags.shots_dir.take().map(|dir| absolute_against(&cwd, dir));
        let profile = Profile::resolve(flags.demo).map_err(|e| anyhow::anyhow!("cannot open the profile folder: {e}"))?;
        Ok(Runtime {
            flags,
            profile,
            extension_dir: extension_dir_for(&exe_dir),
            exe_path,
            is_packaged: !cfg!(debug_assertions),
            runtime_label: runtime_label(),
            build_hash: Self::compiled_build_hash().to_string(),
            argv,
            cwd,
        })
    }

    /// The single-instance key: `com.vox.reader`, or `com.vox.reader-Demo` for the demo profile.
    pub fn instance_key(&self) -> String {
        if self.flags.demo {
            format!("{}-Demo", vox_core::constants::SINGLE_INSTANCE_KEY)
        } else {
            vox_core::constants::SINGLE_INSTANCE_KEY.to_string()
        }
    }

    /// The build hash compiled in by build.rs.
    pub fn compiled_build_hash() -> &'static str {
        env!("VOX_BUILD_HASH")
    }
}

/// `Rust 1.98 · Slint 1.18.1` (the workspace's `rust-version`; Slint is pinned to `=1.18.1`).
pub fn runtime_label() -> String {
    format!("Rust {} \u{00B7} Slint 1.18.1", env!("CARGO_PKG_RUST_VERSION"))
}

/// `path.resolve(cwd, dir)`.
fn absolute_against(cwd: &Path, dir: PathBuf) -> PathBuf {
    if dir.is_absolute() {
        dir
    } else {
        cwd.join(dir)
    }
}

/// D-INT-13: `<exe dir>\browser-extension` when it exists (portable layout); in a dev build, the repo's
/// `browser-extension\` found by walking up from the exe to the folder holding `Cargo.toml` and that folder; else the
/// portable path (Settings shows it; revealing a missing folder only logs).
fn extension_dir_for(exe_dir: &Path) -> PathBuf {
    const NAME: &str = "browser-extension";
    let portable = exe_dir.join(NAME);
    if portable.is_dir() {
        return portable;
    }
    exe_dir
        .ancestors()
        .find(|dir| dir.join("Cargo.toml").is_file() && dir.join(NAME).is_dir())
        .map(|dir| dir.join(NAME))
        .unwrap_or(portable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_keys_and_paths() {
        let rt = |demo: bool| Runtime {
            flags: RuntimeFlags { demo, ..RuntimeFlags::default() },
            profile: Profile::at("C:\\p", demo),
            exe_path: PathBuf::from("C:\\x\\vox.exe"),
            extension_dir: PathBuf::from("C:\\x\\browser-extension"),
            is_packaged: false,
            runtime_label: runtime_label(),
            build_hash: "dev".into(),
            argv: vec![],
            cwd: PathBuf::from("C:\\x"),
        };
        assert_eq!(rt(false).instance_key(), "com.vox.reader");
        assert_eq!(rt(true).instance_key(), "com.vox.reader-Demo");
        assert!(runtime_label().starts_with("Rust 1."));
        assert!(runtime_label().ends_with(" \u{00B7} Slint 1.18.1"));
        assert_eq!(absolute_against(Path::new("C:\\cwd"), PathBuf::from("shots")), PathBuf::from("C:\\cwd\\shots"));
        assert_eq!(absolute_against(Path::new("C:\\cwd"), PathBuf::from("D:\\s")), PathBuf::from("D:\\s"));
    }

    #[test]
    fn dev_builds_find_the_repo_extension_folder() {
        // The test binary lives under <repo>\target\debug\deps; the repo holds Cargo.toml + browser-extension\.
        let exe = std::env::current_exe().unwrap();
        let dir = extension_dir_for(exe.parent().unwrap());
        assert!(dir.ends_with("browser-extension"));
        assert!(dir.is_dir(), "{}", dir.display());
    }
}
