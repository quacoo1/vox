//! Offline voice add-ons: voice ids (docs/addons-design.md "Identity and routing") and the add-on manifest,
//! `addon.json` ("Catalogue and manifest").
//!
//! An add-on voice id is `addon:<add-on id>:<voice key>`, e.g. `addon:kittentts-lite:bella` and
//! `addon:kittentts-2:bella` (the two KittenTTS lines share voice names, so the add-on id keeps them apart):
//!
//! - add-on id: 1–32 of `[a-z0-9-]`, starting and ending with a letter or digit, no `--`, not a DOS device name
//!   (it is also the add-on's folder name);
//! - voice key: 1–64 of `[A-Za-z0-9._-]`, starting with a letter or digit, not ending with `.`.
//!
//! So an id is at most 103 UTF-16 units, under the 200-unit `voiceId` limit, and never contains a version, platform or
//! file name: an add-on id is permanent once published, and voice keys are a public contract of the add-on (updates
//! may add keys, not rename them), because settings, history, analytics and voice art key on the exact string.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::constants::{ADDON_PROTOCOL_MAX, ADDON_PROTOCOL_MIN};
use crate::providers::{split_voice_id, voice_id, Provider};

/// The parts of an add-on voice id, borrowed from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddonVoiceRef<'a> {
    pub addon: &'a str,
    pub voice: &'a str,
}

const DOS_DEVICES: [&str; 22] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8", "com9", "lpt1", "lpt2",
    "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// True for a well-formed add-on id (module docs).
pub fn is_valid_addon_id(s: &str) -> bool {
    let b = s.as_bytes();
    let edge = |c: u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    (1..=32).contains(&b.len())
        && b.iter().all(|&c| edge(c) || c == b'-')
        && edge(b[0])
        && edge(b[b.len() - 1])
        && !s.contains("--")
        && !DOS_DEVICES.contains(&s)
}

/// True for a well-formed voice key (module docs).
pub fn is_valid_voice_key(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=64).contains(&b.len())
        && b.iter().all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
        && b[0].is_ascii_alphanumeric()
        && b[b.len() - 1] != b'.'
}

/// `addon:<addon>:<voice>`; `None` when either part is malformed.
pub fn addon_voice_id(addon: &str, voice: &str) -> Option<String> {
    (is_valid_addon_id(addon) && is_valid_voice_key(voice)).then(|| voice_id(Provider::Addon, &format!("{addon}:{voice}")))
}

/// Parses the `rest` of an add-on voice id (`<addon>:<voice>`); `None` when it is malformed.
pub fn parse_addon_rest(rest: &str) -> Option<AddonVoiceRef<'_>> {
    let (addon, voice) = rest.split_once(':')?;
    (is_valid_addon_id(addon) && is_valid_voice_key(voice)).then_some(AddonVoiceRef { addon, voice })
}

/// Parses a full add-on voice id; `None` when it is not one or is malformed.
pub fn parse_addon_voice_id(id: &str) -> Option<AddonVoiceRef<'_>> {
    match split_voice_id(id) {
        Some((Provider::Addon, rest)) => parse_addon_rest(rest),
        _ => None,
    }
}

/// The voice's display name with its add-on's short name, so voices that share a name stay apart in lists, history
/// and analytics: `Bella` + `KittenTTS 2` → `Bella (KittenTTS 2)`; `Anna (de-DE)` → `Anna (de-DE, KittenTTS 2)`.
pub fn addon_voice_name(voice: &str, short: &str) -> String {
    let voice = voice.trim();
    match voice.strip_suffix(')').and_then(|v| v.rsplit_once(" (")) {
        Some((base, inner)) if !base.is_empty() => format!("{base} ({inner}, {short})"),
        _ => format!("{voice} ({short})"),
    }
}

// ───────────────────────────── versions ─────────────────────────────

/// An add-on version: 1–4 dot-separated numbers (`2.0.0`, `1.3`), compared numerically with missing parts as 0. It
/// also names the version folder, so it holds nothing but digits and dots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    text: String,
    parts: [u64; 4],
}

