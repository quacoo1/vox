//! Speech providers besides ElevenLabs.
//!
//! A voice's provider is encoded in its id so that `settings.voiceId`, history records and cache manifests keep a
//! single string field (and stay readable by the Electron build): ElevenLabs ids are left as they are, and other
//! providers' voices are `"<provider>:<engine>:<voice>"`, for example `polly:neural:Joanna`.

use serde::{Deserialize, Serialize};

/// The speech service a voice belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    ElevenLabs,
    Polly,
}

impl Provider {
    /// The name shown in the UI (and used as the voice category on the Voices page).
    pub fn display_name(self) -> &'static str {
        match self {
            Provider::ElevenLabs => "ElevenLabs",
            Provider::Polly => "Amazon Polly",
        }
    }
}

const POLLY_PREFIX: &str = "polly:";

/// The provider of `voice_id` (anything without a known prefix is an ElevenLabs id).
pub fn provider_of(voice_id: &str) -> Provider {
    if voice_id.starts_with(POLLY_PREFIX) {
        Provider::Polly
    } else {
        Provider::ElevenLabs
    }
}

/// A Polly voice reference parsed from a `polly:<engine>:<VoiceId>` id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollyVoiceRef {
    /// Polly `Engine`: `standard`, `neural`, `long-form` or `generative`.
    pub engine: String,
    /// Polly `VoiceId`, e.g. `Joanna`.
    pub voice: String,
}

/// `polly:<engine>:<voice>`.
pub fn polly_voice_id(engine: &str, voice: &str) -> String {
    format!("{POLLY_PREFIX}{engine}:{voice}")
}

/// Parses a Polly voice id; `None` when it is not one or a part is empty.
pub fn parse_polly_voice_id(voice_id: &str) -> Option<PollyVoiceRef> {
    let rest = voice_id.strip_prefix(POLLY_PREFIX)?;
    let (engine, voice) = rest.split_once(':')?;
    if engine.is_empty() || voice.is_empty() {
        return None;
    }
    Some(PollyVoiceRef { engine: engine.to_string(), voice: voice.to_string() })
}

/// AWS regions where Amazon Polly is available, in the order the region picker shows them.
pub const POLLY_REGIONS: &[(&str, &str)] = &[
    ("us-east-1", "US East (N. Virginia)"),
    ("us-east-2", "US East (Ohio)"),
    ("us-west-1", "US West (N. California)"),
    ("us-west-2", "US West (Oregon)"),
    ("ca-central-1", "Canada (Central)"),
    ("sa-east-1", "South America (São Paulo)"),
    ("eu-west-1", "Europe (Ireland)"),
    ("eu-west-2", "Europe (London)"),
    ("eu-west-3", "Europe (Paris)"),
    ("eu-central-1", "Europe (Frankfurt)"),
    ("eu-north-1", "Europe (Stockholm)"),
    ("af-south-1", "Africa (Cape Town)"),
    ("me-south-1", "Middle East (Bahrain)"),
    ("ap-south-1", "Asia Pacific (Mumbai)"),
    ("ap-southeast-1", "Asia Pacific (Singapore)"),
    ("ap-southeast-2", "Asia Pacific (Sydney)"),
    ("ap-northeast-1", "Asia Pacific (Tokyo)"),
    ("ap-northeast-2", "Asia Pacific (Seoul)"),
    ("ap-northeast-3", "Asia Pacific (Osaka)"),
    ("ap-east-1", "Asia Pacific (Hong Kong)"),
];

/// Amazon Polly credentials (stored DPAPI-encrypted by vox-store; never in settings.json, never logged).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PollyCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub region: String,
}

impl std::fmt::Debug for PollyCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PollyCredentials")
            .field("access_key_id", &mask_access_key(&self.access_key_id))
            .field("region", &self.region)
            .finish_non_exhaustive()
    }
}

/// `AKIA…WXYZ`: the first four and last four characters of an access key id (enough to recognise it).
pub fn mask_access_key(id: &str) -> String {
    let chars: Vec<char> = id.chars().collect();
    if chars.len() <= 8 {
        return "…".to_string();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

/// What the UI may know about the Polly connection (no secret).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PollyStatus {
    pub region: String,
    /// [`mask_access_key`] of the access key id.
    pub access_key_hint: String,
}

impl PollyCredentials {
    /// Trims every field; `Err` with a user-facing message when one is missing or the region is unknown.
    pub fn validated(self) -> Result<PollyCredentials, String> {
        let c = PollyCredentials {
            access_key_id: self.access_key_id.trim().to_string(),
            secret_access_key: self.secret_access_key.trim().to_string(),
            region: self.region.trim().to_string(),
        };
        if c.access_key_id.is_empty() {
            return Err("Enter your AWS access key ID.".into());
        }
        if c.secret_access_key.is_empty() {
            return Err("Enter your AWS secret access key.".into());
        }
        if !POLLY_REGIONS.iter().any(|(r, _)| *r == c.region) {
            return Err("Choose an AWS region.".into());
        }
        Ok(c)
    }

    pub fn status(&self) -> PollyStatus {
        PollyStatus { region: self.region.clone(), access_key_hint: mask_access_key(&self.access_key_id) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_ids_round_trip_and_route() {
        let id = polly_voice_id("neural", "Joanna");
        assert_eq!(id, "polly:neural:Joanna");
        assert_eq!(provider_of(&id), Provider::Polly);
        assert_eq!(parse_polly_voice_id(&id), Some(PollyVoiceRef { engine: "neural".into(), voice: "Joanna".into() }));
        assert_eq!(provider_of("21m00Tcm4TlvDq8ikWAM"), Provider::ElevenLabs);
        assert_eq!(parse_polly_voice_id("polly:neural:"), None);
        assert_eq!(parse_polly_voice_id("polly:Joanna"), None);
    }

    #[test]
    fn credentials_are_trimmed_validated_and_masked() {
        let c = PollyCredentials { access_key_id: " AKIAABCDEFGHWXYZ ".into(), secret_access_key: " TopSecretValue ".into(), region: "eu-west-1".into() };
        let v = c.validated().unwrap();
        assert_eq!(v.access_key_id, "AKIAABCDEFGHWXYZ");
        assert_eq!(v.status().access_key_hint, "AKIA…WXYZ");
        let debug = format!("{v:?}");
        assert!(!debug.contains("TopSecretValue") && !debug.contains("AKIAABCDEFGHWXYZ"), "{debug}");
        let bad = PollyCredentials { access_key_id: "a".into(), secret_access_key: "b".into(), region: "mars-1".into() };
        assert_eq!(bad.validated().unwrap_err(), "Choose an AWS region.");
    }
}
