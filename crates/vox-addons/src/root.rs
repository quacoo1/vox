//! [`AddonsRoot`]: the add-ons folder on disk (docs/addons-design.md "Directories").
//!
//! ```text
//! <root>/.staging/          the installer's work in progress
//! <root>/<id>/current.json  the ONLY thing that makes an add-on installed
//! <root>/<id>/<dir>/        addon.json, voices.json?, bin/…, models/… (named by current.json)
//! <root>/<id>/data/         helper-writable (VOX_ADDON_DATA), kept across updates
//! ```
//!
//! The scan reads small JSON files only and starts no helper. The sweep runs on a worker after startup and deletes
//! what an interrupted install or remove left behind. `VOX_ADDONS_DIR` can point at a helper author's own working
//! folder, so the sweep only ever deletes what Vox could have made itself: the contents of `.staging`, and version
//! folders named `<version>-<8 random characters>` (with the `data/` next to them once nothing installed is left).
//! Anything else in the root stays, and an add-on placed by hand (`source: "local"`) or with an unreadable
//! `current.json` is left alone entirely.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use vox_core::addons::{is_valid_addon_id, parse_installed_pointer, InstallSource, InstalledPointer, Version};
use vox_core::constants::ADDON_PART_MAX_AGE_SEC;
use vox_store::fsutil::remove_dir_all_retry;

use crate::runtime::{read_small, AddonRuntime};

pub const POINTER_FILE: &str = "current.json";
pub const DATA_DIR: &str = "data";
pub const STAGING_DIR: &str = ".staging";

const PART_SUFFIX: &str = ".part";
const PART_INFO_SUFFIX: &str = ".part.json";

/// An add-on that loaded.
pub struct InstalledAddon {
    pub pointer: InstalledPointer,
    pub runtime: AddonRuntime,
}

/// One add-on the scan found: loaded, or why it could not be (its `current.json` stays, so it can be repaired).
pub struct ScanItem {
    pub id: String,
    pub result: Result<InstalledAddon, String>,
}

/// What a sweep did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub removed: usize,
    pub failed: usize,
}

impl SweepReport {
    /// Deletes a folder, file or link (a link is removed, never followed).
    fn delete(&mut self, path: &Path) {
        if remove_dir_all_retry(path) {
            tracing::info!(target: "addons", "removed {}", path.display());
            self.removed += 1;
        } else {
            self.failed += 1;
        }
    }
}

enum Pointer {
    Missing,
    Unreadable(String),
    Valid(InstalledPointer),
}

/// The add-ons folder. Opening it touches nothing; it need not exist yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddonsRoot {
    path: PathBuf,
}