impl Version {
    pub fn parse(s: &str) -> Option<Version> {
        let pieces: Vec<&str> = s.split('.').collect();
        if !(1..=4).contains(&pieces.len()) {
            return None;
        }
        let mut parts = [0u64; 4];
        for (i, p) in pieces.iter().enumerate() {
            if p.is_empty() || p.len() > 9 || !p.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            parts[i] = p.parse().ok()?;
        }
        Some(Version { text: s.to_string(), parts })
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.parts.cmp(&other.parts).then_with(|| self.text.cmp(&other.text))
    }
}

// ───────────────────────────── paths inside an add-on ─────────────────────────────

/// A relative path inside an add-on folder (`exe`, `notices`, and later every archive entry): `/`-separated, at most
/// 8 levels and 200 bytes, each part 1–100 bytes of printable characters other than `<>:"\|?*`, never `.` or `..`, not
/// ending in a dot or a space, and not a DOS device name (also with an extension: `con.txt`). Valid on Windows and
/// Linux alike, and unable to leave the folder.
pub fn is_safe_relative_path(s: &str) -> bool {
    if s.is_empty() || s.len() > 200 || s.starts_with('/') {
        return false;
    }
    let parts: Vec<&str> = s.split('/').collect();
    parts.len() <= 8
        && parts.iter().all(|part| {
            let stem = part.split('.').next().unwrap_or("").to_ascii_lowercase();
            (1..=100).contains(&part.len())
                && *part != "."
                && *part != ".."
                && !part.ends_with(['.', ' '])
                && part.chars().all(|c| !c.is_control() && !matches!(c, '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*'))
                && !DOS_DEVICES.contains(&stem.trim_end())
        })
}

// ───────────────────────────── addon.json ─────────────────────────────

/// How long Vox waits for a helper (docs/addons-design.md "Timeouts"), in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// Spawn → `hello` reply.
    pub start_ms: u64,
    /// Added to the first request after a spawn (model loading).
    pub load_ms: u64,
    /// A pending request fails when no frame for it arrives for this long (progress frames reset it).
    pub idle_ms: u64,
    /// Absolute limit of one `synthesize`.
    pub synth_max_ms: u64,
}

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts { start_ms: 20_000, load_ms: 60_000, idle_ms: 60_000, synth_max_ms: 120_000 }
    }
}

impl Timeouts {
    /// Into the ranges a manifest may ask for: start 5–180 s, load 0–600 s, idle 5–600 s, synth 10 s–30 min.
    pub fn clamp(self) -> Self {
        Timeouts {
            start_ms: self.start_ms.clamp(5_000, 180_000),
            load_ms: self.load_ms.min(600_000),
            idle_ms: self.idle_ms.clamp(5_000, 600_000),
            synth_max_ms: self.synth_max_ms.clamp(10_000, 1_800_000),
        }
    }
}

/// One voice an add-on offers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ManifestVoice {
    pub key: String,
    pub name: String,
    pub gender: String,
    pub language: String,
    pub accent: String,
    pub use_case: String,
    pub description: String,
}

/// A parsed, validated `addon.json` (schema 1). Paths are only checked for shape here; whether they exist is the
/// loader's business.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddonManifest {
    pub id: String,
    pub version: Version,
    pub name: String,
    /// The suffix of voice names (`Bella (KittenTTS 2)`); defaults to `name`.
    pub short_name: String,
    pub description: String,
    /// The helper protocol majors it speaks, already known to overlap Vox's.
    pub protocol: (u32, u32),
    /// The helper program, relative to the version folder.
    pub exe: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Clamped.
    pub timeouts: Timeouts,
    /// Quit the helper after this long unused (0 = never); 0 or 30–3600.
    pub idle_exit_sec: u64,
    pub credit: String,
    pub notices: Vec<String>,
    /// `None`: the add-on did not list them (Vox asks the helper once and caches the answer).
    pub voices: Option<Vec<ManifestVoice>>,
    /// Old voice key → current key, for a voice an update renamed.
    pub voice_aliases: BTreeMap<String, String>,
}

const MAX_VOICES: usize = 1000;
const MAX_ARGS: usize = 64;
const MAX_ENV: usize = 64;

