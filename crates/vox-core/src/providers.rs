//! Speech providers besides ElevenLabs.
//!
//! A voice's provider is encoded in its id, so `settings.voiceId`, history records and cache manifests keep a single
//! string field (and stay readable by the Electron build). ElevenLabs ids are left as they are; other providers' voices
//! are `"<prefix>:<rest>"`, where `rest` is whatever that provider needs to address the voice:
//!
//! | Provider | Id | `rest` |
//! |---|---|---|
//! | Amazon Polly | `polly:neural:Joanna` | `<engine>:<VoiceId>` |
//! | Azure AI Speech | `azure:en-US-AvaMultilingualNeural` | voice short name |
//! | OpenAI | `openai:coral` | voice name |
//! | Google Cloud | `google:en-US-Chirp3-HD-Achernar` | voice name |
//! | Windows | `windows:<token id>` | the OneCore voice id |

use serde::{Deserialize, Serialize};

/// The speech service a voice belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Provider {
    ElevenLabs,
    Polly,
    Azure,
    OpenAi,
    Google,
    Windows,
}

impl Provider {
    /// Every provider other than ElevenLabs, in the order the Voice providers page lists them.
    pub const EXTRA: [Provider; 5] = [Provider::Windows, Provider::Polly, Provider::Azure, Provider::OpenAi, Provider::Google];

    /// The name shown in the UI (and used as the voice category on the Voices page).
    pub fn display_name(self) -> &'static str {
        match self {
            Provider::ElevenLabs => "ElevenLabs",
            Provider::Polly => "Amazon Polly",
            Provider::Azure => "Azure AI Speech",
            Provider::OpenAi => "OpenAI",
            Provider::Google => "Google Cloud",
            Provider::Windows => "Windows",
        }
    }

    /// The stable id used in voice ids, stored credentials and UI callbacks.
    pub fn id(self) -> &'static str {
        match self {
            Provider::ElevenLabs => "elevenlabs",
            Provider::Polly => "polly",
            Provider::Azure => "azure",
            Provider::OpenAi => "openai",
            Provider::Google => "google",
            Provider::Windows => "windows",
        }
    }

    pub fn from_id(id: &str) -> Option<Provider> {
        [Provider::ElevenLabs].into_iter().chain(Provider::EXTRA).find(|p| p.id() == id)
    }

    /// True when the provider needs credentials (everything but the built-in Windows voices).
    pub fn needs_credentials(self) -> bool {
        !matches!(self, Provider::Windows)
    }

    /// The setup form and help text for the Voice providers page.
    pub fn spec(self) -> ProviderSpec {
        match self {
            Provider::Polly => ProviderSpec {
                description: "Neural and generative voices from AWS. Create an access key for an IAM user with the AmazonPollyReadOnlyAccess permission.",
                key_label: "Access key ID",
                key_placeholder: "AKIA…",
                secret_label: "Secret access key",
                regions: POLLY_REGIONS,
                region_hint: "Most voices are available in US East (N. Virginia).",
                keys_url: "https://console.aws.amazon.com/iam/home#/security_credentials",
                exact_timing: true,
            },
            Provider::Azure => ProviderSpec {
                description: "Hundreds of neural voices in many languages. Create a Speech resource in the Azure portal and copy one of its keys and its region.",
                key_label: "Speech resource key",
                key_placeholder: "32-character key",
                secret_label: "",
                regions: AZURE_REGIONS,
                region_hint: "The region of your Speech resource (shown next to its keys).",
                keys_url: "https://portal.azure.com/#create/Microsoft.CognitiveServicesSpeechServices",
                exact_timing: false,
            },
            Provider::OpenAi => ProviderSpec {
                description: "Natural, expressive voices from OpenAI (gpt-4o-mini-tts). Paste an API key from your OpenAI account.",
                key_label: "API key",
                key_placeholder: "sk-…",
                secret_label: "",
                regions: &[],
                region_hint: "",
                keys_url: "https://platform.openai.com/api-keys",
                exact_timing: false,
            },
            Provider::Google => ProviderSpec {
                description: "Chirp 3 HD, Studio and Neural2 voices. Create an API key in a Google Cloud project with the Text-to-Speech API enabled.",
                key_label: "API key",
                key_placeholder: "AIza…",
                secret_label: "",
                regions: &[],
                region_hint: "",
                keys_url: "https://console.cloud.google.com/apis/credentials",
                exact_timing: false,
            },
            Provider::Windows => ProviderSpec {
                description: "The voices built into Windows. Free and offline; no setup needed. Add more under Windows Settings → Time & language → Speech.",
                key_label: "",
                key_placeholder: "",
                secret_label: "",
                regions: &[],
                region_hint: "",
                keys_url: "",
                exact_timing: true,
            },
            Provider::ElevenLabs => ProviderSpec {
                description: "",
                key_label: "API key",
                key_placeholder: "sk_…",
                secret_label: "",
                regions: &[],
                region_hint: "",
                keys_url: "https://elevenlabs.io/app/settings/api-keys",
                exact_timing: true,
            },
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Provider::ElevenLabs => "",
            Provider::Polly => "polly:",
            Provider::Azure => "azure:",
            Provider::OpenAi => "openai:",
            Provider::Google => "google:",
            Provider::Windows => "windows:",
        }
    }
}