impl AddonsRoot {
    pub fn open(path: impl Into<PathBuf>) -> AddonsRoot {
        AddonsRoot { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn staging(&self) -> PathBuf {
        self.path.join(STAGING_DIR)
    }

    /// Every add-on with a `current.json`, sorted by id. Problems are logged and reported in the item.
    pub fn scan(&self) -> Vec<ScanItem> {
        let mut items = Vec::new();
        for (id, path) in children(&self.path) {
            // Follows a link: a helper author may link `<id>` to their build folder.
            if !is_valid_addon_id(&id) || !path.is_dir() {
                continue;
            }
            let result = match self.pointer(&id) {
                Pointer::Missing => continue,
                Pointer::Unreadable(e) => Err(e),
                Pointer::Valid(pointer) => self.load(&id, pointer),
            };
            if let Err(e) = &result {
                tracing::warn!(target: "addons", "{id}: not loaded: {e}");
            }
            items.push(ScanItem { id, result });
        }
        items.sort_by(|a, b| a.id.cmp(&b.id));
        items
    }

    /// Deletes what interrupted installs and removes left behind (module docs). Safe to run only while no install is
    /// running, since it would take an install's staging files for leftovers.
    pub fn sweep(&self) -> SweepReport {
        let mut report = SweepReport::default();
        self.sweep_staging(&mut report);
        for (id, path) in children(&self.path) {
            if is_valid_addon_id(&id) && is_real_dir(&path) {
                self.sweep_addon(&id, &path, &mut report);
            }
        }
        report
    }

    fn pointer(&self, id: &str) -> Pointer {
        let file = self.path.join(id).join(POINTER_FILE);
        match std::fs::symlink_metadata(&file) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Pointer::Missing,
            Err(e) => return Pointer::Unreadable(format!("{}: {e}", file.display())),
            Ok(_) => {}
        }
        match read_small(&file).and_then(|bytes| parse_installed_pointer(&bytes)) {
            Ok(pointer) => Pointer::Valid(pointer),
            Err(e) => Pointer::Unreadable(format!("{POINTER_FILE}: {e}")),
        }
    }

    fn load(&self, id: &str, pointer: InstalledPointer) -> Result<InstalledAddon, String> {
        let base = self.path.join(id);
        let dir = base.join(&pointer.dir);
        if !dir.is_dir() {
            return Err(format!("{} is missing", dir.display()));
        }
        let runtime = AddonRuntime::load(&dir, Some(&base.join(DATA_DIR)))?;
        if runtime.manifest.id != id {
            return Err(format!("addon.json says it is {:?}", runtime.manifest.id));
        }
        // By hand, addon.json is the truth: an author bumping its version need not touch current.json.
        if pointer.source == InstallSource::Catalogue && runtime.manifest.version != pointer.version {
            return Err(format!("addon.json is version {}, current.json says {}", runtime.manifest.version, pointer.version));
        }
        Ok(InstalledAddon { pointer, runtime })
    }

    fn sweep_staging(&self, report: &mut SweepReport) {
        let staging = self.staging();
        if !is_real_dir(&staging) {
            return;
        }
        for (name, path) in children(&staging) {
            if !keep_staged(&staging, &name) {
                report.delete(&path);
            }
        }
    }

    fn sweep_addon(&self, id: &str, path: &Path, report: &mut SweepReport) {
        match self.pointer(id) {
            // Unsure what is current, or not ours to tidy.
            Pointer::Unreadable(e) => tracing::debug!(target: "addons", "{id}: not swept: {e}"),
            Pointer::Valid(p) if p.source == InstallSource::Local => {}
            Pointer::Valid(p) => {
                // Pointing at nothing, it can be repaired; the other versions are not in anyone's way until then.
                if !path.join(&p.dir).is_dir() {
                    return;
                }
                for (name, child) in children(path) {
                    // Windows folder names ignore case, so a differently cased pointer still names this folder.
                    if is_real_dir(&child) && is_version_folder(&name) && !name.eq_ignore_ascii_case(&p.dir) {
                        report.delete(&child);
                    }
                }
            }
            // Not installed: an interrupted first install or remove leaves version folders behind (and maybe `data/`).
            // Without one, nothing here is provably Vox's: a bare `data/` or an empty folder may be anyone's.
            Pointer::Missing => {
                let mut versions = Vec::new();
                let mut data = None;
                for (name, child) in children_all(path).unwrap_or_default() {
                    if is_real_dir(&child) && is_version_folder(&name) {
                        versions.push(child);
                    } else if is_real_dir(&child) && name == DATA_DIR {
                        data = Some(child);
                    } else {
                        tracing::debug!(target: "addons", "{id}: no {POINTER_FILE}, but it holds {name:?}, which Vox did not make; left alone");
                        return;
                    }
                }
                if versions.is_empty() {
                    return;
                }
                for version in &versions {
                    report.delete(version);
                }
                if let Some(data) = data {
                    report.delete(&data);
                }
                // Not recursive: anything that appeared meanwhile stays, and so does the folder.
                if std::fs::remove_dir(path).is_ok() {
                    report.removed += 1;
                }
            }
        }
    }
}

/// A resumable download: `<name>.part` younger than the limit with its `<name>.part.json`, and that info file.
fn keep_staged(staging: &Path, name: &str) -> bool {
    let stem = name.strip_suffix(PART_INFO_SUFFIX).or_else(|| name.strip_suffix(PART_SUFFIX));
    let Some(stem) = stem else { return false };
    let part = staging.join(format!("{stem}{PART_SUFFIX}"));
    let info = staging.join(format!("{stem}{PART_INFO_SUFFIX}"));
    let Ok(meta) = std::fs::symlink_metadata(&part) else { return false };
    if !meta.is_file() || !std::fs::symlink_metadata(&info).is_ok_and(|m| m.is_file()) {
        return false;
    }
    // A clock that went backwards reads as "just written".
    let age = meta.modified().ok().and_then(|at| SystemTime::now().duration_since(at).ok()).unwrap_or(Duration::ZERO);
    age < Duration::from_secs(ADDON_PART_MAX_AGE_SEC)
}

/// `<version>-<8 lower-case letters or digits>`, the only folder name the installer creates.
fn is_version_folder(name: &str) -> bool {
    let Some((version, rand)) = name.rsplit_once('-') else { return false };
    rand.len() == 8 && rand.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()) && Version::parse(version).is_some()
}

/// Is a directory itself, not a link to one.
fn is_real_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

/// The entries of `dir` that have UTF-8 names (none if it cannot be read).
fn children(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    entries.flatten().filter_map(|e| Some((e.file_name().into_string().ok()?, e.path()))).collect()
}

/// Every entry of `dir`; `None` if it cannot be read or a name is not UTF-8, so a caller deciding whether a folder is
/// entirely Vox's own cannot miss an entry (it then sees nothing, and deletes nothing).
fn children_all(dir: &Path) -> Option<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).ok()? {
        let entry = entry.ok()?;
        out.push((entry.file_name().into_string().ok()?, entry.path()));
    }
    Some(out)
}