/// Parses and validates `addon.json`; the error says what is wrong (for the log).
pub fn parse_manifest(bytes: &[u8]) -> Result<AddonManifest, String> {
    let v: Value = serde_json::from_slice(bytes).map_err(|e| format!("not JSON: {e}"))?;
    let Value::Object(m) = &v else { return Err("not a JSON object".into()) };
    if m.get("schema").and_then(Value::as_u64) != Some(1) {
        return Err("schema is not 1".into());
    }
    let id = req_str(m, "id")?;
    if !is_valid_addon_id(id) {
        return Err(format!("bad id {id:?}"));
    }
    let version = Version::parse(req_str(m, "version")?).ok_or("bad version")?;
    let name = display_text(req_str(m, "name")?, 60).ok_or("bad name")?;
    let short_name = match m.get("short_name") {
        None => name.clone(),
        Some(s) => display_text(s.as_str().ok_or("bad short_name")?, 40).ok_or("bad short_name")?,
    };
    let description = opt_text(m, "description", 300)?;
    let credit = opt_text(m, "credit", 100)?;

    let protocol = m.get("protocol").ok_or("no protocol")?;
    let pmin = protocol.get("min").and_then(Value::as_u64).ok_or("bad protocol")?;
    let pmax = protocol.get("max").and_then(Value::as_u64).ok_or("bad protocol")?;
    if pmin > pmax || pmax > u64::from(u32::MAX) {
        return Err("bad protocol".into());
    }
    if pmax < u64::from(ADDON_PROTOCOL_MIN) || pmin > u64::from(ADDON_PROTOCOL_MAX) {
        return Err(format!("protocol {pmin}-{pmax} is not supported"));
    }

    let exe = req_str(m, "exe")?;
    if !is_safe_relative_path(exe) {
        return Err(format!("bad exe path {exe:?}"));
    }
    let args = match m.get("args") {
        None => Vec::new(),
        Some(Value::Array(a)) if a.len() <= MAX_ARGS => a
            .iter()
            .map(|x| x.as_str().filter(|s| s.len() <= 1024 && !s.contains('\0')).map(str::to_string))
            .collect::<Option<Vec<_>>>()
            .ok_or("bad args")?,
        Some(_) => return Err("bad args".into()),
    };
    let env = match m.get("env") {
        None => Vec::new(),
        Some(Value::Object(e)) if e.len() <= MAX_ENV => {
            let mut out = Vec::with_capacity(e.len());
            for (k, val) in e {
                let val = val.as_str().filter(|s| s.len() <= 4096 && !s.contains('\0')).ok_or("bad env value")?;
                if !is_allowed_env_key(k) {
                    return Err(format!("env key {k:?} is not allowed"));
                }
                out.push((k.clone(), val.to_string()));
            }
            out
        }
        Some(_) => return Err("bad env".into()),
    };

    let t = m.get("timeouts");
    let field = |k: &str, default: u64| t.and_then(|t| t.get(k)).and_then(Value::as_u64).unwrap_or(default);
    let d = Timeouts::default();
    let timeouts = Timeouts {
        start_ms: field("start_ms", d.start_ms),
        load_ms: field("load_ms", d.load_ms),
        idle_ms: field("idle_ms", d.idle_ms),
        synth_max_ms: field("synth_max_ms", d.synth_max_ms),
    }
    .clamp();
    let idle_exit_sec = match m.get("idle_exit_sec").and_then(Value::as_u64).unwrap_or(300) {
        0 => 0,
        n => n.clamp(30, 3600),
    };

    let notices = match m.get("notices") {
        None => Vec::new(),
        Some(Value::Array(a)) if a.len() <= 32 => a
            .iter()
            .map(|x| x.as_str().filter(|s| is_safe_relative_path(s)).map(str::to_string))
            .collect::<Option<Vec<_>>>()
            .ok_or("bad notices")?,
        Some(_) => return Err("bad notices".into()),
    };

    let voices = match m.get("voices") {
        None | Some(Value::Null) => None,
        Some(v) => Some(parse_voices(v)?),
    };

    let voice_aliases = match m.get("voice_aliases") {
        None | Some(Value::Null) => BTreeMap::new(),
        Some(Value::Object(a)) if a.len() <= MAX_VOICES => {
            let mut out = BTreeMap::new();
            for (k, val) in a {
                let val = val.as_str().filter(|s| is_valid_voice_key(s)).ok_or("bad voice alias")?;
                if !is_valid_voice_key(k) {
                    return Err("bad voice alias".into());
                }
                out.insert(k.clone(), val.to_string());
            }
            out
        }
        Some(_) => return Err("bad voice_aliases".into()),
    };

    Ok(AddonManifest {
        id: id.to_string(),
        version,
        name,
        short_name,
        description,
        protocol: (pmin as u32, pmax as u32),
        exe: exe.to_string(),
        args,
        env,
        timeouts,
        idle_exit_sec,
        credit,
        notices,
        voices,
        voice_aliases,
    })
}