/// What the Voice providers page shows for a provider's setup form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderSpec {
    pub description: &'static str,
    pub key_label: &'static str,
    pub key_placeholder: &'static str,
    /// Empty when the provider has no second secret field.
    pub secret_label: &'static str,
    /// `(id, display name)`; empty when there is no region.
    pub regions: &'static [(&'static str, &'static str)],
    pub region_hint: &'static str,
    pub keys_url: &'static str,
    /// Word timing comes from the service (otherwise it is spread evenly over the audio).
    pub exact_timing: bool,
}

/// The provider of `voice_id` (anything without a known prefix is an ElevenLabs id).
pub fn provider_of(voice_id: &str) -> Provider {
    Provider::EXTRA.into_iter().find(|p| voice_id.starts_with(p.prefix())).unwrap_or(Provider::ElevenLabs)
}

/// `(provider, rest)` for a non-ElevenLabs voice id with a non-empty rest.
pub fn split_voice_id(voice_id: &str) -> Option<(Provider, &str)> {
    let p = provider_of(voice_id);
    if p == Provider::ElevenLabs {
        return None;
    }
    voice_id.strip_prefix(p.prefix()).filter(|r| !r.is_empty()).map(|r| (p, r))
}

/// `"<prefix><rest>"`.
pub fn voice_id(provider: Provider, rest: &str) -> String {
    format!("{}{rest}", provider.prefix())
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
    voice_id(Provider::Polly, &format!("{engine}:{voice}"))
}

/// Parses the `rest` of a Polly voice id (`<engine>:<voice>`).
pub fn parse_polly_rest(rest: &str) -> Option<PollyVoiceRef> {
    let (engine, voice) = rest.split_once(':')?;
    if engine.is_empty() || voice.is_empty() {
        return None;
    }
    Some(PollyVoiceRef { engine: engine.to_string(), voice: voice.to_string() })
}

/// Parses a full Polly voice id; `None` when it is not one or a part is empty.
pub fn parse_polly_voice_id(voice_id: &str) -> Option<PollyVoiceRef> {
    match split_voice_id(voice_id) {
        Some((Provider::Polly, rest)) => parse_polly_rest(rest),
        _ => None,
    }
}

/// AWS regions where Amazon Polly is available.
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

/// Azure regions with the Speech service (the region id goes into `<region>.tts.speech.microsoft.com`).
pub const AZURE_REGIONS: &[(&str, &str)] = &[
    ("eastus", "East US"),
    ("eastus2", "East US 2"),
    ("westus", "West US"),
    ("westus2", "West US 2"),
    ("westus3", "West US 3"),
    ("centralus", "Central US"),
    ("northcentralus", "North Central US"),
    ("southcentralus", "South Central US"),
    ("canadacentral", "Canada Central"),
    ("brazilsouth", "Brazil South"),
    ("westeurope", "West Europe"),
    ("northeurope", "North Europe"),
    ("uksouth", "UK South"),
    ("francecentral", "France Central"),
    ("germanywestcentral", "Germany West Central"),
    ("switzerlandnorth", "Switzerland North"),
    ("swedencentral", "Sweden Central"),
    ("norwayeast", "Norway East"),
    ("southafricanorth", "South Africa North"),
    ("uaenorth", "UAE North"),
    ("centralindia", "Central India"),
    ("southeastasia", "Southeast Asia"),
    ("eastasia", "East Asia"),
    ("japaneast", "Japan East"),
    ("koreacentral", "Korea Central"),
    ("australiaeast", "Australia East"),
];

/// Credentials for one provider (stored DPAPI-encrypted by vox-store; never in settings.json, never logged).
/// `key`: Polly access key id, Azure resource key, OpenAI / Google API key. `secret`: Polly secret access key (empty
/// elsewhere). `region`: Polly / Azure region id (empty elsewhere). The serde aliases read the Polly-only format
/// saved by the first version of this feature.
#[derive(Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ProviderCredentials {
    #[serde(alias = "accessKeyId")]
    pub key: String,
    #[serde(alias = "secretAccessKey", default)]
    pub secret: String,
    #[serde(default)]
    pub region: String,
}

