//! One installed add-on: its manifest, its version folder, its voices and its helper client.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::RwLock;
use serde_json::Value;
use vox_core::addons::{parse_manifest, parse_voices, AddonManifest, ManifestVoice};

use crate::helper::{HelperClient, HelperError, HelperSpec, HelperTimeouts};

/// The manifest file at the root of every version folder.
pub const MANIFEST_FILE: &str = "addon.json";
/// The voice list Vox writes at install time for an add-on whose manifest lists none.
pub const VOICES_FILE: &str = "voices.json";
/// Manifests and voice lists are small; anything bigger is not one.
const MAX_JSON: u64 = 1 << 20;

/// A loaded add-on. Dropping the last reference quits its helper (a replaced add-on finishes the request it is
/// serving first).
pub struct AddonRuntime {
    pub manifest: AddonManifest,
    /// The version folder.
    pub dir: PathBuf,
    voices: RwLock<Option<Arc<Vec<ManifestVoice>>>>,
    pub helper: HelperClient,
}

/// Reads and checks the add-on in `dir`: `addon.json` must parse, and its program and notices must be files inside
/// `dir` (on Windows the program must be an `.exe`). Returns the manifest and how to start its helper, with the
/// manifest's timeouts; `data_dir` becomes `VOX_ADDON_DATA`.
pub fn load_spec(dir: &Path, data_dir: Option<&Path>) -> Result<(AddonManifest, HelperSpec), String> {
    let manifest = parse_manifest(&read_small(&dir.join(MANIFEST_FILE))?)?;
    let root = dir.canonicalize().map_err(|e| format!("{}: {e}", dir.display()))?;
    let inside = |rel: &str| -> Result<PathBuf, String> {
        let p = root.join(rel).canonicalize().map_err(|e| format!("{rel}: {e}"))?;
        if !p.starts_with(&root) || !p.is_file() {
            return Err(format!("{rel} is not a file inside the add-on folder"));
        }
        Ok(p)
    };
    let program = inside(&manifest.exe)?;
    if cfg!(windows) && !manifest.exe.to_ascii_lowercase().ends_with(".exe") {
        return Err(format!("{} is not an .exe", manifest.exe));
    }
    for notice in &manifest.notices {
        inside(notice)?;
    }
    let mut spec = HelperSpec::new(&manifest.id, &manifest.name, manifest.version.as_str(), plain(program), plain(root));
    spec.data_dir = data_dir.map(Path::to_path_buf);
    spec.args = manifest.args.clone();
    spec.env = manifest.env.clone();
    spec.timeouts = HelperTimeouts::from_manifest(&manifest.timeouts);
    Ok((manifest, spec))
}

impl AddonRuntime {
    /// [`load_spec`], then the runtime with that spec.
    pub fn load(dir: &Path, data_dir: Option<&Path>) -> Result<AddonRuntime, String> {
        let (manifest, spec) = load_spec(dir, data_dir)?;
        Ok(AddonRuntime::new(manifest, spec))
    }

    /// A runtime from a checked manifest and spec. The voices come from the manifest, else from `voices.json` in the
    /// spec's folder, else from the helper the first time they are asked for.
    pub fn new(manifest: AddonManifest, spec: HelperSpec) -> AddonRuntime {
        let dir = spec.dir.clone();
        let voices = manifest.voices.clone().or_else(|| read_voices_file(&dir)).map(Arc::new);
        AddonRuntime { manifest, dir, voices: RwLock::new(voices), helper: HelperClient::new(spec) }
    }

    pub fn id(&self) -> &str {
        &self.manifest.id
    }

    /// The voices, when known without asking the helper.
    pub fn known_voices(&self) -> Option<Arc<Vec<ManifestVoice>>> {
        self.voices.read().clone()
    }

    /// The voices, asking the helper once (and remembering the answer) when neither the manifest nor `voices.json`
    /// lists them.
    pub fn voices(&self) -> Result<Arc<Vec<ManifestVoice>>, HelperError> {
        if let Some(v) = self.known_voices() {
            return Ok(v);
        }
        let asked = Arc::new(self.helper.voices()?);
        *self.voices.write() = Some(asked.clone());
        Ok(asked)
    }

    /// The current key for `key` (a renamed voice keeps working through `voice_aliases`).
    pub fn resolve_voice<'a>(&'a self, key: &'a str) -> &'a str {
        self.manifest.voice_aliases.get(key).map_or(key, String::as_str)
    }
}

fn read_small(path: &Path) -> Result<Vec<u8>, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.len() > MAX_JSON {
        return Err(format!("{} is too big", path.display()));
    }
    std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))
}

fn read_voices_file(dir: &Path) -> Option<Vec<ManifestVoice>> {
    let path = dir.join(VOICES_FILE);
    if !path.exists() {
        return None;
    }
    let parsed = read_small(&path).and_then(|b| serde_json::from_slice::<Value>(&b).map_err(|e| e.to_string())).and_then(|v| {
        if v.get("schema").and_then(Value::as_u64) != Some(1) {
            return Err("schema is not 1".into());
        }
        parse_voices(v.get("voices").unwrap_or(&Value::Null))
    });
    match parsed {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(target: "addons", "{}: ignored: {e}", path.display());
            None
        }
    }
}

/// `canonicalize` gives `\\?\C:\…` on Windows; CreateProcess and the working directory want the plain form.
fn plain(p: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = p.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            if rest.as_bytes().get(1) == Some(&b':') {
                return PathBuf::from(rest);
            }
        }
    }
    p
}