/// A voice list: `addon.json` `voices`, `voices.json` `voices`, or a helper's `voices` reply. At most 1000, with valid
/// and unique keys and 1–60-character names.
pub fn parse_voices(v: &Value) -> Result<Vec<ManifestVoice>, String> {
    let Value::Array(items) = v else { return Err("voices is not a list".into()) };
    if items.len() > MAX_VOICES {
        return Err("too many voices".into());
    }
    let mut out: Vec<ManifestVoice> = Vec::with_capacity(items.len());
    for item in items {
        let Value::Object(o) = item else { return Err("bad voice".into()) };
        let key = o.get("key").and_then(Value::as_str).filter(|k| is_valid_voice_key(k)).ok_or("bad voice key")?;
        if out.iter().any(|x| x.key == key) {
            return Err(format!("voice key {key:?} is listed twice"));
        }
        let name = o.get("name").and_then(Value::as_str).and_then(|n| display_text(n, 60)).ok_or("bad voice name")?;
        let text = |k: &str, max: usize| o.get(k).and_then(Value::as_str).and_then(|s| display_text(s, max)).unwrap_or_default();
        out.push(ManifestVoice {
            key: key.to_string(),
            name,
            gender: text("gender", 30),
            language: text("language", 35),
            accent: text("accent", 40),
            use_case: text("use_case", 40),
            description: text("description", 300),
        });
    }
    Ok(out)
}

fn req_str<'a>(m: &'a serde_json::Map<String, Value>, k: &str) -> Result<&'a str, String> {
    m.get(k).and_then(Value::as_str).ok_or_else(|| format!("no {k}"))
}

fn opt_text(m: &serde_json::Map<String, Value>, k: &str, max: usize) -> Result<String, String> {
    match m.get(k) {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(String::new()),
        Some(Value::String(s)) => display_text(s, max).ok_or_else(|| format!("bad {k}")),
        Some(_) => Err(format!("bad {k}")),
    }
}

/// Trimmed text for the UI: 1–`max` characters, no control characters.
fn display_text(s: &str, max: usize) -> Option<String> {
    let t = s.trim();
    (!t.is_empty() && t.chars().count() <= max && !t.chars().any(char::is_control)).then(|| t.to_string())
}