impl std::fmt::Debug for ProviderCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderCredentials").field("key", &mask_key(&self.key)).field("region", &self.region).finish_non_exhaustive()
    }
}

/// `AKIA…WXYZ`: the first four and last four characters of a key (enough to recognise it). Short keys show `…`.
pub fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return "…".to_string();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

impl ProviderCredentials {
    /// Trims every field and checks what `provider` requires; `Err` carries a user-facing message.
    pub fn validated(self, provider: Provider) -> Result<ProviderCredentials, String> {
        let spec = provider.spec();
        let c = ProviderCredentials { key: self.key.trim().to_string(), secret: self.secret.trim().to_string(), region: self.region.trim().to_string() };
        if c.key.is_empty() {
            return Err(format!("Enter your {}.", lower_first(spec.key_label)));
        }
        if !spec.secret_label.is_empty() && c.secret.is_empty() {
            return Err(format!("Enter your {}.", lower_first(spec.secret_label)));
        }
        if !spec.regions.is_empty() && !spec.regions.iter().any(|(r, _)| *r == c.region) {
            return Err("Choose a region.".into());
        }
        Ok(c)
    }

    /// The one-line summary the UI shows for a connected provider ("US East (N. Virginia) · key AKIA…WXYZ").
    pub fn summary(&self, provider: Provider) -> String {
        let spec = provider.spec();
        let region = spec.regions.iter().find(|(r, _)| *r == self.region).map(|(_, n)| *n);
        match region {
            Some(name) => format!("{name} \u{00B7} key {}", mask_key(&self.key)),
            None => format!("Key {}", mask_key(&self.key)),
        }
    }
}

fn lower_first(s: &str) -> String {
    // "API key" keeps its capitals; other labels read as a phrase ("access key ID").
    if s.starts_with("API") {
        return s.to_string();
    }
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_lowercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// What the UI may know about a connected provider (no secret).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderStatus {
    pub provider: Provider,
    /// [`ProviderCredentials::summary`], or for Windows the voice count.
    pub summary: String,
    /// The region id, when the provider has one (pre-selects the region picker).
    pub region: String,
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
        assert_eq!(split_voice_id("azure:en-US-AvaNeural"), Some((Provider::Azure, "en-US-AvaNeural")));
        assert_eq!(split_voice_id("openai:coral"), Some((Provider::OpenAi, "coral")));
        assert_eq!(split_voice_id("openai:"), None);
        assert_eq!(voice_id(Provider::Google, "en-US-Neural2-F"), "google:en-US-Neural2-F");
        for p in Provider::EXTRA {
            assert_eq!(Provider::from_id(p.id()), Some(p));
        }
    }

    #[test]
    fn credentials_are_trimmed_validated_and_masked() {
        let c = ProviderCredentials { key: " AKIAABCDEFGHWXYZ ".into(), secret: " TopSecretValue ".into(), region: "eu-west-1".into() };
        let v = c.validated(Provider::Polly).unwrap();
        assert_eq!(v.key, "AKIAABCDEFGHWXYZ");
        assert_eq!(v.summary(Provider::Polly), "Europe (Ireland) \u{00B7} key AKIA…WXYZ");
        let debug = format!("{v:?}");
        assert!(!debug.contains("TopSecretValue") && !debug.contains("AKIAABCDEFGHWXYZ"), "{debug}");
        let bad = ProviderCredentials { key: "a".into(), secret: "b".into(), region: "mars-1".into() };
        assert_eq!(bad.validated(Provider::Polly).unwrap_err(), "Choose a region.");
        let no_secret = ProviderCredentials { key: "k".into(), ..Default::default() };
        assert_eq!(no_secret.clone().validated(Provider::Polly).unwrap_err(), "Enter your secret access key.");
        assert!(no_secret.clone().validated(Provider::OpenAi).is_ok());
        assert_eq!(ProviderCredentials::default().validated(Provider::OpenAi).unwrap_err(), "Enter your API key.");
        assert_eq!(no_secret.validated(Provider::Azure).unwrap_err(), "Choose a region.");
    }

    #[test]
    fn the_first_polly_format_still_loads() {
        let old: ProviderCredentials = serde_json::from_str(r#"{"accessKeyId":"AKIA1","secretAccessKey":"s","region":"us-east-1"}"#).unwrap();
        assert_eq!((old.key.as_str(), old.secret.as_str(), old.region.as_str()), ("AKIA1", "s", "us-east-1"));
    }
}
