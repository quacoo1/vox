//! Offline voice add-ons: voice ids (docs/addons-design.md "Identity and routing").
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
}