/// `[A-Z][A-Z0-9_]{0,63}`, and nothing that changes how programs are found or loaded, or that Vox sets itself.
fn is_allowed_env_key(k: &str) -> bool {
    let b = k.as_bytes();
    (1..=64).contains(&b.len())
        && b[0].is_ascii_uppercase()
        && b.iter().all(|&c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
        && k != "PATH"
        && k != "PATHEXT"
        && !k.starts_with("LD_")
        && !k.starts_with("DYLD_")
        && !k.starts_with("VOX_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip() {
        let id = addon_voice_id("kittentts-2", "bella").unwrap();
        assert_eq!(id, "addon:kittentts-2:bella");
        assert_eq!(parse_addon_voice_id(&id), Some(AddonVoiceRef { addon: "kittentts-2", voice: "bella" }));
        assert_eq!(parse_addon_voice_id("addon:kokoro:af_heart.v2"), Some(AddonVoiceRef { addon: "kokoro", voice: "af_heart.v2" }));
        // Not add-on ids.
        assert_eq!(parse_addon_voice_id("polly:neural:Joanna"), None);
        assert_eq!(parse_addon_voice_id("21m00Tcm4TlvDq8ikWAM"), None);
        assert_eq!(parse_addon_voice_id("addon:"), None);
        assert_eq!(parse_addon_voice_id("addon:kittentts"), None);
        // A colon in the voice key is not allowed, so the split is unambiguous.
        assert_eq!(parse_addon_voice_id("addon:a:b:c"), None);
    }

    #[test]
    fn add_on_ids_are_strict() {
        for ok in ["a", "kittentts-lite", "kittentts-2", "k2", &"a".repeat(32)] {
            assert!(is_valid_addon_id(ok), "{ok}");
        }
        for bad in ["", "-a", "a-", "a--b", "A", "kitten_tts", "kitten.tts", "a b", "con", "lpt1", "../x", &"a".repeat(33)] {
            assert!(!is_valid_addon_id(bad), "{bad}");
        }
    }

    #[test]
    fn voice_keys_are_strict() {
        for ok in ["bella", "Bella", "af_heart", "en-US.v2", "x", &"v".repeat(64)] {
            assert!(is_valid_voice_key(ok), "{ok}");
        }
        for bad in ["", ".bella", "_a", "bella.", "a:b", "a b", "a/b", "ä", &"v".repeat(65)] {
            assert!(!is_valid_voice_key(bad), "{bad}");
        }
        assert_eq!(addon_voice_id("kittentts-2", "a:b"), None);
        assert_eq!(addon_voice_id("Kitten", "bella"), None);
        // The longest id fits the 200-unit voiceId limit.
        assert_eq!(addon_voice_id(&"a".repeat(32), &"v".repeat(64)).unwrap().len(), 103);
    }

    #[test]
    fn names_carry_the_add_on() {
        assert_eq!(addon_voice_name("Bella", "KittenTTS 2"), "Bella (KittenTTS 2)");
        assert_eq!(addon_voice_name("Anna (de-DE)", "KittenTTS 2"), "Anna (de-DE, KittenTTS 2)");
        assert_eq!(addon_voice_name(" Leo ", "KittenTTS Lite"), "Leo (KittenTTS Lite)");
        assert_eq!(addon_voice_name("(x)", "K"), "(x) (K)");
    }

    fn manifest(extra: serde_json::Value) -> serde_json::Value {
        let mut base = serde_json::json!({
            "schema": 1, "id": "kittentts-2", "version": "2.0.0", "name": "KittenTTS 2",
            "protocol": {"min": 1, "max": 1}, "exe": "bin/helper.exe"
        });
        for (k, v) in extra.as_object().unwrap() {
            if v.is_null() {
                base.as_object_mut().unwrap().remove(k);
            } else {
                base[k] = v.clone();
            }
        }
        base
    }

    fn parse(v: serde_json::Value) -> Result<AddonManifest, String> {
        parse_manifest(v.to_string().as_bytes())
    }

    #[test]
    fn a_minimal_manifest_gets_the_defaults() {
        let m = parse(manifest(serde_json::json!({}))).unwrap();
        assert_eq!(m.id, "kittentts-2");
        assert_eq!(m.version.as_str(), "2.0.0");
        assert_eq!(m.short_name, "KittenTTS 2");
        assert_eq!(m.protocol, (1, 1));
        assert_eq!(m.timeouts, Timeouts::default());
        assert_eq!(m.idle_exit_sec, 300);
        assert_eq!(m.voices, None);
        assert!(m.args.is_empty() && m.env.is_empty() && m.notices.is_empty() && m.voice_aliases.is_empty());
    }

    #[test]
    fn a_full_manifest_parses() {
        let m = parse(manifest(serde_json::json!({
            "short_name": "Kitten 2", "description": "Expressive offline voices.", "credit": "Powered by KittenML",
            "args": ["--models", "models"], "env": {"OMP_NUM_THREADS": "4"},
            "timeouts": {"start_ms": 1, "load_ms": 900000, "idle_ms": 60000, "synth_max_ms": 99999999},
            "idle_exit_sec": 5, "notices": ["NOTICE.txt", "licenses/KittenML.txt"],
            "voices": [{"key": "bella", "name": " Bella ", "gender": "female", "language": "en-US", "accent": "American",
                        "description": "Warm and clear.", "extra": 1}],
            "voice_aliases": {"bela": "bella"}, "unknown_field": true
        })))
        .unwrap();
        assert_eq!(m.short_name, "Kitten 2");
        assert_eq!(m.args, ["--models", "models"]);
        assert_eq!(m.env, [("OMP_NUM_THREADS".to_string(), "4".to_string())]);
        // Clamped into range.
        assert_eq!(m.timeouts, Timeouts { start_ms: 5_000, load_ms: 600_000, idle_ms: 60_000, synth_max_ms: 1_800_000 });
        assert_eq!(m.idle_exit_sec, 30);
        let voices = m.voices.unwrap();
        assert_eq!(voices[0].name, "Bella");
        assert_eq!(voices[0].gender, "female");
        assert_eq!(m.voice_aliases.get("bela").map(String::as_str), Some("bella"));
        let never = parse(manifest(serde_json::json!({"idle_exit_sec": 0}))).unwrap();
        assert_eq!(never.idle_exit_sec, 0);
    }

    #[test]
    fn bad_manifests_are_rejected() {
        let voice = |key: &str| serde_json::json!({"key": key, "name": "V"});
        for bad in [
            serde_json::json!({"schema": 2}),
            serde_json::json!({"id": "Kitten"}),
            serde_json::json!({"id": null}),
            serde_json::json!({"version": "2.0.0-beta"}),
            serde_json::json!({"name": ""}),
            serde_json::json!({"protocol": {"min": 2, "max": 3}}),
            serde_json::json!({"protocol": {"min": 1, "max": 0}}),
            serde_json::json!({"exe": "../helper.exe"}),
            serde_json::json!({"exe": "/usr/bin/helper"}),
            serde_json::json!({"exe": "bin\\helper.exe"}),
            serde_json::json!({"exe": "C:/helper.exe"}),
            serde_json::json!({"env": {"PATH": "x"}}),
            serde_json::json!({"env": {"LD_PRELOAD": "x"}}),
            serde_json::json!({"env": {"DYLD_LIBRARY_PATH": "x"}}),
            serde_json::json!({"env": {"VOX_ADDON_ID": "x"}}),
            serde_json::json!({"env": {"lower": "x"}}),
            serde_json::json!({"env": {"OK": 1}}),
            serde_json::json!({"args": ["a", 1]}),
            serde_json::json!({"notices": ["../NOTICE"]}),
            serde_json::json!({"voices": [voice("bella"), voice("bella")]}),
            serde_json::json!({"voices": [voice("a:b")]}),
            serde_json::json!({"voices": [{"key": "bella", "name": ""}]}),
            serde_json::json!({"voices": {"key": "bella"}}),
            serde_json::json!({"voice_aliases": {"old": "bad key"}}),
        ] {
            assert!(parse(manifest(bad.clone())).is_err(), "{bad}");
        }
        assert!(parse_manifest(b"not json").is_err());
        assert!(parse_manifest(b"[]").is_err());
        let too_many: Vec<_> = (0..1001).map(|i| voice(&format!("v{i}"))).collect();
        assert!(parse(manifest(serde_json::json!({"voices": too_many}))).is_err());
    }

    #[test]
    fn safe_relative_paths() {
        for ok in ["helper", "bin/helper.exe", "a/b/c/d/e/f/g/h", "licenses/KittenML Community Licence.txt", "espeak-ng-data/en_dict"] {
            assert!(is_safe_relative_path(ok), "{ok}");
        }
        for bad in [
            "", "/abs", "../x", "a/../b", "./a", "a/./b", "a//b", "a/", "C:x", "a:b", "a\\b", "con", "CON.txt", "aux.tar.gz",
            "lpt1", "nul ", "a.", "a ", "a/b/c/d/e/f/g/h/i", "a\u{1}b", "a*b", "a?b", &"x".repeat(101), &format!("{}/{}", "x".repeat(100), "y".repeat(100)),
        ] {
            assert!(!is_safe_relative_path(bad), "{bad:?}");
        }
    }

    #[test]
    fn versions_compare_numerically() {
        let v = |s: &str| Version::parse(s).unwrap();
        assert!(v("2.0.0") > v("1.10.0"));
        assert!(v("1.10.0") > v("1.9.9"));
        assert!(v("1.0") < v("1.0.1"));
        assert_eq!(v("1.0").cmp(&v("1.0")), std::cmp::Ordering::Equal);
        for bad in ["", "1.", ".1", "1..2", "1.2.3.4.5", "v1", "1.0-beta", "1 .0", "1234567890"] {
            assert!(Version::parse(bad).is_none(), "{bad}");
        }
        assert_eq!(v("2.0.0").to_string(), "2.0.0");
    }
}
